//! Plan feedback-lifecycle t-005 — the tree and the cache must never disagree.
//!
//! Feedback has TWO read paths. `AsgFeedbackStore::list_all` prefers the
//! `asd_feedback` SQLite cache when it is warm and falls back to walking the
//! authoritative ASG tree when it is cold. Every lifecycle operation has to
//! land in both, or one path serves a verdict the other has retired.
//!
//! That is not hypothetical. Three test verdicts written into the live asd
//! store during Lens metrics work had to be removed with a throwaway binary,
//! and removing them from the tree alone left them visible via the cache.
//!
//! **Why these tests are file-backed.** `Engine::open_in_memory` sets
//! `fts: None`, so `list_all`'s cache branch never executes and an in-memory
//! test reads the tree twice while appearing to cover both paths. t-001's
//! expire tests are in-memory for exactly this reason and therefore never
//! verified the cache at all; t-003's first attempt at a both-paths test had
//! the same hole and passed with the SQLite read deleted. Every test here
//! opens a real database and asserts the cache is populated BEFORE drawing
//! conclusions from it — a coherence test that silently runs against a cold
//! cache proves nothing.

use agentstatedeveloper_core::{
    AsgFeedbackStore, Engine, FeedbackEntry, FeedbackStore, FeedbackVerdict, SearchFtsDb,
};
use chrono::{Duration, Utc};

struct Fixture {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    engine: Engine,
}

impl Fixture {
    fn new(entries: Vec<FeedbackEntry>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join(".asd-state.db");
        let engine = Engine::open_sqlite(&db).expect("file-backed engine");
        let store = AsgFeedbackStore::from_engine(&engine);
        for e in &entries {
            store.record(&engine.ref_name, e, "test").expect("record");
        }
        let f = Self {
            _dir: dir,
            db,
            engine,
        };
        f.assert_cache_warm();
        f
    }

    fn store(&self) -> AsgFeedbackStore<'_> {
        AsgFeedbackStore::from_engine(&self.engine)
    }

    /// The guard that keeps every other assertion in this file meaningful.
    fn assert_cache_warm(&self) {
        let fts = SearchFtsDb::open(&self.db).expect("open fts");
        assert!(
            fts.feedback_count() > 0,
            "SQLite cache is cold — list_all would bypass it and these tests \
             would be checking the git tree twice"
        );
    }

    /// Read path 1: the SQLite cache, directly and only.
    fn via_cache(&self) -> Vec<FeedbackEntry> {
        SearchFtsDb::open(&self.db)
            .expect("open fts")
            .list_all_feedback()
            .expect("cache read")
    }

    /// Read path 2: the authoritative ASG tree, which never consults the cache.
    fn via_tree(&self, symbol_id: &str) -> Vec<FeedbackEntry> {
        self.store()
            .list_for_symbol(&self.engine.ref_name, symbol_id)
            .expect("tree read")
    }
}

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

/// Assert one entry reads identically down both paths, for the fields that
/// decide whether ranking still sees it.
fn assert_paths_agree(f: &Fixture, entry_id: &str, symbol_id: &str, what: &str) {
    let cached = f.via_cache();
    let tree = f.via_tree(symbol_id);
    let c = cached
        .iter()
        .find(|e| e.entry_id == entry_id)
        .unwrap_or_else(|| panic!("{what}: {entry_id} missing from the SQLite cache"));
    let t = tree
        .iter()
        .find(|e| e.entry_id == entry_id)
        .unwrap_or_else(|| panic!("{what}: {entry_id} missing from the ASG tree"));

    assert_eq!(c.expires_at, t.expires_at, "{what}: expires_at diverged");
    assert_eq!(
        c.withdrawn_at, t.withdrawn_at,
        "{what}: withdrawn_at diverged"
    );
    assert_eq!(
        c.withdrawn_by, t.withdrawn_by,
        "{what}: withdrawn_by diverged"
    );
    assert_eq!(
        c.withdrawn_reason, t.withdrawn_reason,
        "{what}: withdrawn_reason diverged"
    );
    assert_eq!(
        c.is_inert(),
        t.is_inert(),
        "{what}: the two paths disagree on whether ranking should see this"
    );
}

// ---------------------------------------------------------------------------
// expire (t-001)
// ---------------------------------------------------------------------------

