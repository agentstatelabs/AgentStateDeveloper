//! A ledger entry lives under exactly one symbol, through `asd sync`, `asd
//! hydrate` and `asd repair`.
//!
//! SessionDrift-ios ran hooks that `asd hydrate` on every checkout. Hydrate
//! re-filed each entry under the symbol `.asd/v1/` last saw it on, so the 270
//! entries `asd index` had moved to new symbol ids came back under their old
//! ones too: 272 entries stored twice, the ledger cache pointing at the dead
//! copy for 144 of them — hiding those entries from their live symbol —
//! and two commits per entry, ~24,700 per checkout.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentstatedeveloper_core::{
    AsgLedgerStore, Author, AuthorKind, Engine, LedgerEntry, LedgerKind, LedgerStore, Rebind,
    hydrate_from_dir_with_cache, run_index, scan_asg, sync_to_dir,
};
use serde_json::Value;

fn index(engine: &Engine, dir: &Path, db: &Path) {
    run_index(
        &engine.repo,
        &engine.ref_name,
        dir,
        "test",
        &agentstatedeveloper_adapters::default_adapters(),
        None,
        None,
        None,
        Some(db),
    )
    .unwrap();
}

fn asd(dir: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_asd"))
        .args(args)
        .arg("--db")
        .arg(db)
        .current_dir(dir)
        .env("ASD_REGISTRY", dir.join("registry.toml"))
        .output()
        .unwrap()
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

fn id_of(engine: &Engine, qname: &str) -> String {
    engine
        .repo
        .get_json(&engine.ref_name, &format!("/asd/v1/index/by-qname/{qname}"))
        .unwrap()["symbol_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn head(engine: &Engine) -> String {
    engine.repo.log(&engine.ref_name, 1).unwrap()[0].id.to_hex()
}

/// Descriptions of the commits made since `since`.
fn commits_since(engine: &Engine, since: &str) -> Vec<String> {
    engine
        .repo
        .log(&engine.ref_name, 500)
        .unwrap()
        .into_iter()
        .take_while(|c| c.id.to_hex() != since)
        .map(|c| c.intent.description)
        .collect()
}

/// Every symbol `entry_id` is stored under, read from the store itself.
fn stored_at(engine: &Engine, entry_id: &str) -> Vec<String> {
    let mut at: Vec<String> = match engine.repo.get_tree(&engine.ref_name, "/asd/v1/ledger") {
        Ok(Value::Object(by_symbol)) => by_symbol
            .into_iter()
            .filter(|(_, entries)| entries.get(entry_id).is_some())
            .map(|(symbol, _)| symbol)
            .collect(),
        _ => Vec::new(),
    };
    at.sort();
    at
}

fn stored(engine: &Engine, symbol_id: &str, entry_id: &str) -> LedgerEntry {
    serde_json::from_value(
        engine
            .repo
            .get_json(
                &engine.ref_name,
                &format!("/asd/v1/ledger/{symbol_id}/{entry_id}"),
            )
            .unwrap(),
    )
    .unwrap()
}

/// The symbol the ledger cache files an entry under, read directly: a read
/// through the store would repair a stale row on the way.
fn cached_symbol(db: &Path, entry_id: &str) -> Option<String> {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT symbol_id FROM asd_ledger_cache WHERE entry_id = ?1",
            [entry_id],
            |r| r.get(0),
        )
        .ok()
}

fn sidecar_file(dir: &Path, symbol_id: &str, entry_id: &str) -> PathBuf {
    dir.join(format!(".asd/v1/ledger/{symbol_id}/{entry_id}.json"))
}

/// Two same-named functions, told apart by line; a ledger entry on the
/// second; `.asd/v1` synced; then two lines added above them, so the entry
/// moves to the symbol's new id — while the sidecar still files it under
/// the old one, as SessionDrift's did.
struct Moved {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    db: PathBuf,
    old_id: String,
    new_id: String,
    entry: LedgerEntry,
}

fn moved_entry() -> Moved {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    let src = dir.join("m.py");
    std::fs::write(&src, "def f():\n    return 1\n\n\ndef f():\n    return 2\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, &dir, &db);
    let old_id = id_of(&engine, "m.f:5");
    let entry = note(&engine, &old_id, "returns 2 on purpose");
    sync_to_dir(&engine.repo, &engine.ref_name, &dir).unwrap();
    std::fs::write(
        &src,
        format!("\n\n{}", std::fs::read_to_string(&src).unwrap()),
    )
    .unwrap();
    index(&engine, &dir, &db);
    let new_id = id_of(&engine, "m.f:7");
    assert_eq!(stored_at(&engine, &entry.entry_id), vec![new_id.clone()]);
    assert!(
        sidecar_file(&dir, &old_id, &entry.entry_id).exists(),
        "precondition: the sidecar still files the entry under the old id"
    );
    Moved {
        _tmp: tmp,
        dir,
        db,
        old_id,
        new_id,
        entry,
    }
}

#[test]
fn hydrate_leaves_a_moved_entry_where_the_store_moved_it() {
    let m = moved_entry();
    let engine = Engine::open_sqlite(&m.db).unwrap();
    let before = head(&engine);

    let summary = hydrate_from_dir_with_cache(
        &engine.repo,
        engine.fts.as_ref(),
        &engine.ref_name,
        &m.dir,
        "test",
    )
    .unwrap();

    assert_eq!(
        stored_at(&engine, &m.entry.entry_id),
        vec![m.new_id.clone()],
        "hydrate filed the entry under its old id again"
    );
    assert_eq!(summary.ledger_entries_loaded, 0);
    assert_eq!(summary.ledger_entries_skipped, 1);
    assert_eq!(cached_symbol(&m.db, &m.entry.entry_id), Some(m.new_id));
    assert!(
        !scan_asg(&engine.repo, &engine.ref_name)
            .unwrap()
            .iter()
            .any(|i| i.kind == "ledger_duplicate")
    );
    let ledger_commits: Vec<String> = commits_since(&engine, &before)
        .into_iter()
        .filter(|d| d.starts_with("ledger") || d.contains("ledger entr"))
        .collect();
    assert!(ledger_commits.is_empty(), "{ledger_commits:?}");
}

#[test]
fn sync_removes_the_sidecar_copy_of_a_moved_entry() {
    let m = moved_entry();
    let out = asd(&m.dir, &m.db, &["sync"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["moved_entries_removed"], 1);
    assert!(!sidecar_file(&m.dir, &m.old_id, &m.entry.entry_id).exists());
    assert!(sidecar_file(&m.dir, &m.new_id, &m.entry.entry_id).exists());
}

/// A store rebuilt from `.asd/v1` — the case hydrate exists for — gets every
/// entry in one commit, cached.
#[test]
fn hydrating_an_empty_store_files_each_entry_once_in_one_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef b():\n    return 2\n",
    )
    .unwrap();
    let (a, entries) = {
        let engine = Engine::open_sqlite(&db).unwrap();
        index(&engine, &dir, &db);
        let a = id_of(&engine, "m.a");
        let entries: Vec<LedgerEntry> = (0..30)
            .map(|n| note(&engine, &a, &format!("note {n}")))
            .collect();
        sync_to_dir(&engine.repo, &engine.ref_name, &dir).unwrap();
        (a, entries)
    };
    // A pre-v1.4.7 sidecar can hold an entry under a dead id as well.
    let dup = &entries[0];
    let mut dead_copy = dup.clone();
    dead_copy.symbol_id = "sym_dead".into();
    std::fs::create_dir_all(dir.join(".asd/v1/ledger/sym_dead")).unwrap();
    std::fs::write(
        sidecar_file(&dir, "sym_dead", &dup.entry_id),
        serde_json::to_string(&dead_copy).unwrap(),
    )
    .unwrap();
    for f in [".asd-state.db", ".asd-state.db-wal", ".asd-state.db-shm"] {
        let _ = std::fs::remove_file(dir.join(f));
    }

    // Opening an empty store beside a sidecar hydrates it.
    let engine = Engine::open_sqlite(&db).unwrap();

    for e in &entries {
        assert_eq!(stored_at(&engine, &e.entry_id), vec![a.clone()]);
        assert_eq!(cached_symbol(&db, &e.entry_id).as_deref(), Some(a.as_str()));
    }
    let ledger_commits: Vec<String> = engine
        .repo
        .log(&engine.ref_name, 500)
        .unwrap()
        .into_iter()
        .map(|c| c.intent.description)
        .filter(|d| d.starts_with("ledger") || d.contains("ledger entr"))
        .collect();
    assert_eq!(ledger_commits.len(), 1, "{ledger_commits:?}");

    // Hydrating again finds everything in place.
    let settled = head(&engine);
    let again = hydrate_from_dir_with_cache(
        &engine.repo,
        engine.fts.as_ref(),
        &engine.ref_name,
        &dir,
        "t",
    )
    .unwrap();
    assert_eq!(
        (again.ledger_entries_loaded, again.ledger_entries_skipped),
        (0, 31)
    );
    assert!(
        !commits_since(&engine, &settled)
            .iter()
            .any(|d| d.starts_with("ledger") || d.contains("ledger entr")),
        "a settled hydrate rewrote the ledger"
    );
}

/// The sidecar copy of an entry the store already holds under the same
/// symbol replaces it only when it is a newer revision.
#[test]
fn hydrate_keeps_a_newer_stored_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(dir.join("m.py"), "def a():\n    return 1\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, &dir, &db);
    let a = id_of(&engine, "m.a");
    let entry = note(&engine, &a, "older in the sidecar");
    sync_to_dir(&engine.repo, &engine.ref_name, &dir).unwrap();
    let mut revised = entry.clone();
    revised.tags.push("approved-at:2099-01-01T00:00:00Z".into());
    AsgLedgerStore::from_engine(&engine)
        .append_entry(&engine.ref_name, &revised, "dev")
        .unwrap();

    hydrate_from_dir_with_cache(
        &engine.repo,
        engine.fts.as_ref(),
        &engine.ref_name,
        &dir,
        "t",
    )
    .unwrap();

    assert!(
        stored(&engine, &a, &entry.entry_id)
            .tags
            .iter()
            .any(|t| t.starts_with("approved-at:")),
        "hydrate reverted a newer stored revision"
    );
}

/// A rebind recorded in the sidecar moves entries still filed under the old
/// symbol — and does nothing on a store that already applied it.
#[test]
fn hydrate_replays_a_rebind_as_a_move_once() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef b():\n    return 2\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, &dir, &db);
    let b = engine
        .repo
        .get_json(&engine.ref_name, "/asd/v1/index/by-qname/m.b")
        .unwrap();
    let entry = note(&engine, "sym_old", "filed before the rename");
    sync_to_dir(&engine.repo, &engine.ref_name, &dir).unwrap();
    let rebind = Rebind {
        from_symbol_id: "sym_old".into(),
        to_symbol_id: b["symbol_id"].as_str().unwrap().into(),
        to_qname: "m.b".into(),
        at: chrono::Utc::now(),
        by: "dev".into(),
    };
    std::fs::write(
        dir.join(".asd/v1/rebinds/sym_old.json"),
        serde_json::to_string(&rebind).unwrap(),
    )
    .unwrap();
    let before = head(&engine);

    let first = hydrate_from_dir_with_cache(
        &engine.repo,
        engine.fts.as_ref(),
        &engine.ref_name,
        &dir,
        "t",
    )
    .unwrap();

    assert_eq!(first.rebinds_replayed, 1);
    assert_eq!(
        stored_at(&engine, &entry.entry_id),
        vec![rebind.to_symbol_id.clone()]
    );
    let moved = stored(&engine, &rebind.to_symbol_id, &entry.entry_id);
    assert!(moved.tags.iter().any(|t| t.starts_with("rebound-at:")));
    assert_eq!(
        cached_symbol(&db, &entry.entry_id).as_deref(),
        Some(rebind.to_symbol_id.as_str())
    );
    let replay_commits = commits_since(&engine, &before)
        .into_iter()
        .filter(|d| d.contains("rebind") || d.starts_with("ledger"))
        .count();
    assert_eq!(
        replay_commits, 1,
        "one commit for the ledger and the rebind"
    );

    // The sidecar still files the entry under the old symbol; the store has
    // applied the rebind. A second hydrate changes nothing.
    let settled = head(&engine);
    let second = hydrate_from_dir_with_cache(
        &engine.repo,
        engine.fts.as_ref(),
        &engine.ref_name,
        &dir,
        "t",
    )
    .unwrap();
    assert_eq!(second.rebinds_replayed, 0);
    assert_eq!(
        stored_at(&engine, &entry.entry_id),
        vec![rebind.to_symbol_id.clone()]
    );
    assert!(
        !commits_since(&engine, &settled)
            .iter()
            .any(|d| d.contains("ledger entr") || d.contains("rebind")),
        "the settled hydrate rewrote the ledger"
    );
}

