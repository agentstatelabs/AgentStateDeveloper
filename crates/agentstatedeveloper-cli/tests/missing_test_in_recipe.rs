//! Plan J t-002: when `asd prepare-change` finds `test_gap = true`,
//! the `safe_change_recipe.manually_validate` array must include a
//! `kind: "missing_test"` item so an agent reading only the recipe
//! sees the gap without cross-referencing the top-level field.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Position, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn seed_engine_no_tests(db_path: &Path) {
    // One impl symbol, no tests anywhere → test_gap will fire.
    let engine = Engine::open_sqlite(db_path).expect("open sqlite");
    let sym = Symbol {
        symbol_id: "sym_calc".into(),
        symbol_fp: "fp".into(),
        qname: "billing.calc.discount".into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: "src/billing/calc.py".into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 10, col: 0 },
        signature: Some("def discount(total, code)".into()),
        doc: Some("Apply discount based on code".into()),
    };
    AsgIndexStore::from_engine(&engine)
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();
}

fn run_prepare_change(db_path: &Path, description: &str) -> serde_json::Value {
    // `asd prepare-change` takes the description as a positional arg.
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db_path)
        .arg("prepare-change")
        .arg(description)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "prepare-change failed:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("non-JSON stdout: {e}\n{}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn test_gap_surfaces_missing_test_item_in_recipe() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_no_tests(&db);

    let v = run_prepare_change(&db, "discount calculation");

    // Sanity: test_gap is true at the top level.
    assert_eq!(
        v["test_gap"].as_bool(),
        Some(true),
        "fixture should produce test_gap=true; got: {v:#?}"
    );

    let mv = v["safe_change_recipe"]["manually_validate"]
        .as_array()
        .expect("manually_validate array present");
    let missing_test_items: Vec<&serde_json::Value> = mv
        .iter()
        .filter(|item| item["kind"].as_str() == Some("missing_test"))
        .collect();
    assert_eq!(
        missing_test_items.len(),
        1,
        "expected exactly one missing_test item; got recipe: {mv:#?}"
    );
    let item = missing_test_items[0];
    assert!(
        item["step"]
            .as_str()
            .map(|s| s.contains("No test currently exercises"))
            .unwrap_or(false),
        "step text must mention the gap; got: {item:#?}"
    );
}
