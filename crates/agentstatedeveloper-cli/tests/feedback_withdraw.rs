//! `asd feedback withdraw` — plan feedback-lifecycle t-003.
//!
//! These assert the two things the design note says to assert, because both
//! are places this codebase has already been bitten:
//!
//! 1. What the RANKING VIEWS return — not what the predicate computes. Plan J
//!    t-014 shipped `expires_at`, `is_expired()` and doc comments claiming
//!    enforcement while nothing called the helper, and every unit test passed
//!    because they all tested the helper in isolation.
//! 2. That BOTH read paths agree — `list_all` has a SQLite fast path, so a
//!    withdrawal recorded only in the git tree would still be served as live
//!    from the cache. That divergence is the reason this plan exists.

use agentstatedeveloper_core::{
    AsgFeedbackStore, Engine, FeedbackEntry, FeedbackStore, FeedbackVerdict,
};
use chrono::{Duration, Utc};

fn entry(id: &str, qname: &str, verdict: FeedbackVerdict) -> FeedbackEntry {
    FeedbackEntry {
        entry_id: id.to_string(),
        symbol_id: format!("id::{qname}"),
        symbol_qname: qname.to_string(),
        query: "charge a card".to_string(),
        verdict,
        author: "alice".to_string(),
        created_at: Utc::now(),
        note: None,
        file_scope: None,
        expires_at: None,
        withdrawn_at: None,
        withdrawn_by: None,
        withdrawn_reason: None,
    }
}

fn engine_with(entries: Vec<FeedbackEntry>) -> Engine {
    let engine = Engine::open_in_memory().expect("in-memory engine");
    let store = AsgFeedbackStore::from_engine(&engine);
    for e in &entries {
        store.record(&engine.ref_name, e, "test").expect("record");
    }
    engine
}

#[test]
fn a_withdrawn_verdict_leaves_the_ranking_view() {
    let engine = engine_with(vec![
        entry("fb-bad", "util.log", FeedbackVerdict::Noisy),
        entry("fb-good", "pay.charge", FeedbackVerdict::Useful),
    ]);
    let store = AsgFeedbackStore::from_engine(&engine);

    assert_eq!(store.flat_verdicts(&engine.ref_name).unwrap().len(), 2);

    let w = store
        .withdraw(&engine.ref_name, "fb-bad", "craig", Some("mistyped"))
        .expect("withdraw")
        .expect("entry existed");
    assert!(w.is_withdrawn());
    assert_eq!(w.withdrawn_by.as_deref(), Some("craig"));
    assert_eq!(w.withdrawn_reason.as_deref(), Some("mistyped"));

    let ranked = store.flat_verdicts(&engine.ref_name).unwrap();
    assert_eq!(
        ranked.len(),
        1,
        "withdrawn verdict still ranking: {ranked:?}"
    );
    assert_eq!(ranked[0].2, FeedbackVerdict::Useful);
}

#[test]
fn withdrawn_entries_stay_listed() {
    // The deliberate asymmetry: ranking stops seeing it, `list_all` does not.
    // A retracted verdict still explains why a past search ranked as it did.
    let engine = engine_with(vec![entry("fb-bad", "util.log", FeedbackVerdict::Noisy)]);
    let store = AsgFeedbackStore::from_engine(&engine);
    store
        .withdraw(&engine.ref_name, "fb-bad", "craig", None)
        .unwrap();

    let listed = store.list_all(&engine.ref_name).unwrap();
    assert_eq!(listed.len(), 1, "withdrawn entries must stay listed");
    assert!(listed[0].is_withdrawn());
    assert!(store.flat_verdicts(&engine.ref_name).unwrap().is_empty());
}