/// What SessionDrift-ios was left with: entries stored under the live
/// symbol and again under a dead id, the cache pointing at the dead copy.
#[test]
fn repair_keeps_one_copy_of_a_duplicated_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef b():\n    return 2\n",
    )
    .unwrap();
    let (a, b, dup, revised) = {
        let engine = Engine::open_sqlite(&db).unwrap();
        index(&engine, &dir, &db);
        let (a, b) = (id_of(&engine, "m.a"), id_of(&engine, "m.b"));
        let ledger = AsgLedgerStore::from_engine(&engine);
        // Live copy, then a dead copy written last — the cache follows it.
        let dup = note(&engine, &a, "stored twice");
        let mut dead = dup.clone();
        dead.symbol_id = "sym_dead".into();
        ledger.append_entry(&engine.ref_name, &dead, "dev").unwrap();
        assert_eq!(
            cached_symbol(&db, &dup.entry_id).as_deref(),
            Some("sym_dead")
        );
        // Under two live symbols: the later revision wins, even though the
        // entry index and the cache name the other copy.
        let revised = note(&engine, &a, "revised under b");
        let mut on_b = revised.clone();
        on_b.symbol_id = b.clone();
        on_b.tags.push("approved-at:2099-01-01T00:00:00Z".into());
        ledger.append_entry(&engine.ref_name, &on_b, "dev").unwrap();
        ledger
            .append_entry(&engine.ref_name, &revised, "dev")
            .unwrap();
        assert_eq!(cached_symbol(&db, &revised.entry_id), Some(a.clone()));
        // What a pre-v1.4.7 hydrate left after replaying a rebind.
        engine
            .repo
            .set_json(
                &engine.ref_name,
                "/asd/v1/ledger/sym_emptied",
                &serde_json::json!({}),
                agentstategraph::CommitOptions::new(
                    "t",
                    agentstategraph_core::IntentCategory::Refine,
                    "empty node",
                ),
            )
            .unwrap();
        (a, b, dup, revised)
    };

    let scan = asd(&dir, &db, &["repair", "--json"]);
    let found: Value = serde_json::from_slice(&scan.stdout).unwrap();
    let dupes = found["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "ledger_duplicate")
        .count();
    assert_eq!(dupes, 2);
    let emptied: Vec<&str> = found["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/sym_emptied"))
        })
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(emptied, vec!["empty_ledger_node"]);

    let before = head(&Engine::open_sqlite(&db).unwrap());
    let fix = asd(&dir, &db, &["repair", "--fix", "--json"]);
    assert!(
        fix.status.success(),
        "{}",
        String::from_utf8_lossy(&fix.stderr)
    );
    let engine = Engine::open_sqlite(&db).unwrap();
    let log = engine.repo.log(&engine.ref_name, 2).unwrap();
    assert_eq!(log[1].id.to_hex(), before, "more than one commit");
    assert_eq!(stored_at(&engine, &dup.entry_id), vec![a.clone()]);
    assert_eq!(cached_symbol(&db, &dup.entry_id), Some(a.clone()));
    assert_eq!(
        engine
            .repo
            .get_json(
                &engine.ref_name,
                &format!("/asd/v1/ledger-idx/{}", dup.entry_id)
            )
            .unwrap(),
        Value::String(a)
    );
    assert_eq!(stored_at(&engine, &revised.entry_id), vec![b.clone()]);
    assert_eq!(cached_symbol(&db, &revised.entry_id), Some(b));
    let left: Vec<String> = scan_asg(&engine.repo, &engine.ref_name)
        .unwrap()
        .into_iter()
        .filter(|i| {
            ["ledger_duplicate", "orphaned_ledger", "empty_ledger_node"].contains(&i.kind.as_str())
        })
        .map(|i| i.kind)
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn status_warns_about_hooks_that_still_hydrate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(dir.join(".asd/hooks")).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(dir.join("m.py"), "def a():\n    return 1\n").unwrap();
    index(&Engine::open_sqlite(&db).unwrap(), &dir, &db);
    let warning = |dir: &Path| -> Option<String> {
        let out = asd(dir, &db, &["status", "--json"]);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["hooks_warning"].as_str().map(str::to_string)
    };

    std::fs::write(
        dir.join(".asd/hooks/post-checkout"),
        "#!/bin/sh\n# asd hydrate used to run here\nasd conclusions import\n",
    )
    .unwrap();
    assert_eq!(warning(&dir), None, "a comment is not a hydrate");

    std::fs::write(
        dir.join(".asd/hooks/post-checkout"),
        "#!/bin/sh\nset -e\nasd hydrate\nasd index .\n",
    )
    .unwrap();
    let w = warning(&dir).expect("old hooks not flagged");
    assert!(w.contains("asd init"), "{w}");
}