#[test]
fn expiry_reaches_both_read_paths() {
    // t-001 shipped with in-memory tests only, so this is the first coverage
    // of expire against the cache.
    let f = Fixture::new(vec![entry("fb-1", "util.log", FeedbackVerdict::Noisy)]);
    let store = f.store();

    let mut lapsed = store.list_all(&f.engine.ref_name).unwrap()[0].clone();
    lapsed.expires_at = Some(Utc::now());
    store
        .record(&f.engine.ref_name, &lapsed, "alice")
        .expect("re-record with expiry");

    assert_paths_agree(&f, "fb-1", "id::util.log", "expire");
    assert!(f.via_cache()[0].is_expired(), "cache still shows it live");
    assert!(
        store.flat_verdicts(&f.engine.ref_name).unwrap().is_empty(),
        "expired verdict still reaching ranking"
    );
}

// ---------------------------------------------------------------------------
// withdraw (t-003)
// ---------------------------------------------------------------------------

#[test]
fn withdrawal_reaches_both_read_paths() {
    let f = Fixture::new(vec![entry("fb-1", "util.log", FeedbackVerdict::Noisy)]);
    let store = f.store();
    store
        .withdraw(&f.engine.ref_name, "fb-1", "craig", Some("wrong symbol"))
        .expect("withdraw");

    assert_paths_agree(&f, "fb-1", "id::util.log", "withdraw");
    assert_eq!(f.via_cache()[0].withdrawn_by.as_deref(), Some("craig"));
    assert!(
        store.flat_verdicts(&f.engine.ref_name).unwrap().is_empty(),
        "withdrawn verdict still reaching ranking"
    );
}

// ---------------------------------------------------------------------------
// The asymmetry both operations share
// ---------------------------------------------------------------------------

#[test]
fn retired_verdicts_leave_ranking_but_stay_listed_on_both_paths() {
    let f = Fixture::new(vec![
        entry("fb-expired", "util.log", FeedbackVerdict::Noisy),
        entry("fb-withdrawn", "util.log", FeedbackVerdict::Noisy),
        entry("fb-live", "pay.charge", FeedbackVerdict::Useful),
    ]);
    let store = f.store();

    let mut lapsed = entry("fb-expired", "util.log", FeedbackVerdict::Noisy);
    lapsed.expires_at = Some(Utc::now() - Duration::hours(1));
    store.record(&f.engine.ref_name, &lapsed, "alice").unwrap();
    store
        .withdraw(&f.engine.ref_name, "fb-withdrawn", "craig", None)
        .unwrap();

    // Ranking sees only the live one…
    let ranked = store.flat_verdicts(&f.engine.ref_name).unwrap();
    assert_eq!(ranked.len(), 1, "ranking view: {ranked:?}");
    assert_eq!(ranked[0].2, FeedbackVerdict::Useful);

    // …while every path still lists all three. A retired verdict explains why
    // a past search ranked as it did; hiding it makes old results
    // inexplicable.
    assert_eq!(store.list_all(&f.engine.ref_name).unwrap().len(), 3);
    assert_eq!(f.via_cache().len(), 3, "cache dropped a retired entry");
    assert_eq!(f.via_tree("id::util.log").len(), 2, "tree dropped one");
}

#[test]
fn a_cold_cache_and_a_warm_one_report_the_same_thing() {
    // The fallback path: `list_all` walks the tree when the cache is empty.
    // Both answers must be identical, or which one you get depends on whether
    // someone happened to reindex.
    let f = Fixture::new(vec![entry("fb-1", "util.log", FeedbackVerdict::Noisy)]);
    let store = f.store();
    store
        .withdraw(&f.engine.ref_name, "fb-1", "craig", Some("wrong"))
        .unwrap();

    let warm = store.list_all(&f.engine.ref_name).unwrap();

    // Empty the cache so `list_all` must fall back to the tree.
    rusqlite::Connection::open(&f.db)
        .expect("open db")
        .execute("DELETE FROM asd_feedback", [])
        .expect("clear cache");
    let cold = store.list_all(&f.engine.ref_name).unwrap();

    assert_eq!(warm.len(), cold.len(), "warm and cold disagree on count");
    assert_eq!(warm[0].entry_id, cold[0].entry_id);
    assert_eq!(warm[0].withdrawn_at, cold[0].withdrawn_at);
    assert_eq!(warm[0].withdrawn_by, cold[0].withdrawn_by);
    assert_eq!(warm[0].is_inert(), cold[0].is_inert());
}

