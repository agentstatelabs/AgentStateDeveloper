//! Ledger integrity counts and restore (cache vs store vs sidecar).
//!
//! A lost entry is simulated the way the field loss looked: present in
//! `asd_ledger_cache`, absent from the ASG ledger tree. Deleting it from ASG
//! directly leaves the cache row behind, exactly as a discarded commit did.

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;

use agentstatedeveloper_core::{
    AsgLedgerStore, Author, AuthorKind, Engine, LedgerEntry, LedgerKind, LedgerStore,
    ledger_counts, missing_ledger_entries, paths, restore_missing_ledger_entries, sync_to_dir,
};

const KINDS: [LedgerKind; 4] = [
    LedgerKind::Decision,
    LedgerKind::Invariant,
    LedgerKind::Proof,
    LedgerKind::ValidationScenario,
];

fn author() -> Author {
    Author {
        kind: AuthorKind::Human,
        id: "tester".into(),
    }
}

/// Engine on a fresh SQLite store with `n` multi-kind entries over a few
/// symbols, synced to the sidecar so every count starts equal.
fn seeded(n: usize) -> (tempfile::TempDir, Engine, Vec<LedgerEntry>) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open_sqlite(&dir.path().join(".asd-state.db")).unwrap();
    let ledger = AsgLedgerStore::from_engine(&engine);
    let entries: Vec<_> = (0..n)
        .map(|i| {
            let e = LedgerEntry::new(
                format!("sym_{}", i % 3),
                KINDS[i % KINDS.len()],
                format!("entry {i}"),
                author(),
            );
            ledger.append_entry(&engine.ref_name, &e, "tester").unwrap();
            e
        })
        .collect();
    sync_to_dir(&engine.repo, &engine.ref_name, dir.path()).unwrap();
    (dir, engine, entries)
}

/// Drop an entry from ASG only; its cache row survives.
fn lose_from_store(engine: &Engine, e: &LedgerEntry) {
    for path in [
        paths::ledger_entry_path(&e.symbol_id, &e.entry_id),
        paths::ledger_entry_index_path(&e.entry_id),
    ] {
        engine
            .repo
            .delete(
                &engine.ref_name,
                &path,
                CommitOptions::new("test", IntentCategory::Refine, "simulate loss"),
            )
            .unwrap();
    }
}

fn counts(dir: &tempfile::TempDir, engine: &Engine) -> agentstatedeveloper_core::LedgerCounts {
    ledger_counts(
        &engine.repo,
        &engine.ref_name,
        engine.fts.as_ref(),
        Some(dir.path()),
    )
    .unwrap()
}

#[test]
fn consistent_store_reports_equal_counts_and_no_warning() {
    let (dir, engine, _) = seeded(8);
    let c = counts(&dir, &engine);
    assert_eq!((c.cache, c.asg, c.sidecar), (Some(8), 8, Some(8)));
    assert!(c.is_consistent());
    assert_eq!(c.warning(), None);
}

#[test]
fn entry_lost_from_the_store_is_counted_and_warned() {
    let (dir, engine, entries) = seeded(8);
    lose_from_store(&engine, &entries[2]);
    lose_from_store(&engine, &entries[5]);

    let c = counts(&dir, &engine);
    assert_eq!(c.cache, Some(8));
    assert_eq!(c.asg, 6);
    assert_eq!(c.missing_from_asg, 2);
    let w = c.warning().expect("a lost entry must produce a warning");
    assert!(
        w.contains("2 ledger entries") && w.contains("asd repair --fix"),
        "{w}"
    );

    // Re-syncing cannot fix it — the store no longer has the entries to export.
    sync_to_dir(&engine.repo, &engine.ref_name, dir.path()).unwrap();
    assert_eq!(counts(&dir, &engine).missing_from_asg, 2);
}

