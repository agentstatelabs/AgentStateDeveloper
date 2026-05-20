//! Plan E t-013: end-to-end integration tests for the Plan C
//! semantic-layer features. These exercise the full Engine + index +
//! ledger + ranking chain through real SQLite (not in-memory) so the
//! storage round-trip is real, not just per-function unit logic.
//!
//! Each scenario seeds a small workspace, walks the same code paths a
//! production caller would, and asserts the observable behavior. Aim:
//! catch regressions in the seams Plan C added (decisions-as-constraints,
//! recipes, asd map) without spawning a subprocess.

use std::path::PathBuf;

use agentstatedeveloper_core::{
    candidates::{apply_constraint_penalties, apply_task_bias},
    recipes::{classify_test_migration, ActionKind},
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind,
};

fn fresh_engine() -> (tempfile::TempDir, PathBuf, Engine) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db_path).expect("open sqlite engine");
    (tmp, db_path, engine)
}

fn put_sym(engine: &Engine, qname: &str, file: &str) -> String {
    let index = AsgIndexStore::from_engine(engine);
    let sym = Symbol {
        symbol_id: format!("sym_{}", qname.replace('.', "_")),
        symbol_fp: "fp".into(),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!("def {qname}()")),
        doc: None,
    };
    index
        .put_symbol(&engine.ref_name, &sym, "test")
        .expect("put_symbol");
    sym.symbol_id
}

fn append_role_entry(
    engine: &Engine,
    sym_id: &str,
    kind: LedgerKind,
    role: Option<&str>,
    body: Option<&str>,
) {
    let ledger = AsgLedgerStore::from_engine(engine);
    let mut entry = LedgerEntry::new(
        sym_id,
        kind,
        "integration-seed",
        Author { kind: AuthorKind::Agent, id: "test".into() },
    );
    entry.role = role.map(str::to_string);
    entry.body = body.map(str::to_string);
    ledger
        .append_entry(&engine.ref_name, &entry, "test")
        .expect("append_entry");
}

// --- Scenario 1: stale-api Constraint demotes its symbol --------------------
// Proves Plan C t-003 wired end-to-end through real SQLite — the Constraint
// goes into the ledger cache via append_entry, the SQL fast-path reads it
// out, apply_constraint_penalties suppresses the symbol.

#[test]
fn stale_api_constraint_actually_demotes_in_real_engine() {
    let (_tmp, _db, engine) = fresh_engine();
    let legacy_id = put_sym(&engine, "app.legacy.api", "src/legacy.py");
    let _modern_id = put_sym(&engine, "app.modern.api", "src/modern.py");

    append_role_entry(&engine, &legacy_id, LedgerKind::Constraint, Some("stale-api"), None);

    let index = AsgIndexStore::from_engine(&engine);
    let mut scored = vec![
        (10.0_f64, "app.legacy.api".to_string()),
        (10.0_f64, "app.modern.api".to_string()),
    ];
    let suppressed = apply_constraint_penalties(&engine, &index, &mut scored);

    assert_eq!(suppressed, 1, "stale-api Constraint must demote exactly one symbol");
    let legacy = scored.iter().find(|(_, q)| q == "app.legacy.api").unwrap();
    let modern = scored.iter().find(|(_, q)| q == "app.modern.api").unwrap();
    assert_eq!(legacy.0, f64::NEG_INFINITY);
    assert_eq!(modern.0, 10.0);
}

// --- Scenario 2: scope-restricted Constraint -------------------------------
// Plan E t-008 layered on top of t-003: scope: [glob] in entry body restricts
// suppression. End-to-end the SQL fast path should parse the body JSON and
// only suppress when the candidate's file matches.

