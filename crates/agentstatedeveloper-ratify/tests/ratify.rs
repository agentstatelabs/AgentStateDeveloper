//! Integration tests for approve/reject/withdraw using an in-memory ASG repo.

use agentstatedeveloper_core::{
    AsgLedgerStore, Author, AuthorKind, Engine, LedgerEntry, LedgerKind, LedgerStore,
};
use agentstatedeveloper_ratify::RatifyLedgerStore;

fn make_entry(symbol_id: &str, author_id: &str, tags: &[&str]) -> LedgerEntry {
    let mut e = LedgerEntry::new(
        symbol_id,
        LedgerKind::Hazard,
        "test entry body",
        Author { kind: AuthorKind::Human, id: author_id.to_string() },
    );
    for t in tags {
        e.tags.push(t.to_string());
    }
    e
}

fn seed(engine: &Engine, entry: &LedgerEntry) {
    let store = AsgLedgerStore { repo: &engine.repo };
    store.append_entry(&engine.ref_name, entry, "test-agent").expect("append");
}

// ---------------------------------------------------------------------------
// approve happy path
// ---------------------------------------------------------------------------

#[test]
fn approve_transitions_entry_to_approved() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let outcome = store
        .approve_entry(&engine.ref_name, &entry.entry_id, "bob", "human", None, "test-agent")
        .expect("approve");

    assert!(!outcome.already_approved);
    assert!(outcome.entry.tags.iter().any(|t| t == "approved"));
    assert!(!outcome.entry.tags.iter().any(|t| t == "awaiting-approval"));
    assert!(outcome.entry.tags.iter().any(|t| t.starts_with("approved-by:")));
    assert!(outcome.entry.tags.iter().any(|t| t.starts_with("approved-at:")));
}

#[test]
fn approve_with_message_appends_to_body() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let outcome = store
        .approve_entry(
            &engine.ref_name,
            &entry.entry_id,
            "bob",
            "human",
            Some("looks good"),
            "test-agent",
        )
        .expect("approve with message");

    let body = outcome.entry.body.expect("body should be set");
    assert!(body.contains("looks good"), "body: {body}");
    assert!(body.contains("bob"), "body should mention approver: {body}");
}

// ---------------------------------------------------------------------------
// approve idempotency
// ---------------------------------------------------------------------------

#[test]
fn approve_already_approved_is_idempotent() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    store
        .approve_entry(&engine.ref_name, &entry.entry_id, "bob", "human", None, "test-agent")
        .expect("first approve");

    let outcome = store
        .approve_entry(&engine.ref_name, &entry.entry_id, "carol", "human", None, "test-agent")
        .expect("second approve");

    assert!(outcome.already_approved, "second approve should be idempotent");
}

// ---------------------------------------------------------------------------
// approve guards
// ---------------------------------------------------------------------------

#[test]
fn approve_rejected_entry_errors() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    store
        .reject_entry(
            &engine.ref_name,
            &entry.entry_id,
            "bob",
            "human",
            "not ready",
            "test-agent",
        )
        .expect("reject");

    let err = store
        .approve_entry(&engine.ref_name, &entry.entry_id, "carol", "human", None, "test-agent")
        .unwrap_err();
    assert!(err.to_string().contains("rejected"), "err: {err}");
}

#[test]
fn approve_non_awaiting_entry_errors() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &[]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let err = store
        .approve_entry(&engine.ref_name, &entry.entry_id, "bob", "human", None, "test-agent")
        .unwrap_err();
    assert!(err.to_string().contains("not awaiting approval"), "err: {err}");
}

// ---------------------------------------------------------------------------
// authorize_reviewer enforcement
// ---------------------------------------------------------------------------

#[test]
fn approve_enforces_approver_tag() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval", "approver:security-team"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);

    let err = store
        .approve_entry(
            &engine.ref_name,
            &entry.entry_id,
            "random-dev",
            "human",
            None,
            "test-agent",
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not match"), "err: {err}");

    let ok = store
        .approve_entry(
            &engine.ref_name,
            &entry.entry_id,
            "sec-lead",
            "security-team",
            None,
            "test-agent",
        );
    assert!(ok.is_ok(), "security-team member should be allowed: {ok:?}");
}