#[test]
fn file_scope_verdicts_are_coherent_too() {
    // File-scope verdicts take a different route through the flat_* views, so
    // they get their own coherence check rather than being assumed to follow.
    let mut e = entry("fb-glob", "", FeedbackVerdict::Noisy);
    e.file_scope = Some("src/legacy/**".to_string());
    e.symbol_id = "__file_scope__glob".to_string();
    let f = Fixture::new(vec![e]);
    let store = f.store();

    assert_eq!(
        store
            .flat_file_scope_verdicts(&f.engine.ref_name)
            .unwrap()
            .len(),
        1
    );
    store
        .withdraw(&f.engine.ref_name, "fb-glob", "craig", None)
        .unwrap();

    assert_paths_agree(&f, "fb-glob", "__file_scope__glob", "file-scope withdraw");
    assert!(
        store
            .flat_file_scope_verdicts(&f.engine.ref_name)
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// purge (t-004)
// ---------------------------------------------------------------------------

#[test]
fn purge_clears_both_stores() {
    let f = Fixture::new(vec![
        entry("fb-gone", "util.log", FeedbackVerdict::Noisy),
        entry("fb-stays", "pay.charge", FeedbackVerdict::Useful),
    ]);
    let store = f.store();

    let purged = store
        .purge(&f.engine.ref_name, "fb-gone")
        .expect("purge")
        .expect("entry existed");
    assert_eq!(purged.entry_id, "fb-gone");

    // Gone from the cache…
    assert!(
        !f.via_cache().iter().any(|e| e.entry_id == "fb-gone"),
        "purged entry still in the SQLite cache — list_all would serve it"
    );
    // …and from the authoritative tree.
    assert!(
        !f.via_tree("id::util.log")
            .iter()
            .any(|e| e.entry_id == "fb-gone"),
        "purged entry still in the ASG tree"
    );
    // The unrelated entry is untouched.
    assert!(f.via_cache().iter().any(|e| e.entry_id == "fb-stays"));
}

#[test]
fn purging_the_last_entry_for_a_symbol_leaves_nothing_behind() {
    // t-005 asks specifically about an orphaned parent node: removing the only
    // entry under a symbol must not leave an empty `/asd/v1/feedback/<sym>`
    // that later reads trip over or that makes the symbol look like it still
    // carries feedback.
    let f = Fixture::new(vec![entry("fb-only", "util.log", FeedbackVerdict::Noisy)]);
    let store = f.store();
    store.purge(&f.engine.ref_name, "fb-only").unwrap();

    assert!(
        f.via_tree("id::util.log").is_empty(),
        "symbol still reports feedback after its last entry was purged"
    );
    assert!(store.list_all(&f.engine.ref_name).unwrap().is_empty());
    assert!(store.flat_verdicts(&f.engine.ref_name).unwrap().is_empty());

    // And a cold cache must agree — the fallback tree walk must not resurrect
    // the entry from an orphaned node.
    rusqlite::Connection::open(&f.db)
        .expect("open db")
        .execute("DELETE FROM asd_feedback", [])
        .expect("clear cache");
    assert!(
        store.list_all(&f.engine.ref_name).unwrap().is_empty(),
        "cold-cache read resurrected a purged entry"
    );
}

#[test]
fn purging_an_unknown_entry_reports_rather_than_half_succeeding() {
    let f = Fixture::new(vec![entry("fb-real", "util.log", FeedbackVerdict::Noisy)]);
    let store = f.store();
    assert!(
        store
            .purge(&f.engine.ref_name, "fb-nope")
            .unwrap()
            .is_none(),
        "purge invented a deletion"
    );
    assert_eq!(store.list_all(&f.engine.ref_name).unwrap().len(), 1);
    assert_eq!(f.via_cache().len(), 1);
}

#[test]
fn purge_removes_a_withdrawn_entry_too() {
    // The realistic sequence: retract first, then discover the note held
    // something that must not persist.
    let f = Fixture::new(vec![entry("fb-1", "util.log", FeedbackVerdict::Noisy)]);
    let store = f.store();
    store
        .withdraw(&f.engine.ref_name, "fb-1", "craig", Some("oops"))
        .unwrap();
    store.purge(&f.engine.ref_name, "fb-1").unwrap();

    assert!(f.via_cache().is_empty(), "cache retained a purged entry");
    assert!(f.via_tree("id::util.log").is_empty(), "tree retained it");
}