#[test]
fn withdrawal_reaches_both_read_paths() {
    // THE invariant this plan exists for, and it needs a FILE-BACKED engine:
    // `Engine::open_in_memory` sets `fts: None`, so `list_all`'s SQLite fast
    // path never triggers and an in-memory version of this test reads the git
    // tree twice while appearing to cover both. Caught by mutation-testing —
    // deleting the withdrawal columns from the SQLite read left the
    // in-memory version passing.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).expect("file-backed engine");
    let store = AsgFeedbackStore::from_engine(&engine);
    store
        .record(
            &engine.ref_name,
            &entry("fb-bad", "util.log", FeedbackVerdict::Noisy),
            "test",
        )
        .expect("record");

    // Prove the cache is actually populated, or the rest proves nothing.
    let fts = agentstatedeveloper_core::SearchFtsDb::open(&db).expect("fts");
    assert!(
        fts.feedback_count() > 0,
        "cache cold — list_all would bypass it and this test would be vacuous"
    );

    store
        .withdraw(&engine.ref_name, "fb-bad", "craig", Some("wrong"))
        .unwrap();

    // Path 1: the SQLite cache, read directly.
    let cached = fts.list_all_feedback().expect("cache read");
    assert_eq!(cached.len(), 1);
    assert!(
        cached[0].is_withdrawn(),
        "SQLite cache still serving it live"
    );
    assert_eq!(cached[0].withdrawn_by.as_deref(), Some("craig"));
    assert_eq!(cached[0].withdrawn_reason.as_deref(), Some("wrong"));

    // Path 2: the authoritative git tree, which never consults the cache.
    let via_tree = store
        .list_for_symbol(&engine.ref_name, "id::util.log")
        .unwrap();
    assert_eq!(via_tree.len(), 1);
    assert!(via_tree[0].is_withdrawn(), "git tree stale");

    // And the view that actually gates ranking.
    assert!(store.flat_verdicts(&engine.ref_name).unwrap().is_empty());
}

#[test]
fn withdrawing_twice_keeps_the_first_timestamp() {
    let engine = engine_with(vec![entry("fb-bad", "util.log", FeedbackVerdict::Noisy)]);
    let store = AsgFeedbackStore::from_engine(&engine);

    let first = store
        .withdraw(&engine.ref_name, "fb-bad", "craig", Some("wrong"))
        .unwrap()
        .unwrap();
    let again = store
        .withdraw(&engine.ref_name, "fb-bad", "someone-else", Some("other"))
        .unwrap()
        .unwrap();

    assert_eq!(
        first.withdrawn_at, again.withdrawn_at,
        "a second withdrawal must not move the timestamp"
    );
    assert_eq!(again.withdrawn_by.as_deref(), Some("craig"));
}

#[test]
fn withdrawing_an_unknown_entry_reports_rather_than_inventing_one() {
    let engine = engine_with(vec![entry("fb-real", "util.log", FeedbackVerdict::Noisy)]);
    let store = AsgFeedbackStore::from_engine(&engine);
    assert!(
        store
            .withdraw(&engine.ref_name, "fb-nope", "craig", None)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.list_all(&engine.ref_name).unwrap().len(), 1);
}

#[test]
fn withdrawal_and_expiry_are_independent_states() {
    // Withdrawal must not be revivable by future-dating an expiry — the
    // reason the two fields are separate rather than one.
    let mut e = entry("fb-bad", "util.log", FeedbackVerdict::Noisy);
    e.expires_at = Some(Utc::now() + Duration::days(30)); // explicitly NOT expired
    let engine = engine_with(vec![e]);
    let store = AsgFeedbackStore::from_engine(&engine);

    store
        .withdraw(&engine.ref_name, "fb-bad", "craig", None)
        .unwrap();

    let listed = &store.list_all(&engine.ref_name).unwrap()[0];
    assert!(!listed.is_expired(), "future expiry means not expired");
    assert!(listed.is_withdrawn());
    assert!(listed.is_inert(), "inert via withdrawal alone");
    assert!(
        store.flat_verdicts(&engine.ref_name).unwrap().is_empty(),
        "a live expiry must not resurrect a withdrawn verdict"
    );
}

#[test]
fn file_scope_verdicts_withdraw_too() {
    let mut e = entry("fb-glob", "", FeedbackVerdict::Noisy);
    e.file_scope = Some("src/legacy/**".to_string());
    let engine = engine_with(vec![e]);
    let store = AsgFeedbackStore::from_engine(&engine);

    assert_eq!(
        store
            .flat_file_scope_verdicts(&engine.ref_name)
            .unwrap()
            .len(),
        1
    );
    store
        .withdraw(&engine.ref_name, "fb-glob", "craig", None)
        .unwrap();
    assert!(
        store
            .flat_file_scope_verdicts(&engine.ref_name)
            .unwrap()
            .is_empty(),
        "file-scope verdicts withdraw on the same terms"
    );
}
