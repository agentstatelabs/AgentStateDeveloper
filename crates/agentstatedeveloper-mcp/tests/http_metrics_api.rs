//! In-process integration tests for the distilled-metrics endpoints:
//! `/api/v1/history/milestones`, `/api/v1/history/rollup`,
//! `/api/v1/commits`, `/api/v1/feedback`, `/api/v1/index-health` and
//! `/api/v1/scorecard`.
//!
//! Same shape as `http_lens_api.rs`: a hand-built in-memory engine and
//! `tower::ServiceExt::oneshot` dispatch, so the tests always run without a
//! pre-indexed fixture DB or a bound socket.
//!
//! The fixture writes commits under three agents and two intent categories,
//! including `Checkpoint` commits (which the ASG extractor lifts onto the
//! milestone spine), so the rollup/milestone/commit endpoints all have real
//! distilled rows to filter.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Effect,
    EffectCategory, EffectDecl, Engine, FeedbackEntry, FeedbackStore, FeedbackVerdict, IndexStore,
    LedgerEntry, LedgerKind, LedgerStore, Position, Symbol, SymbolKind,
};
use agentstatedeveloper_mcp::build_router;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use tokio::sync::Mutex;
use tower::ServiceExt;

fn sym_id(qname: &str) -> String {
    format!("id::{}", qname)
}

fn make_symbol(qname: &str, file: &str, line: u32) -> Symbol {
    Symbol {
        symbol_id: sym_id(qname),
        symbol_fp: format!("fp::{}", qname),
        qname: qname.to_string(),
        language: "python".to_string(),
        kind: SymbolKind::Function,
        file: file.to_string(),
        start: Position { line, col: 0 },
        end: Position {
            line: line + 10,
            col: 0,
        },
        signature: Some(format!("def {}()", qname.rsplit('.').next().unwrap())),
        doc: Some(format!("Doc for {}.", qname)),
    }
}

/// Write one commit with an explicit agent + intent so the distilled rollup
/// has something to group by.
fn commit(engine: &Engine, agent: &str, intent: IntentCategory, description: &str, n: u32) {
    let opts = CommitOptions::new(agent, intent, description);
    engine
        .repo
        .set_json(
            &engine.ref_name,
            // Non-numeric leaf: a bare integer segment is read as a list
            // index, which collides with the map written by the first commit.
            &format!("/test/marker/m{}", n),
            &serde_json::json!({ "n": n }),
            opts,
        )
        .expect("commit marker");
}

fn put_feedback(
    engine: &Engine,
    qname: &str,
    entry_id: &str,
    query: &str,
    verdict: FeedbackVerdict,
    author: &str,
    note: Option<&str>,
    expires_in_days: Option<i64>,
) {
    let store = AsgFeedbackStore::from_engine(engine);
    let created = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
    let entry = FeedbackEntry {
        entry_id: entry_id.to_string(),
        symbol_id: sym_id(qname),
        symbol_qname: qname.to_string(),
        query: query.to_string(),
        verdict,
        author: author.to_string(),
        created_at: created,
        note: note.map(str::to_string),
        file_scope: None,
        // Relative to *now*, not to `created_at`, so "expired" is
        // deterministic regardless of when the suite runs.
        expires_at: expires_in_days.map(|d| Utc::now() + Duration::days(d)),
    };
    store
        .record(&engine.ref_name, &entry, "metrics-test")
        .expect("record feedback");
}

