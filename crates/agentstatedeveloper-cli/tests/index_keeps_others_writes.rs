//! `asd index` and `asd conclusions import` must not revert what someone
//! else wrote: a declared effect, a write that lands while an index runs, an
//! entry revised in place since the last export. Each of these was lost
//! before 1.4.1 (data-loss audit, ASD S1-1..S1-3).

use agentstatedeveloper_core::{
    AsgEffectStore, Effect, EffectCategory, EffectDecl, EffectStore, Engine, run_index,
};
use serde_json::Value;

fn index(engine: &Engine, dir: &std::path::Path, db: &std::path::Path) {
    index_with(engine, dir, db, &|_| {});
}

/// Index `path`, calling `on_phase` as each post-parse phase starts.
fn index_with(
    engine: &Engine,
    path: &std::path::Path,
    db: &std::path::Path,
    on_phase: &dyn Fn(&str),
) {
    run_index(
        &engine.repo,
        &engine.ref_name,
        path,
        "test",
        &agentstatedeveloper_adapters::default_adapters(),
        None,
        None,
        Some(on_phase),
        Some(db),
    )
    .unwrap();
}

fn symbol_id(engine: &Engine, qname_suffix: &str) -> String {
    engine
        .repo
        .get_json(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap()
        .as_object()
        .unwrap()
        .iter()
        .find(|(q, _)| q.ends_with(qname_suffix))
        .and_then(|(_, s)| s.get("symbol_id").and_then(Value::as_str))
        .unwrap_or_else(|| panic!("{qname_suffix} indexed"))
        .to_string()
}

fn declared_by_hand(sym_id: &str, note: &str) -> EffectDecl {
    let mut decl = EffectDecl {
        symbol_id: sym_id.to_string(),
        declared: vec![Effect::new(EffectCategory::IoNetOut)],
        transitive: vec![],
        verification: None,
        confidence: Some(0.95),
        runtime: None,
        matched_policy: None,
    };
    decl.declared[0].note = Some(note.into());
    decl
}

fn declared_notes(engine: &Engine, sym_id: &str) -> Vec<Value> {
    let decl = engine
        .repo
        .get_json(&engine.ref_name, &format!("/asd/v1/effects/{sym_id}"))
        .unwrap();
    decl["declared"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| e["note"].clone())
        .collect()
}

#[test]
fn reindex_keeps_manually_declared_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let src = dir.join("client.py");
    std::fs::write(&src, "def fetch_rates():\n    return 1\n").unwrap();
    let db = dir.join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);

    let sym_id = symbol_id(&engine, "fetch_rates");

    // What `effect_declare` records: a human-declared network effect.
    AsgEffectStore::from_engine(&engine)
        .put_effects(
            &engine.ref_name,
            &sym_id,
            &declared_by_hand(&sym_id, "calls the rates API"),
            "human",
        )
        .unwrap();

    // The user edits the file and commits; the post-commit hook re-indexes.
    std::fs::write(&src, "def fetch_rates():\n    # retry once\n    return 1\n").unwrap();
    index(&engine, dir, &db);

    assert_eq!(
        declared_notes(&engine, &sym_id),
        vec![Value::from("calls the rates API")],
        "re-index replaced the declared effects"
    );
}

/// The index reads the stored symbols and effects before it parses and used
/// to write that copy back whole, reverting anything written meanwhile — to
/// any symbol, not just the ones it re-parsed.
#[test]
fn index_keeps_a_write_that_lands_while_it_parses() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("client.py"), "def fetch_rates():\n    return 1\n").unwrap();
    std::fs::write(dir.join("cache.py"), "def load_cache():\n    return 2\n").unwrap();
    let db = dir.join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let cache_id = symbol_id(&engine, "load_cache");

    // Re-index one file; an `effect_declare` on the other lands after the
    // parse, before the index commits.
    std::fs::write(dir.join("client.py"), "def fetch_rates():\n    return 3\n").unwrap();
    let effects = AsgEffectStore::from_engine(&engine);
    index_with(&engine, &dir.join("client.py"), &db, &|phase| {
        if phase.contains("committing symbols") {
            effects
                .put_effects(
                    &engine.ref_name,
                    &cache_id,
                    &declared_by_hand(&cache_id, "reads the disk cache"),
                    "human",
                )
                .unwrap();
        }
    });

    assert_eq!(
        declared_notes(&engine, &cache_id),
        vec![Value::from("reads the disk cache")],
        "the index reverted a declaration made while it ran"
    );
}

/// `conclusions import` (the post-merge/post-checkout hook) re-appended every
/// committed record unconditionally, reverting an entry updated in place
/// since the last export to the committed copy.
#[test]
fn conclusions_import_keeps_a_newer_in_place_update() {
    use agentstatedeveloper_core::{
        AsgLedgerStore, Author, AuthorKind, LedgerEntry, LedgerKind, LedgerStore,
        conclusions_export::{export_all, import_all},
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("client.py"), "def fetch_rates():\n    return 1\n").unwrap();
    let db = dir.join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let sym_id = symbol_id(&engine, "fetch_rates");

    let ledger = AsgLedgerStore::from_engine(&engine);
    let author = Author {
        kind: AuthorKind::Agent,
        id: "agent".into(),
    };
    let mut e = LedgerEntry::new(&sym_id, LedgerKind::Decision, "retry rates once", author);
    e.entry_id = "led_think_stable_id".into(); // deterministic id, updated in place
    e.confidence = Some(0.4);
    ledger.append_entry(&engine.ref_name, &e, "agent").unwrap();

    let out = dir.join(".asd/conclusions");
    export_all(&engine, &out).unwrap(); // committed state: confidence 0.4

    e.confidence = Some(0.9); // later in-place update, not yet committed
    e.summary = "retry rates twice".into();
    ledger.append_entry(&engine.ref_name, &e, "agent").unwrap();

    import_all(&engine, &out, "hook").unwrap(); // `git pull` / branch switch

    let now: LedgerEntry = serde_json::from_value(
        engine
            .repo
            .get_json(
                &engine.ref_name,
                &format!("/asd/v1/ledger/{sym_id}/led_think_stable_id"),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (now.confidence, now.summary.as_str()),
        (Some(0.9), "retry rates twice"),
        "conclusions import reverted an in-place update to the committed copy"
    );
}