#[test]
fn restore_puts_lost_entries_back_and_sync_exports_them() {
    let (dir, engine, entries) = seeded(8);
    lose_from_store(&engine, &entries[1]);
    lose_from_store(&engine, &entries[6]);

    let (missing, unparseable) = missing_ledger_entries(&engine).unwrap();
    let mut ids: Vec<_> = missing.iter().map(|e| e.entry_id.clone()).collect();
    ids.sort();
    let mut want = vec![entries[1].entry_id.clone(), entries[6].entry_id.clone()];
    want.sort();
    assert_eq!(ids, want);
    assert!(unparseable.is_empty());

    let report = restore_missing_ledger_entries(&engine, "tester").unwrap();
    assert_eq!((report.missing, report.restored), (2, 2));
    assert!(report.failed.is_empty());

    // Restored byte-for-byte from the cache, reverse index included.
    for e in [&entries[1], &entries[6]] {
        let back: LedgerEntry = serde_json::from_value(
            engine
                .repo
                .get_json(
                    &engine.ref_name,
                    &paths::ledger_entry_path(&e.symbol_id, &e.entry_id),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(e).unwrap()
        );
        assert_eq!(
            engine
                .repo
                .get_json(
                    &engine.ref_name,
                    &paths::ledger_entry_index_path(&e.entry_id)
                )
                .unwrap(),
            serde_json::json!(e.symbol_id)
        );
    }

    sync_to_dir(&engine.repo, &engine.ref_name, dir.path()).unwrap();
    let c = counts(&dir, &engine);
    assert_eq!((c.cache, c.asg, c.sidecar), (Some(8), 8, Some(8)));
    assert!(c.is_consistent());

    // Idempotent: nothing left to restore.
    let again = restore_missing_ledger_entries(&engine, "tester").unwrap();
    assert_eq!((again.missing, again.restored), (0, 0));
}

/// A restore is one commit, however many entries it brings back. It was two
/// per entry, each storing a fresh copy of the ledger and ledger-idx maps:
/// restoring 1,684 entries grew a 1.5 GB store to 5.5 GB.
#[test]
fn a_restore_lands_as_one_commit() {
    let (_dir, engine, entries) = seeded(10);
    for e in &entries[..6] {
        lose_from_store(&engine, e);
    }
    let before = engine.repo.log(&engine.ref_name, 10_000).unwrap().len();

    let report = restore_missing_ledger_entries(&engine, "tester").unwrap();
    assert_eq!((report.missing, report.restored), (6, 6));

    let log = engine.repo.log(&engine.ref_name, 10_000).unwrap();
    assert_eq!(log.len() - before, 1, "one commit for the whole restore");
    assert!(
        log[0]
            .intent
            .description
            .starts_with("restore 6 ledger entries"),
        "{}",
        log[0].intent.description
    );
    let (missing, _) = missing_ledger_entries(&engine).unwrap();
    assert!(missing.is_empty(), "{} still missing", missing.len());
}

/// `ledger rebind` moves an entry to a new symbol and its cache row follows.
/// Matching by entry id means the move is never mistaken for a loss.
#[test]
fn rebound_entry_is_not_reported_missing() {
    let (dir, engine, entries) = seeded(4);
    let e = &entries[0];
    let mut moved = e.clone();
    moved.symbol_id = "sym_renamed".into();
    let ledger = AsgLedgerStore::from_engine(&engine);
    ledger
        .append_entry(&engine.ref_name, &moved, "tester")
        .unwrap();
    engine
        .repo
        .delete(
            &engine.ref_name,
            &paths::ledger_entry_path(&e.symbol_id, &e.entry_id),
            CommitOptions::new("test", IntentCategory::Refine, "rebind"),
        )
        .unwrap();

    let c = counts(&dir, &engine);
    assert_eq!(c.missing_from_asg, 0);
    assert!(missing_ledger_entries(&engine).unwrap().0.is_empty());
}

#[test]
fn entries_not_yet_synced_are_reported_as_sidecar_lag() {
    let (dir, engine, _) = seeded(3);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let e = LedgerEntry::new("sym_new", LedgerKind::Decision, "after sync", author());
    ledger.append_entry(&engine.ref_name, &e, "tester").unwrap();

    let c = counts(&dir, &engine);
    assert_eq!(c.missing_from_asg, 0);
    assert_eq!(c.missing_from_sidecar, 1);
    assert!(c.warning().unwrap().contains("asd sync"));

    sync_to_dir(&engine.repo, &engine.ref_name, dir.path()).unwrap();
    assert!(counts(&dir, &engine).is_consistent());
}
