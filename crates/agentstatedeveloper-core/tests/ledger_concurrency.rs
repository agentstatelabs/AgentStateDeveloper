//! Ledger entries must survive concurrent writers.
//!
//! Field data (one store, v1.4.0): 1,684 of 12,085 ledger entries were
//! in `asd_ledger_cache` but gone from the ASG ledger tree, so `asd sync`
//! could not write them and `asd hydrate` could not restore them. The ASG
//! history showed four or five processes appending at once, and `asd index`
//! committing speculations in between. Both are unconditional ref moves: a
//! writer that built its commit on a stale head silently discards every
//! commit that landed since.
//!
//! `append_entry` returns Ok and writes the cache in every one of those
//! cases, so nothing but a count against the ASG tree can see the loss.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Barrier};

use agentstatedeveloper_core::{
    AsgLedgerStore, Author, AuthorKind, Engine, LedgerEntry, LedgerKind, LedgerStore, sync_to_dir,
};
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use serde_json::Value;

const KINDS: [LedgerKind; 4] = [
    LedgerKind::Decision,
    LedgerKind::Invariant,
    LedgerKind::Proof,
    LedgerKind::ValidationScenario,
];

fn author(id: &str) -> Author {
    Author {
        kind: AuthorKind::Agent,
        id: id.into(),
    }
}

/// Every entry id in the ASG ledger tree, across all symbols.
fn asg_entry_ids(engine: &Engine) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if let Ok(Value::Object(by_symbol)) = engine.repo.get_json(&engine.ref_name, "/asd/v1/ledger") {
        for bucket in by_symbol.values() {
            if let Value::Object(entries) = bucket {
                ids.extend(entries.keys().cloned());
            }
        }
    }
    ids
}

/// Every entry id `sync_to_dir` wrote to the sidecar.
fn sidecar_entry_ids(root: &Path) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let ledger = root.join(".asd/v1/ledger");
    for sym in std::fs::read_dir(&ledger).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(sym.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(stem) = f.path().file_stem().and_then(|s| s.to_str()) {
                ids.insert(stem.to_string());
            }
        }
    }
    ids
}

/// Several engines on one DB file stand in for concurrent `asd` processes
/// (parallel agents, an MCP server, git-hook re-indexes). Each appends
/// multi-kind entries across many symbols; every one must reach ASG and the
/// sidecar.
#[test]
fn concurrent_appends_from_separate_engines_all_survive() {
    const WRITERS: usize = 4;
    const PER_WRITER: usize = 12;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(".asd-state.db");
    // Initialise the store once so the writers race on appends, not on init.
    drop(Engine::open_sqlite(&db).unwrap());

    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let db = db.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let engine = Engine::open_sqlite(&db).unwrap();
                let ledger = AsgLedgerStore::from_engine(&engine);
                barrier.wait();
                let mut written = Vec::new();
                for i in 0..PER_WRITER {
                    let e = LedgerEntry::new(
                        format!("sym_{:02}", (w * PER_WRITER + i) % 17),
                        KINDS[i % KINDS.len()],
                        format!("writer {w} entry {i}"),
                        author(&format!("writer-{w}")),
                    );
                    ledger
                        .append_entry(&engine.ref_name, &e, &format!("writer-{w}"))
                        .unwrap();
                    written.push(e.entry_id);
                }
                written
            })
        })
        .collect();
    let written: BTreeSet<String> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(written.len(), WRITERS * PER_WRITER);

    let engine = Engine::open_sqlite(&db).unwrap();
    let in_asg = asg_entry_ids(&engine);
    let lost: Vec<_> = written.difference(&in_asg).collect();
    assert!(
        lost.is_empty(),
        "{} of {} acknowledged entries are missing from ASG: {:?}",
        lost.len(),
        written.len(),
        lost
    );

    sync_to_dir(&engine.repo, &engine.ref_name, dir.path()).unwrap();
    assert_eq!(sidecar_entry_ids(dir.path()), written);
}

/// A ledger write that lands while a speculation is open (the window every
/// `asd index` pass opens) must survive the speculation's commit.
#[test]
fn ledger_write_inside_open_speculation_survives_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).unwrap();
    let ledger = AsgLedgerStore::from_engine(&engine);

    let spec = engine
        .repo
        .speculate(&engine.ref_name, Some("test-index-pass".into()))
        .unwrap();
    engine
        .repo
        .spec_set_json(
            spec,
            "/asd/v1/index/by-qname",
            &serde_json::json!({ "pkg.f": "sym_f" }),
        )
        .unwrap();

    let e = LedgerEntry::new("sym_f", LedgerKind::Invariant, "must hold", author("agent"));
    ledger.append_entry(&engine.ref_name, &e, "agent").unwrap();

    engine
        .repo
        .commit_speculation(
            spec,
            CommitOptions::new("test", IntentCategory::Checkpoint, "test index pass"),
        )
        .unwrap();

    let in_asg = asg_entry_ids(&engine);
    assert!(
        in_asg.contains(&e.entry_id),
        "ledger entry written during the speculation was reverted by its commit"
    );
    // And the speculation's own write landed.
    assert!(
        engine
            .repo
            .get_json(&engine.ref_name, "/asd/v1/index/by-qname/pkg.f")
            .is_ok()
    );
}