#[test]
fn scoped_constraint_only_demotes_in_scope_files_end_to_end() {
    let (_tmp, _db, engine) = fresh_engine();
    let legacy_id = put_sym(&engine, "app.legacy.api", "src/legacy/api.py");
    let _modern_id = put_sym(&engine, "app.modern.api", "src/modern/api.py");

    // Constraint scoped to src/legacy/** only.
    append_role_entry(
        &engine,
        &legacy_id,
        LedgerKind::Constraint,
        Some("stale-api"),
        Some(r#"{"scope":["src/legacy/**"]}"#),
    );

    let index = AsgIndexStore::from_engine(&engine);
    let mut scored = vec![
        (10.0_f64, "app.legacy.api".to_string()),
        (10.0_f64, "app.modern.api".to_string()),
    ];
    apply_constraint_penalties(&engine, &index, &mut scored);

    let legacy = scored.iter().find(|(_, q)| q == "app.legacy.api").unwrap();
    let modern = scored.iter().find(|(_, q)| q == "app.modern.api").unwrap();
    assert_eq!(legacy.0, f64::NEG_INFINITY, "in-scope symbol must be suppressed");
    assert_eq!(modern.0, 10.0, "out-of-scope symbol must be untouched");
}

// --- Scenario 3: classify-test-migration recipe end-to-end ----------------
// Plan C t-004 + role tags. Mapping entry → KeepAsCovered; stale-api
// Constraint → Delete; default → Review.

#[test]
fn classify_test_migration_recipe_routes_by_ledger_evidence() {
    let (_tmp, _db, engine) = fresh_engine();

    // Three test-tier symbols, each with different ledger evidence.
    let covered_id = put_sym(&engine, "pkg.tests.covered", "tests/covered_test.py");
    append_role_entry(&engine, &covered_id, LedgerKind::Mapping, None, None);

    let stale_id = put_sym(&engine, "pkg.tests.stale", "tests/stale_test.py");
    append_role_entry(&engine, &stale_id, LedgerKind::Constraint, Some("stale-api"), None);

    let _bare_id = put_sym(&engine, "pkg.tests.bare", "tests/bare_test.py");

    let index = AsgIndexStore::from_engine(&engine);
    let recipe = classify_test_migration(
        &engine,
        &index,
        &[
            "pkg.tests.covered".into(),
            "pkg.tests.stale".into(),
            "pkg.tests.bare".into(),
        ],
        "migrate stale tests",
    );

    assert_eq!(recipe.intent, "classify-test-migration");
    assert_eq!(recipe.actions.len(), 3, "all three test symbols should land in the plan");
    let action_for = |qn: &str| {
        recipe.actions.iter().find(|a| a.qname == qn).expect("action for qname").kind
    };
    assert_eq!(action_for("pkg.tests.covered"), ActionKind::KeepAsCovered);
    assert_eq!(action_for("pkg.tests.stale"), ActionKind::Delete);
    assert_eq!(action_for("pkg.tests.bare"), ActionKind::Review);
}

// --- Scenario 4: task-bias boost on in-scope files ------------------------
// Plan C t-006 wired through real SQLite — apply_task_bias should bulk-fetch
// the candidate files via the new files_for_qnames helper and boost
// in-scope candidates.

#[test]
fn task_bias_boosts_in_scope_candidates_end_to_end() {
    let (_tmp, _db, engine) = fresh_engine();
    put_sym(&engine, "app.audio.engine", "Packages/AudioEngine/src/lib.py");
    put_sym(&engine, "app.ui.button", "Packages/UI/src/button.py");

    let index = AsgIndexStore::from_engine(&engine);
    let mut scored = vec![
        (5.0_f64, "app.audio.engine".to_string()),
        (5.0_f64, "app.ui.button".to_string()),
    ];
    let n = apply_task_bias(
        &engine,
        &index,
        &mut scored,
        &["Packages/AudioEngine/**".to_string()],
        1.0,
    );

    assert_eq!(n, 1);
    let audio = scored.iter().find(|(_, q)| q == "app.audio.engine").unwrap();
    let ui = scored.iter().find(|(_, q)| q == "app.ui.button").unwrap();
    assert_eq!(audio.0, 6.0, "in-scope audio engine should be boosted +1");
    assert_eq!(ui.0, 5.0, "out-of-scope UI symbol should be unchanged");
}

// --- Scenario 5: conclusions export → import round-trip -------------------
// Plan B t-004/t-005 round-trip. A Mapping entry seeded into the ledger,
// exported to JSONL, imported into a fresh engine, must reappear with the
// same entry_id and kind.

#[test]
fn conclusions_export_import_round_trips_through_real_engine() {
    use agentstatedeveloper_core::conclusions_export::{export_all, import_all};

    let (tmp1, _db1, engine1) = fresh_engine();
    let covered_id = put_sym(&engine1, "pkg.tests.covered", "tests/covered_test.py");
    append_role_entry(
        &engine1,
        &covered_id,
        LedgerKind::Mapping,
        None,
        Some(r#"{"from_qname":"pkg.tests.covered","to_qname":"pkg.tests.new"}"#),
    );

    let out_dir = tmp1.path().join("conclusions");
    let exports = export_all(&engine1, &out_dir).expect("export");
    assert!(
        exports.iter().any(|(stem, n, _)| *stem == "mappings" && *n == 1),
        "expected exactly one mapping in exports; got {exports:?}"
    );

    // Fresh engine, re-seed the symbol (import looks up by qname), then import.
    let (_tmp2, _db2, engine2) = fresh_engine();
    put_sym(&engine2, "pkg.tests.covered", "tests/covered_test.py");
    let results = import_all(&engine2, &out_dir, "test").expect("import");
    let imported: usize = results.iter().map(|r| r.imported).sum();
    assert_eq!(imported, 1, "the single mapping must round-trip");

    // Verify the imported entry survived round-trip with the right kind.
    let ledger2 = AsgLedgerStore::from_engine(&engine2);
    let entries = ledger2
        .list_entries(&engine2.ref_name, "sym_pkg_tests_covered")
        .expect("list_entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, LedgerKind::Mapping);
}