// ---------------------------------------------------------------------------
// reject happy path + idempotency
// ---------------------------------------------------------------------------

#[test]
fn reject_transitions_entry_to_rejected() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let outcome = store
        .reject_entry(
            &engine.ref_name,
            &entry.entry_id,
            "bob",
            "human",
            "needs more context",
            "test-agent",
        )
        .expect("reject");

    assert!(!outcome.already_resolved);
    assert!(outcome.entry.tags.iter().any(|t| t == "rejected"));
    assert!(!outcome.entry.tags.iter().any(|t| t == "awaiting-approval"));
    assert!(outcome.entry.tags.iter().any(|t| t.starts_with("rejected-by:")));
    let body = outcome.entry.body.expect("body");
    assert!(body.contains("needs more context"), "body: {body}");
}

#[test]
fn reject_already_rejected_is_idempotent() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    store
        .reject_entry(&engine.ref_name, &entry.entry_id, "bob", "human", "r1", "test-agent")
        .expect("first reject");
    let outcome = store
        .reject_entry(&engine.ref_name, &entry.entry_id, "carol", "human", "r2", "test-agent")
        .expect("second reject");
    assert!(outcome.already_resolved);
}

#[test]
fn reject_requires_nonempty_reason() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let err = store
        .reject_entry(&engine.ref_name, &entry.entry_id, "bob", "human", "  ", "test-agent")
        .unwrap_err();
    assert!(err.to_string().contains("non-empty reason"), "err: {err}");
}

// ---------------------------------------------------------------------------
// withdraw happy path + author guard
// ---------------------------------------------------------------------------

#[test]
fn withdraw_transitions_entry_to_withdrawn() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let outcome = store
        .withdraw_entry(&engine.ref_name, &entry.entry_id, "alice", "test-agent")
        .expect("withdraw");

    assert!(!outcome.already_resolved);
    assert!(outcome.entry.tags.iter().any(|t| t == "withdrawn"));
    assert!(!outcome.entry.tags.iter().any(|t| t == "awaiting-approval"));
    assert!(outcome.entry.tags.iter().any(|t| t.starts_with("withdrawn-at:")));
}

#[test]
fn withdraw_rejects_non_author() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    let err = store
        .withdraw_entry(&engine.ref_name, &entry.entry_id, "bob", "test-agent")
        .unwrap_err();
    assert!(err.to_string().contains("original author"), "err: {err}");
}

#[test]
fn withdraw_already_withdrawn_is_idempotent() {
    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_foo", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let store = RatifyLedgerStore::new(&engine.repo);
    store
        .withdraw_entry(&engine.ref_name, &entry.entry_id, "alice", "test-agent")
        .expect("first withdraw");
    let outcome = store
        .withdraw_entry(&engine.ref_name, &entry.entry_id, "alice", "test-agent")
        .expect("second withdraw");
    assert!(outcome.already_resolved);
}

// ---------------------------------------------------------------------------
// entry not found
// ---------------------------------------------------------------------------

#[test]
fn approve_missing_entry_errors() {
    let engine = Engine::open_in_memory().expect("engine");
    let store = RatifyLedgerStore::new(&engine.repo);
    let err = store
        .approve_entry(&engine.ref_name, "nonexistent-id", "bob", "human", None, "test-agent")
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "err: {err}");
}

// ---------------------------------------------------------------------------
// RatifyOpsImpl (zero-sized) dispatches correctly
// ---------------------------------------------------------------------------

#[test]
fn ratify_ops_impl_approve_works() {
    use agentstatedeveloper_core::RatifyOps;
    use agentstatedeveloper_ratify::RatifyOpsImpl;

    let engine = Engine::open_in_memory().expect("engine");
    let entry = make_entry("sym_bar", "alice", &["awaiting-approval"]);
    seed(&engine, &entry);

    let ops = RatifyOpsImpl;
    let outcome = ops
        .approve_entry(
            &engine.repo,
            &engine.ref_name,
            &entry.entry_id,
            "bob",
            "human",
            None,
            "test-agent",
        )
        .expect("approve via RatifyOpsImpl");
    assert!(!outcome.already_approved);
    assert!(outcome.entry.tags.iter().any(|t| t == "approved"));
}
