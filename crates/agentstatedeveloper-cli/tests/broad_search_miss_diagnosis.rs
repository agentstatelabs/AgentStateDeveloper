//! Plan J t-004: when `asd search` returns fewer than the
//! broaden threshold AND a narrowing path/language filter is
//! active, the response surfaces a `broadened_search` block
//! listing what was dropped and which extra qnames appear once
//! the filter is cleared.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Position, SearchFtsDb, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn mk_sym(sym_id: &str, qname: &str, file: &str, language: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: "fp".into(),
        qname: qname.into(),
        language: language.into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!("def {}()", qname.rsplit('.').next().unwrap_or(qname))),
        doc: Some(format!("Function {qname}")),
    }
}

fn put_sym(
    engine: &Engine,
    sym_id: &str,
    qname: &str,
    file: &str,
    language: &str,
) -> Symbol {
    let sym = mk_sym(sym_id, qname, file, language);
    AsgIndexStore::from_engine(engine)
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();
    sym
}

fn rebuild_fts(db_path: &Path, symbols: &[Symbol]) {
    let fts = SearchFtsDb::open(db_path).expect("open fts");
    fts.rebuild(symbols).expect("rebuild fts");
}

fn seed_engine_two_langs(db_path: &Path) {
    let engine = Engine::open_sqlite(db_path).expect("open");
    // Two `discount` functions, one Python, one Swift. The query
    // "discount" hits both unfiltered; `--language python` narrows
    // to just the Python one (FTS SQL filter). Broadener drops
    // language → swift symbol surfaces in extra_hits.
    let py = put_sym(
        &engine,
        "sym_py_discount",
        "billing.calc.discount",
        "src/billing/calc.py",
        "python",
    );
    let sw = put_sym(
        &engine,
        "sym_sw_discount",
        "Catalog.Pricing.discount",
        "App/Sources/Catalog/Pricing.swift",
        "swift",
    );
    rebuild_fts(db_path, &[py, sw]);
}

fn run_search(db: &Path, args: &[&str]) -> serde_json::Value {
    // `--agent` flips the CLI to JSON output (default is human-readable).
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
        "search failed:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("non-JSON stdout: {e}\n{}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn broadened_search_null_when_no_narrowing_filter_active() {
    // Two hits, no narrowing flags. Hits < threshold (3) but no
    // path/language filter to drop → broadener stays null because
    // running it would just re-produce the same set.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_two_langs(&db);

    let v = run_search(&db, &["discount"]);
    let bs = &v["broadened_search"];
    assert!(
        bs.is_null(),
        "broadened_search must be null with no narrowing filter; got: {bs:#?}"
    );
}

#[test]
fn broadened_search_fires_when_language_filter_narrows_below_threshold() {
    // Two `discount` symbols across two languages. `--language python`
    // narrows the FTS query to one primary hit (below threshold 3)
    // AND a narrowing filter is active → broadener must fire and
    // surface the swift symbol when language is dropped.
    //
    // (Originally written against `--paths`, but `--paths` is
    // currently a no-op for results in `asd search` — only sets the
    // `scope_narrowed` advisory. That's a separate pre-existing gap.
    // `--language` runs as a SQL filter inside FTS so it actually
    // narrows the result set, which is what t-004 needs.)
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_two_langs(&db);

    let v = run_search(
        &db,
        &["discount", "--language", "python", "--limit", "20"],
    );
    let primary: Vec<&str> = v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r["qname"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let bs = &v["broadened_search"];
    assert!(
        bs.is_object(),
        "expected broadened_search object; primary={primary:?}; bs={bs:#?}"
    );
    assert_eq!(bs["triggered"].as_bool(), Some(true));
    let dropped = bs["dropped_filters"]
        .as_array()
        .expect("dropped_filters array");
    assert!(
        dropped
            .iter()
            .any(|d| d.as_str().map(|s| s.contains("language")).unwrap_or(false)),
        "must report language filter as dropped; got: {dropped:#?}"
    );
    let extra: Vec<&str> = bs["extra_hits"]
        .as_array()
        .expect("extra_hits array")
        .iter()
        .filter_map(|h| h["qname"].as_str())
        .collect();
    assert!(
        extra.iter().any(|q| *q == "Catalog.Pricing.discount"),
        "swift symbol must surface as an extra hit when language filter is dropped; got extra={extra:?}; primary={primary:?}; bs={bs:#?}"
    );
}

#[test]
fn broadened_search_null_when_primary_already_meets_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    // Seed enough hits to clear the broadening threshold (3).
    let engine = Engine::open_sqlite(&db).unwrap();
    let mut all: Vec<Symbol> = Vec::new();
    for i in 0..4 {
        let s = put_sym(
            &engine,
            &format!("sym_d_{i}"),
            &format!("billing.calc.discount_{i}"),
            &format!("src/billing/calc_{i}.py"),
            "python",
        );
        all.push(s);
    }
    rebuild_fts(&db, &all);

    let v = run_search(&db, &["discount", "--paths", "src/billing/**"]);
    let bs = &v["broadened_search"];
    assert!(
        bs.is_null(),
        "above threshold: broadened_search must be null; got: {bs:#?}"
    );
}
