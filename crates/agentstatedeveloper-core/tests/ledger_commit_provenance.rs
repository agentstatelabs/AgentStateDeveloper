//! Plan C t-001 (ASD side): a ledger entry's own rationale and confidence are
//! carried onto the AgentStateGraph commit that records it, so the decision's
//! provenance lives on the commit and not only in the entry payload.

use agentstatedeveloper_core::{
    AsgLedgerStore, Author, AuthorKind, Engine, LedgerEntry, LedgerKind, LedgerStore,
};

fn author() -> Author {
    Author {
        kind: AuthorKind::Agent,
        id: "test-agent".to_string(),
    }
}

/// `append_entry` writes the entry commit and then a reverse-index commit, so
/// HEAD is the index commit. Find the entry commit by its intent description.
fn entry_commit(engine: &Engine, symbol_id: &str) -> agentstategraph_core::Commit {
    let wanted = format!("ledger decision for {symbol_id}");
    engine
        .repo
        .log(&engine.ref_name, 100)
        .expect("log")
        .into_iter()
        .find(|c| c.intent.description == wanted)
        .expect("ledger entry commit present in log")
}

/// The commit recording a ledger entry carries the entry's `body` as
/// `reasoning` and its `confidence` as the commit `confidence`.
#[test]
fn ledger_commit_carries_body_and_confidence() {
    let engine = Engine::open_in_memory().expect("open engine");

    let mut entry = LedgerEntry::new(
        "sym_abc123",
        LedgerKind::Decision,
        "use a bounded channel",
        author(),
    );
    entry.body = Some("unbounded growth under backpressure; cap at 1024".to_string());
    entry.confidence = Some(0.8);

    let ledger = AsgLedgerStore::new(&engine.repo);
    ledger
        .append_entry(&engine.ref_name, &entry, "test-agent")
        .expect("append entry");

    let commit = entry_commit(&engine, "sym_abc123");
    assert_eq!(
        commit.reasoning.as_deref(),
        Some("unbounded growth under backpressure; cap at 1024"),
        "commit.reasoning should carry the entry body"
    );
    assert_eq!(commit.confidence, Some(0.8));
}

/// With no body, `reasoning` falls back to the entry summary — never emptier
/// than what the entry records. Confidence stays absent when the entry has none.
#[test]
fn ledger_commit_falls_back_to_summary_and_omits_absent_confidence() {
    let engine = Engine::open_in_memory().expect("open engine");

    let entry = LedgerEntry::new(
        "sym_def456",
        LedgerKind::Decision,
        "adopt the retry policy",
        author(),
    );
    // body: None, confidence: None (constructor defaults)

    let ledger = AsgLedgerStore::new(&engine.repo);
    ledger
        .append_entry(&engine.ref_name, &entry, "test-agent")
        .expect("append entry");

    let commit = entry_commit(&engine, "sym_def456");
    assert_eq!(commit.reasoning.as_deref(), Some("adopt the retry policy"));
    assert_eq!(commit.confidence, None);
}