/// Build the fixture engine + router.
async fn router() -> axum::Router {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    let index_store = AsgIndexStore::new(&engine.repo);

    for (qname, file, line) in [
        ("app.main", "app.py", 1),
        ("pay.charge_card", "pay.py", 10),
        ("net.post", "net.py", 20),
        ("util.log", "util.py", 30),
    ] {
        index_store
            .put_symbol(
                &engine.ref_name,
                &make_symbol(qname, file, line),
                "metrics-test",
            )
            .expect("put symbol");
    }

    // One symbol carries a verified-effect decl and an ownership entry so the
    // scorecard's truth dimension is non-zero rather than trivially 0.
    {
        use agentstatedeveloper_core::EffectStore;
        let store = AsgEffectStore::new(&engine.repo);
        let decl = EffectDecl {
            symbol_id: sym_id("net.post"),
            declared: vec![Effect::new(EffectCategory::IoNetOut)],
            transitive: Vec::new(),
            verification: None,
            confidence: None,
            runtime: None,
            matched_policy: None,
        };
        store
            .put_effects(&engine.ref_name, &sym_id("net.post"), &decl, "metrics-test")
            .expect("put effects");
    }
    {
        let store = AsgLedgerStore::new(&engine.repo);
        let entry = LedgerEntry {
            entry_id: "e-own-1".to_string(),
            symbol_id: sym_id("pay.charge_card"),
            kind: LedgerKind::Ownership,
            summary: "payments team owns this".to_string(),
            body: None,
            author: Author {
                kind: AuthorKind::Agent,
                id: "metrics-test".to_string(),
            },
            confidence: Some(0.9),
            evidence: Vec::new(),
            supersedes: Vec::new(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            tags: vec!["ctx:plan-t".to_string()],
            matched_policy: None,
            role: None,
            command: None,
        };
        store
            .append_entry(&engine.ref_name, &entry, "metrics-test")
            .expect("append ledger entry");
    }

    // Distilled-history fodder: two agents, two intent categories. The two
    // Checkpoint commits become milestones; the Refine ones stay rollup-only.
    commit(
        &engine,
        "alice",
        IntentCategory::Refine,
        "widen the parser",
        1,
    );
    commit(
        &engine,
        "alice",
        IntentCategory::Refine,
        "tighten the lexer",
        2,
    );
    commit(
        &engine,
        "bob",
        IntentCategory::Refine,
        "rename the adapter",
        3,
    );
    commit(
        &engine,
        "alice",
        IntentCategory::Checkpoint,
        "checkpoint: parser stable",
        4,
    );
    commit(
        &engine,
        "bob",
        IntentCategory::Checkpoint,
        "checkpoint: adapter stable",
        5,
    );

    put_feedback(
        &engine,
        "pay.charge_card",
        "fb-1",
        "charge a card",
        FeedbackVerdict::Useful,
        "alice",
        Some("exactly the entry point"),
        None,
    );
    put_feedback(
        &engine,
        "util.log",
        "fb-2",
        "charge a card",
        FeedbackVerdict::Noisy,
        "bob",
        None,
        None,
    );
    put_feedback(
        &engine,
        "net.post",
        "fb-3",
        "outbound http",
        FeedbackVerdict::Useful,
        "alice",
        None,
        // Already lapsed — must still be listed, flagged `expired`.
        Some(-1),
    );

    build_router(
        Arc::new(Mutex::new(engine)),
        PathBuf::from(":memory:"),
        None,
        None,
        true,
    )
}

async fn get_body(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(
        |_| serde_json::json!({"_raw": String::from_utf8_lossy(&bytes).to_string()}),
    );
    (status, value)
}

/// Pull a facet axis into `(value, count)` pairs for easier assertions.
fn facet_pairs(body: &serde_json::Value, axis: &str) -> Vec<(String, u64)> {
    body["facets"][axis]
        .as_array()
        .unwrap_or_else(|| panic!("facet axis {} missing; body={}", axis, body))
        .iter()
        .map(|f| {
            (
                f["value"].as_str().unwrap().to_string(),
                f["count"].as_u64().unwrap(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// /api/v1/history/milestones
// ---------------------------------------------------------------------------

/// The fixture's own two `Checkpoint` commits, plus the engine's `system`
/// "Initialize empty state" checkpoint. Asserted as a named constant because
/// several tests pivot on it and the engine's bookkeeping commits are not
/// this suite's to pin down.
const FIXTURE_MILESTONES: u64 = 3;

#[tokio::test]
async fn milestones_lists_checkpoint_commits_with_facets() {
    let (status, body) = get_body(router().await, "/api/v1/history/milestones").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);

    // Only Checkpoint-intent commits reach the spine — Refine ones do not.
    assert_eq!(body["total"], FIXTURE_MILESTONES, "body={}", body);
    assert_eq!(
        facet_pairs(&body, "kinds"),
        vec![("checkpoint".into(), FIXTURE_MILESTONES)],
        "body={}",
        body
    );
    let agents = facet_pairs(&body, "agents");
    for who in ["alice", "bob"] {
        assert_eq!(
            agents.iter().find(|(v, _)| v == who).map(|(_, c)| *c),
            Some(1),
            "{} should own exactly one milestone; body={}",
            who,
            body
        );
    }
}

#[tokio::test]
async fn milestones_report_whether_each_pins_a_state_root() {
    let (_, body) = get_body(router().await, "/api/v1/history/milestones").await;
    for m in body["items"].as_array().unwrap() {
        // Rows written by a current extractor always carry the retention
        // hook; `unpinned` exists for legacy rows, and must agree.
        assert_eq!(m["pins_state"], true, "body={}", body);
        assert!(m["state_root"].is_string(), "body={}", body);
    }
    assert_eq!(body["unpinned"], 0, "body={}", body);
}

#[tokio::test]
async fn milestones_free_text_search_narrows_results() {
    let (status, body) = get_body(router().await, "/api/v1/history/milestones?q=adapter").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["total"], 1, "body={}", body);
    assert_eq!(body["items"][0]["agent"], "bob", "body={}", body);
    assert!(
        body["items"][0]["description"]
            .as_str()
            .unwrap()
            .contains("adapter"),
        "body={}",
        body
    );
}

#[tokio::test]
async fn milestones_agent_filter_does_not_collapse_its_own_facet() {
    // Facets are computed before the categorical filters precisely so the
    // sibling chips keep a usable count while one is selected. If this
    // regresses, selecting `alice` would report `bob: 0` and the UI's filter
    // chips become dead ends.
    let (status, body) = get_body(router().await, "/api/v1/history/milestones?agent=alice").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["total"], 1, "body={}", body);

    let agents = facet_pairs(&body, "agents");
    assert_eq!(
        agents.iter().find(|(v, _)| v == "bob").map(|(_, c)| *c),
        Some(1),
        "bob's chip must keep its count while alice is selected; body={}",
        body
    );
    assert_eq!(
        agents.len() as u64,
        FIXTURE_MILESTONES,
        "every agent should still be offered; body={}",
        body
    );
}

#[tokio::test]
async fn milestones_agent_filter_is_case_insensitive() {
    let (_, body) = get_body(router().await, "/api/v1/history/milestones?agent=ALICE").await;
    assert_eq!(body["total"], 1, "body={}", body);
}

#[tokio::test]
async fn milestones_paginate_without_changing_total() {
    let (_, page1) = get_body(
        router().await,
        "/api/v1/history/milestones?limit=1&offset=0",
    )
    .await;
    let (_, page2) = get_body(
        router().await,
        "/api/v1/history/milestones?limit=1&offset=1",
    )
    .await;
    assert_eq!(page1["total"], FIXTURE_MILESTONES);
    assert_eq!(
        page2["total"], FIXTURE_MILESTONES,
        "total describes the set, not the page"
    );
    assert_eq!(page1["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_ne!(
        page1["items"][0]["commit"], page2["items"][0]["commit"],
        "pages must not overlap"
    );
}

// ---------------------------------------------------------------------------
// /api/v1/history/rollup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rollup_groups_by_agent_and_intent() {
    let (status, body) = get_body(router().await, "/api/v1/history/rollup").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);

    // Facet counts are weighted by commit_count, not row count. The engine's
    // own bookkeeping commits inflate the absolutes, so assert against the
    // agents this fixture actually drives: alice made 2 Refine + 1 Checkpoint,
    // bob 1 of each.
    let agents = facet_pairs(&body, "agents");
    assert_eq!(
        agents.iter().find(|(v, _)| v == "alice").map(|(_, c)| *c),
        Some(3),
        "body={}",
        body
    );
    assert_eq!(
        agents.iter().find(|(v, _)| v == "bob").map(|(_, c)| *c),
        Some(2),
        "body={}",
        body
    );

    // The rollup splits an agent's commits across intent categories.
    let alice_rows: Vec<&serde_json::Value> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["agent"] == "alice")
        .collect();
    let by_intent = |intent: &str| -> i64 {
        alice_rows
            .iter()
            .filter(|r| r["intent"] == intent)
            .map(|r| r["commits"].as_i64().unwrap())
            .sum()
    };
    assert_eq!(by_intent("Refine"), 2, "body={}", body);
    assert_eq!(by_intent("Checkpoint"), 1, "body={}", body);
}

#[tokio::test]
async fn rollup_totals_describe_the_filtered_set_not_the_page() {
    let (_, all) = get_body(router().await, "/api/v1/history/rollup").await;
    let full_total = all["totals"]["commits"].as_i64().unwrap();

    let (_, paged) = get_body(router().await, "/api/v1/history/rollup?limit=1").await;
    assert_eq!(paged["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        paged["totals"]["commits"].as_i64().unwrap(),
        full_total,
        "a one-row page must still sum the whole filtered set"
    );
}

#[tokio::test]
async fn rollup_agent_filter_restricts_totals() {
    let (status, body) = get_body(router().await, "/api/v1/history/rollup?agent=bob").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    // bob: one Refine + one Checkpoint.
    assert_eq!(body["totals"]["commits"], 2, "body={}", body);
}

#[tokio::test]
async fn rollup_date_range_excludes_everything_outside_it() {
    let (_, body) = get_body(
        router().await,
        "/api/v1/history/rollup?from=1999-01-01&to=1999-12-31",
    )
    .await;
    assert_eq!(body["total"], 0, "body={}", body);
    assert_eq!(body["totals"]["commits"], 0, "body={}", body);
}

// ---------------------------------------------------------------------------
// /api/v1/commits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commits_marks_milestone_commits_as_on_spine() {
    let (status, body) = get_body(router().await, "/api/v1/commits").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);

    // Only Checkpoint commits are pinned. This is also the regression guard
    // for the endpoint refreshing the distilled tables itself: without that,
    // a store whose extractor has never run reports on_spine = 0 here.
    assert_eq!(body["on_spine"], FIXTURE_MILESTONES, "body={}", body);
    let pinned: Vec<&serde_json::Value> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["on_spine"] == true)
        .collect();
    assert_eq!(pinned.len() as u64, FIXTURE_MILESTONES, "body={}", body);
    for c in &pinned {
        assert_eq!(c["intent"], "Checkpoint", "body={}", body);
    }
    for who in ["alice", "bob"] {
        assert!(
            pinned.iter().any(|c| c["agent"] == who),
            "{}'s checkpoint should be pinned; body={}",
            who,
            body
        );
    }
}

