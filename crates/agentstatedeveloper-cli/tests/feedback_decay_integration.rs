//! Plan J t-016: end-to-end decay regression — proves that a
//! fresh `Useful` verdict boosts a symbol's score MORE than a
//! 9-month-old one (3 half-lives at 90-day default = ~12.5%
//! weight, so the old verdict adds ~0.19 instead of +1.5).
//!
//! The pure decay tests in `feedback.rs` cover the math; this
//! test covers the wiring — `created_at` flows from
//! FeedbackEntry → FeedbackStore::flat_verdicts → tuple →
//! apply_feedback_adjustments → score arithmetic. A regression
//! anywhere in that chain (e.g. someone shipping a 4-tuple but
//! dropping created_at on the floor inside the function) gets
//! caught here.

use chrono::Duration;

use agentstatedeveloper_core::{
    AsgFeedbackStore, AsgIndexStore, Engine, FeedbackEntry, FeedbackStore, FeedbackVerdict,
    IndexStore, Position, Symbol, SymbolKind, apply_feedback_adjustments,
};

fn mk_sym(sym_id: &str, qname: &str, file: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: format!("fp-{sym_id}"),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!(
            "def {}()",
            qname.rsplit('.').next().unwrap_or(qname)
        )),
        doc: None,
    }
}

fn mk_entry(entry_id: &str, sym_id: &str, qname: &str, age_days: i64) -> FeedbackEntry {
    let created_at = chrono::Utc::now() - Duration::days(age_days);
    FeedbackEntry {
        entry_id: entry_id.into(),
        symbol_id: sym_id.into(),
        symbol_qname: qname.into(),
        query: "discount".into(),
        verdict: FeedbackVerdict::Useful,
        author: "alice".into(),
        created_at,
        note: None,
        file_scope: None,
        expires_at: None,
    }
}

#[test]
fn fresh_useful_boosts_more_than_nine_month_old() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).expect("open");
    let idx = AsgIndexStore::from_engine(&engine);
    let fb = AsgFeedbackStore::from_engine(&engine);

    // Two symbols, two different feedback ages. We score each
    // independently so the boosts don't compound.
    let s_fresh = mk_sym("sym_fresh", "billing.calc.discount_fresh", "src/a.py");
    let s_old = mk_sym("sym_old", "billing.calc.discount_old", "src/b.py");
    idx.put_symbol(&engine.ref_name, &s_fresh, "t").unwrap();
    idx.put_symbol(&engine.ref_name, &s_old, "t").unwrap();

    let e_fresh = mk_entry("fb_fresh", "sym_fresh", &s_fresh.qname, 0);
    let e_old = mk_entry("fb_old", "sym_old", &s_old.qname, 270); // 9 months
    fb.record(&engine.ref_name, &e_fresh, "alice").unwrap();
    fb.record(&engine.ref_name, &e_old, "alice").unwrap();

    // Pull the flat tuples (default-method path — what real
    // callers do).
    let tuples = fb.flat_verdicts(&engine.ref_name).unwrap();
    assert_eq!(tuples.len(), 2, "got: {tuples:?}");

    // Baseline score 10.0 for each symbol; apply feedback and
    // measure the delta.
    let mut fresh_scored = vec![(10.0_f64, s_fresh.qname.clone())];
    apply_feedback_adjustments(&engine, &idx, "discount", &mut fresh_scored, &tuples);
    let fresh_delta = fresh_scored[0].0 - 10.0;

    let mut old_scored = vec![(10.0_f64, s_old.qname.clone())];
    apply_feedback_adjustments(&engine, &idx, "discount", &mut old_scored, &tuples);
    let old_delta = old_scored[0].0 - 10.0;

    // Fresh entry: ~full +1.5 (decay ≈ 1.0).
    assert!(
        (fresh_delta - 1.5).abs() < 0.01,
        "fresh boost should be ~+1.5; got +{fresh_delta}"
    );

    // 9-month-old entry at 90-day half-life = 3 half-lives →
    // 0.5^3 = 0.125 → 1.5 * 0.125 = 0.1875.
    assert!(
        (old_delta - 0.1875).abs() < 0.01,
        "9-month-old boost should be ~+0.1875 (3 half-lives at 90d default); got +{old_delta}"
    );

    // The wiring assertion: fresh strictly beats old by a
    // meaningful margin. If a future refactor drops created_at
    // somewhere in the chain, both deltas would collapse to
    // +1.5 and this assertion catches it.
    assert!(
        fresh_delta > old_delta * 3.0,
        "fresh must dominate 9-month-old by >3×; got fresh={fresh_delta}, old={old_delta}"
    );
}

#[test]
fn negative_verdicts_do_not_decay() {
    // Plan J t-016 intentionally does NOT decay suppression
    // verdicts (Noisy / WrongLayer). A 6-month-old "this is
    // wrong" verdict is still wrong unless explicitly TTL'd
    // via t-014's expires_at. Lock that.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).expect("open");
    let idx = AsgIndexStore::from_engine(&engine);
    let fb = AsgFeedbackStore::from_engine(&engine);

    let s = mk_sym("sym_n", "pkg.noisy", "src/n.py");
    idx.put_symbol(&engine.ref_name, &s, "t").unwrap();

    let mut e = mk_entry("fb_n", "sym_n", &s.qname, 365); // 1 year old
    e.verdict = FeedbackVerdict::Noisy;
    fb.record(&engine.ref_name, &e, "alice").unwrap();

    let tuples = fb.flat_verdicts(&engine.ref_name).unwrap();
    let mut scored = vec![(10.0_f64, s.qname.clone())];
    apply_feedback_adjustments(&engine, &idx, "discount", &mut scored, &tuples);

    // Noisy → score set to NEG_INFINITY → filtered out by
    // apply_feedback_adjustments. 1-year-old or 1-day-old —
    // doesn't matter; suppressions are hard.
    assert!(
        scored.is_empty(),
        "1-year-old Noisy verdict must still suppress (no decay); got: {scored:?}"
    );
}
