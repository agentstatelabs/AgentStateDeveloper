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
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use serde_json::{Value, json};

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

fn put_json(engine: &Engine, path: &str, value: &Value) {
    engine
        .repo
        .set_json(
            &engine.ref_name,
            path,
            value,
            CommitOptions::new("t", IntentCategory::Refine, format!("seed {path}")),
        )
        .unwrap();
}

fn has_effects(engine: &Engine, symbol_id: &str) -> bool {
    engine
        .repo
        .get_json(&engine.ref_name, &format!("/asd/v1/effects/{symbol_id}"))
        .is_ok()
}

/// `/asd/v1/code` keys (`clean_file/symbol_fp`) across languages.
fn code_keys(engine: &Engine) -> Vec<String> {
    let tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/code")
        .unwrap();
    let mut out: Vec<String> = tree
        .as_object()
        .unwrap()
        .values()
        .flat_map(|by_key| by_key.as_object().unwrap().keys().cloned())
        .collect();
    out.sort();
    out
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

    assert!(
        code_keys(&engine).iter().any(|k| k.starts_with("top.py/")),
        "a partial run pruned a code entry outside it"
    );

    let summary = index(&engine, dir, &db);
    assert!(!qnames(&engine).contains(&"top.t".to_string()));
    assert!(summary.stale_pruned >= 1);
    assert!(!code_keys(&engine).iter().any(|k| k.starts_with("top.py/")));
}

/// Swift anchors a qname at the SPM target (the path after `Sources/`), so
/// moving the package keeps the qname while the id — which hashes the whole
/// path — changes. The new entry used to overwrite the old one and strand
/// its ledger entries and effects on an id nothing indexed: ThreadWeaver-ios
/// held 54,685 such effects records.
#[test]
fn moving_a_package_carries_its_ledger_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    std::fs::create_dir_all(dir.join("Old/Sources/App")).unwrap();
    std::fs::write(
        dir.join("Old/Sources/App/payments.swift"),
        "func chargeCard(amount: Double) -> Bool {\n    print(\"charging\")\n    return amount > 0\n}\n\nfunc refund(amount: Double) -> Bool { return amount > 0 }\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let old_id = id_of(&engine, "App.payments.chargeCard");
    let entry = note(&engine, &old_id, "charges before it ships");
    cache_effects(&engine, &db, &old_id);

    std::fs::rename(dir.join("Old"), dir.join("New")).unwrap();
    let summary = index(&engine, dir, &db);

    let new_id = id_of(&engine, "App.payments.chargeCard");
    assert_ne!(new_id, old_id, "precondition: the id hashes the path");
    assert_eq!(
        (
            summary.stale_rebound,
            summary.ledger_entries_rebound,
            summary.stale_pruned
        ),
        (2, 1, 0)
    );
    assert_eq!(
        cached_ledger_symbol(&db, &entry.entry_id),
        new_id,
        "the ledger cache still files the entry under the old id"
    );
    assert!(!effects_cached(&db, &old_id));
    let ledger = AsgLedgerStore::from_engine(&engine);
    assert_eq!(
        ledger
            .list_entries(&engine.ref_name, &new_id)
            .unwrap()
            .iter()
            .map(|e| e.entry_id.as_str())
            .collect::<Vec<_>>(),
        vec![entry.entry_id.as_str()],
        "the ledger entry stayed on the old id"
    );
    assert!(
        ledger
            .list_entries(&engine.ref_name, &old_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        !has_effects(&engine, &old_id),
        "the old id's effects stayed"
    );
    assert!(has_effects(&engine, &new_id));
    assert!(
        code_keys(&engine).iter().all(|k| k.starts_with("New/")),
        "code entries left under the old folder: {:?}",
        code_keys(&engine)
    );
}

/// Effects records nothing in the index refers to — what earlier versions
/// left behind — go on a run over the whole project. A partial run cannot
/// tell, and a record holding runtime evidence is kept regardless.
#[test]
fn a_whole_project_run_drops_orphaned_effects_but_keeps_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("top.py"), "def t():\n    return 1\n").unwrap();
    std::fs::write(dir.join("sub/inner.py"), "def i():\n    return 2\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let template = engine
        .repo
        .get_json(
            &engine.ref_name,
            &format!("/asd/v1/effects/{}", id_of(&engine, "top.t")),
        )
        .unwrap();
    let orphan = |id: &str, runtime: Option<Value>| {
        let mut decl = template.clone();
        decl["symbol_id"] = json!(id);
        if let Some(r) = runtime {
            decl["runtime"] = r;
        }
        put_json(&engine, &format!("/asd/v1/effects/{id}"), &decl);
    };
    orphan("sym_orphan_plain", None);
    orphan(
        "sym_orphan_traced",
        Some(json!({
            "confirmations": 3,
            "contradictions": 0,
            "prior": 0.5,
            "last_observed_at": "2026-10-01T00:00:00Z"
        })),
    );
    cache_effects(&engine, &db, "sym_orphan_plain");

    let partial = index(&engine, &dir.join("sub"), &db);
    assert_eq!(partial.orphaned_effects_pruned, 0);
    assert!(has_effects(&engine, "sym_orphan_plain"));

    let whole = index(&engine, dir, &db);
    assert_eq!(whole.orphaned_effects_pruned, 1);
    assert!(!has_effects(&engine, "sym_orphan_plain"));
    assert!(!effects_cached(&db, "sym_orphan_plain"));
    assert!(
        has_effects(&engine, "sym_orphan_traced"),
        "runtime evidence was dropped"
    );
    assert!(has_effects(&engine, &id_of(&engine, "top.t")));
}