#[tokio::test]
async fn commits_on_spine_counts_the_filtered_set() {
    // `on_spine` is paired against `total` in the UI's summary line, so the
    // two must be filtered identically. Counting `on_spine` over the
    // pre-filter set would report "2 of 1 are pinned".
    let (_, body) = get_body(router().await, "/api/v1/commits?q=widen").await;
    let total = body["total"].as_u64().unwrap();
    let on_spine = body["on_spine"].as_u64().unwrap();
    assert_eq!(total, 1, "body={}", body);
    assert!(
        on_spine <= total,
        "on_spine ({}) must not exceed total ({}); body={}",
        on_spine,
        total,
        body
    );
    assert_eq!(on_spine, 0, "the matched Refine commit is not pinned");
}

#[tokio::test]
async fn commits_milestone_filter_selects_each_side() {
    let (_, pinned) = get_body(router().await, "/api/v1/commits?milestone=1").await;
    let (_, unpinned) = get_body(router().await, "/api/v1/commits?milestone=0").await;

    assert_eq!(pinned["total"], FIXTURE_MILESTONES, "body={}", pinned);
    assert!(
        pinned["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["on_spine"] == true),
        "body={}",
        pinned
    );
    assert!(
        unpinned["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["on_spine"] == false),
        "body={}",
        unpinned
    );
    // Every commit is on exactly one side of the split.
    let scanned = pinned["scanned"].as_u64().unwrap();
    assert_eq!(
        pinned["total"].as_u64().unwrap() + unpinned["total"].as_u64().unwrap(),
        scanned,
        "the two sides must partition the walk"
    );
}

#[tokio::test]
async fn commits_walk_reports_scan_window_honestly() {
    let (_, capped) = get_body(router().await, "/api/v1/commits?scan=2").await;
    assert_eq!(capped["scanned"], 2, "body={}", capped);
    assert_eq!(
        capped["capped"], true,
        "a walk cut short must say so; body={}",
        capped
    );

    let (_, full) = get_body(router().await, "/api/v1/commits?scan=1000").await;
    assert_eq!(
        full["capped"], false,
        "a walk that drained the frontier is not capped; body={}",
        full
    );
}

#[tokio::test]
async fn commits_reports_the_rollups_own_total_for_comparison() {
    // `distilled` is what lets the UI say "N commits are no longer reachable".
    // With nothing pruned, the walk and the rollup must agree.
    let (_, body) = get_body(router().await, "/api/v1/commits?scan=1000").await;
    assert_eq!(
        body["distilled"].as_i64().unwrap(),
        body["scanned"].as_i64().unwrap(),
        "nothing is unreachable in a fresh store; body={}",
        body
    );
}

#[tokio::test]
async fn commits_agent_filter_narrows_results() {
    let (_, body) = get_body(router().await, "/api/v1/commits?agent=bob").await;
    assert_eq!(body["total"], 2, "body={}", body);
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["agent"] == "bob"),
        "body={}",
        body
    );
}

