//! `asd index` removes index entries for symbols that no longer exist, carries
//! ledger entries along when a symbol only moved, and keeps anything that
//! still has knowledge attached. The by-qname index used to only ever grow:
//! SessionDrift-ios held 12,044 entries for 9,244 live symbols, 81% of the
//! stale ones line-number churn (`load:412` → `load:418`) with 490 ledger
//! entries left behind on the old ids.

use std::path::Path;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgLedgerStore, Author, AuthorKind, EffectStore, Engine, IndexSummary,
    LedgerEntry, LedgerKind, LedgerStore, run_index,
};
use serde_json::Value;

fn index(engine: &Engine, path: &Path, db: &Path) -> IndexSummary {
    run_index(
        &engine.repo,
        &engine.ref_name,
        path,
        "test",
        &agentstatedeveloper_adapters::default_adapters(),
        None,
        None,
        None,
        Some(db),
    )
    .unwrap()
}

/// qname → symbol_id, as the index holds them now.
fn by_qname(engine: &Engine) -> Vec<(String, String)> {
    let tree = engine
        .repo
        .get_json(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap();
    let mut out: Vec<(String, String)> = tree
        .as_object()
        .unwrap()
        .iter()
        .map(|(q, s)| (q.clone(), s["symbol_id"].as_str().unwrap().to_string()))
        .collect();
    out.sort();
    out
}

fn id_of(engine: &Engine, qname: &str) -> String {
    by_qname(engine)
        .into_iter()
        .find(|(q, _)| q == qname)
        .unwrap_or_else(|| panic!("{qname} not indexed: {:?}", by_qname(engine)))
        .1
}

fn qnames(engine: &Engine) -> Vec<String> {
    by_qname(engine).into_iter().map(|(q, _)| q).collect()
}

fn note(engine: &Engine, symbol_id: &str, summary: &str) -> LedgerEntry {
    let entry = LedgerEntry::new(
        symbol_id,
        LedgerKind::Decision,
        summary,
        Author {
            kind: AuthorKind::Human,
            id: "dev".into(),
        },
    );
    AsgLedgerStore::from_engine(engine)
        .append_entry(&engine.ref_name, &entry, "dev")
        .unwrap();
    entry
}

/// Read a symbol's effects through the store, which caches them — so a test
/// can show the cache row is gone afterwards, not merely never written.
fn cache_effects(engine: &Engine, db: &Path, symbol_id: &str) {
    AsgEffectStore::from_engine(engine)
        .get_effects(&engine.ref_name, symbol_id)
        .unwrap()
        .expect("indexed symbols have effects");
    assert!(
        effects_cached(db, symbol_id),
        "precondition: effects cached"
    );
}

/// The symbol the ledger cache files an entry under, read directly: a read
/// through the store would repair a stale row on the way.
fn cached_ledger_symbol(db: &Path, entry_id: &str) -> String {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(
        "SELECT symbol_id FROM asd_ledger_cache WHERE entry_id = ?1",
        [entry_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn effects_cached(db: &Path, symbol_id: &str) -> bool {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(
        "SELECT COUNT(*) FROM asd_effects_cache WHERE symbol_id = ?1",
        [symbol_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

/// Two same-named functions are told apart by line (`m.f:1`, `m.f:5`). Two
/// lines added above them used to mint `m.f:3`/`m.f:7` beside the old
/// entries, with the ledger entry stranded on `m.f:5`.
#[test]
fn a_line_shift_carries_ledger_entries_to_the_moved_symbol() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    let src = dir.join("m.py");
    std::fs::write(
        &src,
        "def f():\n    return 1\n\n\ndef f():\n    return 2\n\n\ndef g():\n    return 3\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    assert_eq!(qnames(&engine), vec!["m.f:1", "m.f:5", "m.g"]);
    let old_id = id_of(&engine, "m.f:5");
    let entry = note(&engine, &old_id, "returns 2 on purpose");
    cache_effects(&engine, &db, &old_id);
    assert_eq!(cached_ledger_symbol(&db, &entry.entry_id), old_id);

    std::fs::write(
        &src,
        format!("\n\n{}", std::fs::read_to_string(&src).unwrap()),
    )
    .unwrap();
    let summary = index(&engine, dir, &db);

    assert_eq!(
        qnames(&engine),
        vec!["m.f:3", "m.f:7", "m.g"],
        "stale entries left behind"
    );
    assert_eq!(
        (
            summary.stale_rebound,
            summary.ledger_entries_rebound,
            summary.stale_pruned
        ),
        (2, 1, 0)
    );
    let new_id = id_of(&engine, "m.f:7");
    // The caches first: a read through the store repairs a stale row.
    assert_eq!(
        cached_ledger_symbol(&db, &entry.entry_id),
        new_id,
        "the ledger cache still files the entry under the old id"
    );
    assert!(
        !effects_cached(&db, &old_id),
        "the old id's effects stayed cached"
    );
    let ledger = AsgLedgerStore::from_engine(&engine);
    let moved = ledger.list_entries(&engine.ref_name, &new_id).unwrap();
    assert_eq!(
        moved
            .iter()
            .map(|e| e.entry_id.as_str())
            .collect::<Vec<_>>(),
        vec![entry.entry_id.as_str()],
        "the ledger entry did not follow its symbol"
    );
    assert!(
        ledger
            .list_entries(&engine.ref_name, &old_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        engine
            .repo
            .get_json(
                &engine.ref_name,
                &format!("/asd/v1/ledger-idx/{}", entry.entry_id)
            )
            .unwrap(),
        Value::String(new_id)
    );
}

#[test]
fn a_deleted_symbol_is_pruned_unless_its_ledger_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    let src = dir.join("m.py");
    std::fs::write(
        &src,
        "def a():\n    return 1\n\n\ndef b():\n    return 2\n\n\ndef c():\n    return 3\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let a_id = id_of(&engine, "m.a");
    note(&engine, &id_of(&engine, "m.b"), "b is load-bearing");
    cache_effects(&engine, &db, &a_id);

    std::fs::write(&src, "def c():\n    return 3\n").unwrap();
    let summary = index(&engine, dir, &db);

    assert_eq!(qnames(&engine), vec!["m.b", "m.c"]);
    assert_eq!((summary.stale_pruned, summary.stale_kept), (1, 1));
    assert!(
        engine
            .repo
            .get_json(&engine.ref_name, &format!("/asd/v1/effects/{a_id}"))
            .is_err(),
        "the pruned symbol's effects stayed"
    );
    assert!(!effects_cached(&db, &a_id));
}

/// A partial run records paths relative to its own root, so it must not
/// read another file as deleted; a later run over the whole project does.
#[test]
fn only_a_whole_project_run_prunes_deleted_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("top.py"), "def t():\n    return 1\n").unwrap();
    std::fs::write(dir.join("sub/inner.py"), "def i():\n    return 2\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    assert!(qnames(&engine).contains(&"top.t".to_string()));

    std::fs::remove_file(dir.join("top.py")).unwrap();
    index(&engine, &dir.join("sub"), &db);
    assert!(
        qnames(&engine).contains(&"top.t".to_string()),
        "a partial run pruned a file outside it"
    );

    let summary = index(&engine, dir, &db);
    assert!(!qnames(&engine).contains(&"top.t".to_string()));
    assert!(summary.stale_pruned >= 1);
}
