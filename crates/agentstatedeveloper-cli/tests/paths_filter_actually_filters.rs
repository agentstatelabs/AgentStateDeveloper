//! Plan J t-017: `asd search --paths`, `--exclude-path`,
//! and `--exclude-lang` actually prune the result set.
//!
//! Pre-1.0.71 these flags populated `FtsFilters` and gated the
//! `scope_narrowed` advisory, but the actual pruning lived in
//! `core::candidates::apply_*_filter` — only `find_candidates`
//! (used by prepare_change / impact / context_for) ran them.
//! Search bypassed all of it, so `--paths "src/billing/**"`
//! returned hits from `src/catalog/...` too. t-004 (broad-search
//! miss diagnosis) discovered this gap; t-017 fixes it.

use std::path::Path;
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Position, SearchFtsDb, Symbol, SymbolKind,
};

fn asd_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn mk_sym(sym_id: &str, qname: &str, file: &str, language: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: format!("fp-{sym_id}"),
        qname: qname.into(),
        language: language.into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!(
            "def {}()",
            qname.rsplit('.').next().unwrap_or(qname)
        )),
        doc: Some(format!("Function {qname}")),
    }
}

fn seed(db: &Path) -> Vec<Symbol> {
    // Three symbols all matching the same query token, in three
    // distinct directories and two languages.
    let engine = Engine::open_sqlite(db).expect("open");
    let idx = AsgIndexStore::from_engine(&engine);
    let s_billing = mk_sym(
        "sym_bill",
        "billing.calc.discount",
        "src/billing/calc.py",
        "python",
    );
    let s_catalog = mk_sym(
        "sym_cat",
        "catalog.pricing.discount",
        "src/catalog/pricing.py",
        "python",
    );
    let s_swift = mk_sym(
        "sym_sw",
        "Catalog.Pricing.discount",
        "App/Sources/Pricing.swift",
        "swift",
    );
    idx.put_symbol(&engine.ref_name, &s_billing, "t").unwrap();
    idx.put_symbol(&engine.ref_name, &s_catalog, "t").unwrap();
    idx.put_symbol(&engine.ref_name, &s_swift, "t").unwrap();

    let all = vec![s_billing.clone(), s_catalog.clone(), s_swift.clone()];
    let fts = SearchFtsDb::open(db).unwrap();
    fts.rebuild(&all).expect("rebuild fts");
    all
}

fn run_search(db: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db)
        .arg("search")
        .arg("--agent")
        .args(args)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "search exited non-zero\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "non-JSON stdout: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn qnames_of(v: &serde_json::Value) -> Vec<String> {
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r["qname"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn baseline_no_filter_returns_all_three_hits() {
    // Sanity: without filters the query returns all 3. If this
    // test fails, the seed/FTS setup is broken — not a t-017
    // regression.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);
    let v = run_search(&db, &["discount", "--limit", "20"]);
    let qs = qnames_of(&v);
    assert_eq!(qs.len(), 3, "baseline expected 3 hits; got {qs:?}");
}

#[test]
fn paths_filter_drops_non_matching_files() {
    // The headline t-017 test: --paths "src/billing/**" must
    // KEEP only `billing.calc.discount`. Before 1.0.71 this
    // returned all 3.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);
    let v = run_search(
        &db,
        &["discount", "--paths", "src/billing/**", "--limit", "20"],
    );
    let qs = qnames_of(&v);
    assert_eq!(
        qs,
        vec!["billing.calc.discount"],
        "paths filter must drop catalog + swift; got {qs:?}"
    );
}

#[test]
fn exclude_path_drops_matching_files() {
    // Negative axis: --exclude-path "src/catalog/**" drops the
    // python catalog symbol but keeps billing and swift.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);
    let v = run_search(
        &db,
        &[
            "discount",
            "--exclude-path",
            "src/catalog/**",
            "--limit",
            "20",
        ],
    );
    let qs = qnames_of(&v);
    assert!(
        !qs.iter().any(|q| q == "catalog.pricing.discount"),
        "exclude-path must drop the catalog symbol; got {qs:?}"
    );
    assert!(
        qs.iter().any(|q| q == "billing.calc.discount"),
        "billing symbol must remain; got {qs:?}"
    );
}

#[test]
fn exclude_lang_drops_matching_language() {
    // Language exclude: --exclude-lang swift drops the swift
    // symbol; both python symbols remain.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);
    let v = run_search(
        &db,
        &["discount", "--exclude-lang", "swift", "--limit", "20"],
    );
    let qs = qnames_of(&v);
    assert!(
        !qs.iter().any(|q| q == "Catalog.Pricing.discount"),
        "exclude-lang swift must drop the swift symbol; got {qs:?}"
    );
    let py_count = qs
        .iter()
        .filter(|q| q.starts_with("billing.") || q.starts_with("catalog."))
        .count();
    assert_eq!(
        py_count, 2,
        "both python symbols must remain after excluding swift; got {qs:?}"
    );
}

#[test]
fn combined_filters_compose() {
    // Combine --paths AND --exclude-lang: positive narrows to
    // src/, exclude removes swift → 2 python symbols remain.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);
    let v = run_search(
        &db,
        &[
            "discount",
            "--paths",
            "src/**",
            "--exclude-lang",
            "swift",
            "--limit",
            "20",
        ],
    );
    let qs = qnames_of(&v);
    let mut got: Vec<String> = qs;
    got.sort();
    let mut expected = vec![
        "billing.calc.discount".to_string(),
        "catalog.pricing.discount".to_string(),
    ];
    expected.sort();
    assert_eq!(
        got, expected,
        "filter composition: src/** AND not-swift → 2 python hits; got {got:?}"
    );
}

#[test]
fn t004_broadener_extra_hits_now_recoverable_via_paths_drop() {
    // Plan J t-004 broadener fires when primary < threshold AND
    // a narrowing filter is set. Now that paths_filter actually
    // narrows, the broadener with paths_filter active should
    // recover the dropped hits in extra_hits.
    //
    // Setup: --paths "src/billing/**" gives 1 primary hit
    // (billing.calc.discount); broadener drops paths_filter and
    // surfaces the other 2 as extra_hits.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);
    let v = run_search(
        &db,
        &["discount", "--paths", "src/billing/**", "--limit", "20"],
    );
    let bs = &v["broadened_search"];
    assert!(
        bs.is_object(),
        "broadener should fire on 1-hit narrowed query; got: {bs:#?}"
    );
    assert_eq!(bs["triggered"].as_bool(), Some(true));
    let extra: Vec<String> = bs["extra_hits"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| h["qname"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let extra_set: std::collections::HashSet<&str> = extra.iter().map(|s| s.as_str()).collect();
    assert!(
        extra_set.contains("catalog.pricing.discount"),
        "broadener should recover the python catalog symbol; got: {extra:?}"
    );
    assert!(
        extra_set.contains("Catalog.Pricing.discount"),
        "broadener should recover the swift symbol; got: {extra:?}"
    );
}