// ---------------------------------------------------------------------------
// /api/v1/feedback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feedback_lists_entries_with_verdict_and_author_facets() {
    let (status, body) = get_body(router().await, "/api/v1/feedback").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["total"], 3, "body={}", body);

    let verdicts = facet_pairs(&body, "verdicts");
    assert_eq!(
        verdicts
            .iter()
            .find(|(v, _)| v == "Useful")
            .map(|(_, c)| *c),
        Some(2),
        "body={}",
        body
    );
    let authors = facet_pairs(&body, "authors");
    assert_eq!(
        authors.iter().find(|(v, _)| v == "alice").map(|(_, c)| *c),
        Some(2),
        "body={}",
        body
    );
}

#[tokio::test]
async fn feedback_flags_lapsed_entries_rather_than_hiding_them() {
    let (_, body) = get_body(router().await, "/api/v1/feedback").await;
    let items = body["items"].as_array().unwrap();
    let expired: Vec<&serde_json::Value> = items.iter().filter(|e| e["expired"] == true).collect();
    assert_eq!(
        expired.len(),
        1,
        "the lapsed entry must still be listed; body={}",
        body
    );
    assert_eq!(expired[0]["entry_id"], "fb-3", "body={}", body);
}

#[tokio::test]
async fn feedback_search_covers_query_symbol_author_and_note() {
    // "entry point" appears only in fb-1's note.
    let (_, by_note) = get_body(router().await, "/api/v1/feedback?q=entry%20point").await;
    assert_eq!(by_note["total"], 1, "body={}", by_note);
    assert_eq!(by_note["items"][0]["entry_id"], "fb-1");

    // The query text is shared by two entries.
    let (_, by_query) = get_body(router().await, "/api/v1/feedback?q=charge%20a%20card").await;
    assert_eq!(by_query["total"], 2, "body={}", by_query);
}