/// The code tree is keyed by body fingerprint, so every edit used to leave
/// the previous body's entry behind.
#[test]
fn an_edited_body_leaves_no_code_entry_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    let src = dir.join("m.py");
    std::fs::write(&src, "def f():\n    return 1\n\n\ndef g():\n    return 3\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let before = code_keys(&engine);
    assert_eq!(before.len(), 2);

    std::fs::write(&src, "def f():\n    return 2\n\n\ndef g():\n    return 3\n").unwrap();
    let summary = index(&engine, dir, &db);

    let after = code_keys(&engine);
    assert_eq!(after.len(), 2, "stale code entries: {after:?}");
    assert_ne!(after, before, "precondition: f's fingerprint changed");
    assert_eq!(summary.code_entries_pruned, 1);
}

/// A symbol that lost a cross-file qname collision has a code entry but no
/// by-qname entry, so only the run that produced it can vouch for it: a
/// partial run elsewhere must leave its code entry alone.
#[test]
fn a_partial_run_keeps_code_entries_outside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    for (pkg, body) in [("A", "1"), ("B", "2")] {
        std::fs::create_dir_all(dir.join(format!("{pkg}/Sources/App"))).unwrap();
        std::fs::write(
            dir.join(format!("{pkg}/Sources/App/x.swift")),
            format!("func f() -> Int {{ return {body} }}\n"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(dir.join("tools")).unwrap();
    std::fs::write(dir.join("tools/t.py"), "def t():\n    return 1\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let swift = |keys: Vec<String>| -> Vec<String> {
        keys.into_iter().filter(|k| k.contains(".swift/")).collect()
    };
    let before = swift(code_keys(&engine));
    assert_eq!(before.len(), 2, "precondition: both sides of the collision");

    let summary = index(&engine, &dir.join("tools"), &db);
    assert_eq!(swift(code_keys(&engine)), before);
    assert_eq!(summary.code_entries_pruned, 0);
}

/// Pruning can empty the code tree; the emptied tree must still be written.
#[test]
fn deleting_every_file_empties_the_code_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    std::fs::write(dir.join("m.py"), "def f():\n    return 1\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    assert_eq!(code_keys(&engine).len(), 1);

    std::fs::remove_file(dir.join("m.py")).unwrap();
    let summary = index(&engine, dir, &db);
    assert_eq!(summary.code_entries_pruned, 1);
    assert!(code_keys(&engine).is_empty(), "{:?}", code_keys(&engine));
}
