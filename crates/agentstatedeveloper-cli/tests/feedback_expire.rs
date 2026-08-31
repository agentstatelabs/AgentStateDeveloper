//! `asd feedback expire` and the expiry filter it depends on.
//!
//! The bug these guard: Plan J t-014 shipped `expires_at`, `is_expired()`,
//! `--ttl-days`, SQLite persistence and doc comments on both the field and
//! the helper asserting that lapsed verdicts stop influencing ranking — but
//! never wired the filter into `flat_verdicts`. Every unit test passed,
//! because they all tested `is_expired()` in isolation and none asked whether
//! anything called it. So these tests deliberately assert the *integration*:
//! what the ranking views return, and what both stores hold.

use agentstatedeveloper_core::{
    AsgFeedbackStore, Engine, FeedbackEntry, FeedbackStore, FeedbackVerdict,
};
use chrono::{Duration, Utc};

fn entry(id: &str, qname: &str, query: &str, verdict: FeedbackVerdict) -> FeedbackEntry {
    FeedbackEntry {
        entry_id: id.to_string(),
        symbol_id: format!("id::{qname}"),
        symbol_qname: qname.to_string(),
        query: query.to_string(),
        verdict,
        author: "test".to_string(),
        created_at: Utc::now(),
        note: None,
        file_scope: None,
        expires_at: None,
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
fn lapsed_verdicts_are_excluded_from_the_ranking_view() {
    let mut expired = entry("fb-old", "pay.charge", "charge", FeedbackVerdict::Noisy);
    expired.expires_at = Some(Utc::now() - Duration::days(1));
    let live = entry("fb-live", "pay.charge", "charge", FeedbackVerdict::Useful);

    let engine = engine_with(vec![expired, live]);
    let store = AsgFeedbackStore::from_engine(&engine);

    let ranked = store
        .flat_verdicts(&engine.ref_name)
        .expect("flat_verdicts");
    assert_eq!(
        ranked.len(),
        1,
        "the lapsed verdict must not reach ranking; got {ranked:?}"
    );
    assert_eq!(ranked[0].2, FeedbackVerdict::Useful);
}

#[test]
fn a_future_expiry_still_counts() {
    // Guards the inverse: `expires_at` in the future must NOT be filtered, or
    // `--ttl-days 30` would silently mean "ignore this immediately".
    let mut later = entry("fb-later", "pay.charge", "charge", FeedbackVerdict::Noisy);
    later.expires_at = Some(Utc::now() + Duration::days(30));

    let engine = engine_with(vec![later]);
    let store = AsgFeedbackStore::from_engine(&engine);
    assert_eq!(
        store.flat_verdicts(&engine.ref_name).expect("flat").len(),
        1
    );
}

#[test]
fn lapsed_file_scope_verdicts_are_excluded_too() {
    let mut e = entry("fb-glob", "", "charge", FeedbackVerdict::Noisy);
    e.file_scope = Some("src/legacy/**".to_string());
    e.expires_at = Some(Utc::now() - Duration::hours(1));

    let engine = engine_with(vec![e]);
    let store = AsgFeedbackStore::from_engine(&engine);
    assert!(
        store
            .flat_file_scope_verdicts(&engine.ref_name)
            .expect("flat")
            .is_empty(),
        "file-scope verdicts expire on the same terms as symbol verdicts"
    );
}

#[test]
fn expired_entries_stay_visible_in_list_all() {
    // The deliberate asymmetry: ranking stops seeing a lapsed verdict, but it
    // remains listed. It still explains why a past search ranked as it did —
    // hiding it would make old results inexplicable.
    let mut e = entry("fb-old", "pay.charge", "charge", FeedbackVerdict::Noisy);
    e.expires_at = Some(Utc::now() - Duration::days(1));

    let engine = engine_with(vec![e]);
    let store = AsgFeedbackStore::from_engine(&engine);

    let listed = store.list_all(&engine.ref_name).expect("list_all");
    assert_eq!(listed.len(), 1, "expired entries stay listed");
    assert!(listed[0].is_expired());
    assert!(
        store
            .flat_verdicts(&engine.ref_name)
            .expect("flat")
            .is_empty(),
        "…but ranking must not see it"
    );
}