#[tokio::test]
async fn feedback_verdict_filter_selects_one_kind() {
    let (_, body) = get_body(router().await, "/api/v1/feedback?verdict=noisy").await;
    assert_eq!(body["total"], 1, "body={}", body);
    assert_eq!(body["items"][0]["verdict"], "Noisy", "body={}", body);
}

#[tokio::test]
async fn feedback_symbol_filter_matches_a_qname_substring() {
    let (_, body) = get_body(router().await, "/api/v1/feedback?symbol=charge").await;
    assert_eq!(body["total"], 1, "body={}", body);
    assert_eq!(body["items"][0]["symbol_qname"], "pay.charge_card");
}

// ---------------------------------------------------------------------------
// /api/v1/index-health
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_health_reports_the_asg_symbol_count() {
    let (status, body) = get_body(router().await, "/api/v1/index-health").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["symbols"]["asg"], 4, "body={}", body);
    assert_eq!(body["ref_name"], "main", "body={}", body);
}

// ---------------------------------------------------------------------------
// /api/v1/scorecard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scorecard_returns_the_full_cli_shape() {
    let (status, body) = get_body(router().await, "/api/v1/scorecard").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);

    for key in [
        "capability_scores",
        "scores",
        "data_quality",
        "details",
        "token_economy",
    ] {
        assert!(body.get(key).is_some(), "missing {}; body={}", key, body);
    }
    for dim in [
        "truth",
        "feedback",
        "change",
        "uncertainty",
        "workflow",
        "overall",
    ] {
        let v = body["capability_scores"][dim]
            .as_u64()
            .unwrap_or_else(|| panic!("dimension {} not a number; body={}", dim, body));
        assert!(v <= 100, "{} out of range: {}", dim, v);
    }
    assert_eq!(body["details"]["total_symbols"], 4, "body={}", body);
    assert_eq!(body["details"]["owned_symbols"], 1, "body={}", body);
    assert_eq!(body["details"]["feedback_entries"], 3, "body={}", body);
    assert_eq!(
        body["details"]["ctx_tagged_ledger_entries"], 1,
        "body={}",
        body
    );
}

#[tokio::test]
async fn scorecard_token_economy_compares_index_against_source() {
    let (_, body) = get_body(router().await, "/api/v1/scorecard").await;
    let te = &body["token_economy"];
    assert!(
        te["structured_tokens"].as_u64().unwrap() > 0,
        "body={}",
        body
    );
    assert!(
        te["source_read_tokens_est"].as_u64().unwrap() > te["structured_tokens"].as_u64().unwrap(),
        "reading source should cost more than the index; body={}",
        body
    );
}

#[tokio::test]
async fn scorecard_drill_down_lists_gap_symbols() {
    let (status, body) =
        get_body(router().await, "/api/v1/scorecard?drill_down=truth&limit=2").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);

    let drill = &body["drill_down"];
    assert_eq!(drill["dimension"], "truth", "body={}", body);
    let shown = drill["shown"].as_u64().unwrap();
    let total_gaps = drill["total_gaps"].as_u64().unwrap();
    assert!(shown <= 2, "limit not honored; body={}", body);
    assert_eq!(
        drill["omitted"].as_u64().unwrap(),
        total_gaps - shown,
        "omitted must account for the truncation; body={}",
        body
    );
    assert_eq!(
        drill["gap_symbols"].as_array().unwrap().len() as u64,
        shown,
        "body={}",
        body
    );
}

#[tokio::test]
async fn scorecard_path_filter_that_matches_nothing_says_so() {
    let (status, body) = get_body(router().await, "/api/v1/scorecard?paths=no/such/dir/**").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert!(
        body["note"]
            .as_str()
            .unwrap_or_default()
            .contains("path filter"),
        "body={}",
        body
    );
    assert_eq!(body["capability_scores"]["overall"], 0, "body={}", body);
}

#[tokio::test]
async fn scorecard_path_filter_scopes_the_symbol_set() {
    let (_, body) = get_body(router().await, "/api/v1/scorecard?paths=pay.py").await;
    assert_eq!(body["details"]["total_symbols"], 1, "body={}", body);
    assert_eq!(body["details"]["owned_symbols"], 1, "body={}", body);
}
