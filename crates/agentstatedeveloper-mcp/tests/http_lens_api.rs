//! In-process integration tests for the Plan T Lens endpoints:
//! `/api/v1/search`, `/api/v1/symbols/{qname}/graph`,
//! `/api/v1/effects/overview`, and `/api/v1/timeline`.
//!
//! Unlike `http_api.rs` (which opens the pre-indexed sample-py-repo DB and
//! self-skips when it's absent), these tests build a small hand-crafted
//! fixture in an in-memory engine — symbols, call edges, effect decls
//! (with `propagate_transitive` run for real), and ledger entries — so
//! they always run. Requests are dispatched via
//! `tower::ServiceExt::oneshot`; no network binding.
//!
//! Fixture call graph (caller → callee):
//!
//!   app.main ──► pay.charge_card ──► net.post   [declares io.net.out]
//!       │               │
//!       └───────────────┴─────────► util.log    [declares log]

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Effect, EffectCategory,
    EffectDecl, Engine, IndexStore, LedgerEntry, LedgerKind, LedgerStore, Position, Symbol,
    SymbolKind, paths, propagate_transitive,
};
use agentstatedeveloper_mcp::build_router;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{TimeZone, Utc};
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
        doc: None,
    }
}

fn put_edges(engine: &Engine, qname: &str, callers: &[&str], callees: &[&str]) {
    let id = sym_id(qname);
    let caller_ids: Vec<String> = callers.iter().map(|q| sym_id(q)).collect();
    let callee_ids: Vec<String> = callees.iter().map(|q| sym_id(q)).collect();
    for (path, key, ids) in [
        (paths::callers_path(&id), "callers", caller_ids),
        (paths::callees_path(&id), "callees", callee_ids),
    ] {
        let opts = CommitOptions::new(
            "lens-test",
            IntentCategory::Refine,
            format!("test {} for {}", key, id),
        );
        engine
            .repo
            .set_json(
                &engine.ref_name,
                &path,
                &serde_json::json!({ key: ids }),
                opts,
            )
            .expect("set edges");
    }
}

fn put_decl(engine: &Engine, qname: &str, declared: Vec<EffectCategory>) {
    use agentstatedeveloper_core::EffectStore;
    let store = AsgEffectStore::new(&engine.repo);
    let decl = EffectDecl {
        symbol_id: sym_id(qname),
        declared: declared.into_iter().map(Effect::new).collect(),
        transitive: Vec::new(),
        verification: None,
        confidence: None,
        runtime: None,
        matched_policy: None,
    };
    store
        .put_effects(&engine.ref_name, &sym_id(qname), &decl, "lens-test")
        .expect("put effects");
}

fn put_ledger(
    engine: &Engine,
    qname: &str,
    entry_id: &str,
    kind: LedgerKind,
    summary: &str,
    day: u32,
) {
    let store = AsgLedgerStore::new(&engine.repo);
    let entry = LedgerEntry {
        entry_id: entry_id.to_string(),
        symbol_id: sym_id(qname),
        kind,
        summary: summary.to_string(),
        body: None,
        author: Author {
            kind: AuthorKind::Agent,
            id: "lens-test".to_string(),
        },
        confidence: Some(0.6),
        evidence: Vec::new(),
        supersedes: Vec::new(),
        created_at: Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap(),
        tags: Vec::new(),
        matched_policy: None,
        role: None,
        command: None,
    };
    store
        .append_entry(&engine.ref_name, &entry, "lens-test")
        .expect("append ledger entry");
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
                "lens-test",
            )
            .expect("put symbol");
    }

    put_edges(&engine, "app.main", &[], &["pay.charge_card", "util.log"]);
    put_edges(
        &engine,
        "pay.charge_card",
        &["app.main"],
        &["net.post", "util.log"],
    );
    put_edges(&engine, "net.post", &["pay.charge_card"], &[]);
    put_edges(&engine, "util.log", &["app.main", "pay.charge_card"], &[]);

    put_decl(&engine, "app.main", vec![]);
    put_decl(&engine, "pay.charge_card", vec![]);
    put_decl(&engine, "net.post", vec![EffectCategory::IoNetOut]);
    put_decl(&engine, "util.log", vec![EffectCategory::Log]);

    // Populate EffectDecl.transitive with the real machinery so the
    // /effects/overview blast radii come from stored propagation data.
    let effect_store = AsgEffectStore::new(&engine.repo);
    let all_ids: Vec<String> = ["app.main", "pay.charge_card", "net.post", "util.log"]
        .iter()
        .map(|q| sym_id(q))
        .collect();
    propagate_transitive(&index_store, &effect_store, &engine.ref_name, &all_ids)
        .expect("propagate transitive effects");

    // Two ledger entries a day apart — one "classic" kind, one Plan G
    // thinking kind — so /timeline ordering and kinds= slicing are testable.
    put_ledger(
        &engine,
        "pay.charge_card",
        "e-decision-1",
        LedgerKind::Decision,
        "Use idempotency keys for card charges",
        1,
    );
    put_ledger(
        &engine,
        "app.main",
        "e-hypothesis-1",
        LedgerKind::Hypothesis,
        "Retries may double-charge without idempotency",
        2,
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

// ---------------------------------------------------------------------------
// /api/v1/search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_returns_ranked_symbols_with_score_and_why() {
    let (status, body) = get_body(router().await, "/api/v1/search?q=charge%20card").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let arr = body.as_array().expect("array response");
    assert!(!arr.is_empty(), "expected at least one hit, body={}", body);

    let top = &arr[0];
    assert_eq!(top["qname"], "pay.charge_card");
    assert_eq!(top["name"], "charge_card");
    assert_eq!(top["kind"], "function");
    assert_eq!(top["language"], "python");
    assert_eq!(top["file"], "pay.py");
    assert_eq!(top["line"], 10);
    assert!(top["score"].as_f64().unwrap() > 0.0, "body={}", body);
    let why: Vec<&str> = top["why"]
        .as_array()
        .expect("why array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        why.iter().any(|r| r.starts_with("name:")),
        "expected a name: match reason, got {:?}",
        why
    );
}

#[tokio::test]
async fn search_respects_kind_filter() {
    // All fixture symbols are functions — a class filter must return nothing.
    let (status, body) = get_body(router().await, "/api/v1/search?q=charge&kind=class").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body.as_array().map(Vec::len), Some(0), "body={}", body);
}

#[tokio::test]
async fn search_rejects_empty_query() {
    let (status, body) = get_body(router().await, "/api/v1/search?q=%20").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body={}", body);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("must not be empty"),
        "body={}",
        body
    );
}

// ---------------------------------------------------------------------------
// /api/v1/symbols/{qname}/graph
// ---------------------------------------------------------------------------

fn node_qnames(body: &serde_json::Value) -> Vec<String> {
    body["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|n| n["qname"].as_str().unwrap().to_string())
        .collect()
}

fn has_link(body: &serde_json::Value, source_qname: &str, target_qname: &str) -> bool {
    let (s, t) = (sym_id(source_qname), sym_id(target_qname));
    body["links"]
        .as_array()
        .expect("links array")
        .iter()
        .any(|l| l["source"] == s.as_str() && l["target"] == t.as_str())
}

#[tokio::test]
async fn graph_default_is_one_hop_both_directions() {
    let (status, body) = get_body(router().await, "/api/v1/symbols/pay.charge_card/graph").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["root"], "pay.charge_card");
    assert_eq!(body["hops"], 1);
    assert_eq!(body["direction"], "both");
    assert_eq!(body["truncated"], false);

    let mut qnames = node_qnames(&body);
    qnames.sort();
    assert_eq!(
        qnames,
        vec!["app.main", "net.post", "pay.charge_card", "util.log"],
        "body={}",
        body
    );
    // Links always point caller → callee, whichever direction found them.
    assert!(
        has_link(&body, "app.main", "pay.charge_card"),
        "body={}",
        body
    );
    assert!(
        has_link(&body, "pay.charge_card", "net.post"),
        "body={}",
        body
    );
    assert!(
        has_link(&body, "pay.charge_card", "util.log"),
        "body={}",
        body
    );

    // Node shape: stable id + render fields.
    let node = body["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["qname"] == "pay.charge_card")
        .expect("root node present");
    assert_eq!(node["id"], sym_id("pay.charge_card").as_str());
    assert_eq!(node["kind"], "function");
    assert_eq!(node["file"], "pay.py");
    assert_eq!(node["module"], "pay");
}

#[tokio::test]
async fn graph_callees_only_walks_down_two_hops() {
    let (status, body) = get_body(
        router().await,
        "/api/v1/symbols/app.main/graph?hops=2&direction=callees",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={}", body);

    let mut qnames = node_qnames(&body);
    qnames.sort();
    assert_eq!(
        qnames,
        vec!["app.main", "net.post", "pay.charge_card", "util.log"],
        "body={}",
        body
    );
    // net.post is only reachable at hop 2 through pay.charge_card.
    assert!(
        has_link(&body, "pay.charge_card", "net.post"),
        "body={}",
        body
    );
    // No links deduped away: app.main→util.log (hop 1) and
    // pay.charge_card→util.log (hop 2) are distinct edges.
    assert!(has_link(&body, "app.main", "util.log"), "body={}", body);
    assert!(
        has_link(&body, "pay.charge_card", "util.log"),
        "body={}",
        body
    );
}

#[tokio::test]
async fn graph_callers_only_excludes_callees() {
    let (status, body) = get_body(
        router().await,
        "/api/v1/symbols/net.post/graph?hops=3&direction=callers",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let mut qnames = node_qnames(&body);
    qnames.sort();
    // Upward chain only: net.post ← pay.charge_card ← app.main. util.log
    // is a sibling callee and must NOT appear.
    assert_eq!(
        qnames,
        vec!["app.main", "net.post", "pay.charge_card"],
        "body={}",
        body
    );
    assert_eq!(body["truncated"], false);
}

#[tokio::test]
async fn graph_validates_params_and_symbol() {
    let app = router().await;
    let (status, _) = get_body(app.clone(), "/api/v1/symbols/app.main/graph?hops=0").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get_body(app.clone(), "/api/v1/symbols/app.main/graph?hops=4").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = get_body(
        app.clone(),
        "/api/v1/symbols/app.main/graph?direction=sideways",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("direction"),
        "body={}",
        body
    );
    let (status, _) = get_body(app, "/api/v1/symbols/does.not.exist/graph").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// /api/v1/effects/overview
// ---------------------------------------------------------------------------

#[tokio::test]
async fn effects_overview_counts_declarers_and_ranks_blast_radius() {
    let (status, body) = get_body(router().await, "/api/v1/effects/overview").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let rows = body.as_array().expect("array response");
    assert_eq!(
        rows.len(),
        2,
        "one row per declared category, body={}",
        body
    );

    let row = |effect: &str| -> &serde_json::Value {
        rows.iter()
            .find(|r| r["effect"] == effect)
            .unwrap_or_else(|| panic!("row for {} missing in {}", effect, body))
    };

    // io.net.out: declared only by net.post; pay.charge_card and app.main
    // inherit it transitively → blast radius 2.
    let net = row("io.net.out");
    assert_eq!(net["symbol_count"], 1);
    let top = net["top_symbols"].as_array().unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0]["qname"], "net.post");
    assert_eq!(top[0]["blast_radius"], 2, "body={}", body);

    // log: declared only by util.log; both direct callers inherit it.
    let log = row("log");
    assert_eq!(log["symbol_count"], 1);
    assert_eq!(log["top_symbols"][0]["qname"], "util.log");
    assert_eq!(log["top_symbols"][0]["blast_radius"], 2, "body={}", body);
}

#[tokio::test]
async fn effects_overview_respects_limit() {
    let (status, body) = get_body(router().await, "/api/v1/effects/overview?limit=1").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body.as_array().map(Vec::len), Some(1), "body={}", body);
}

// ---------------------------------------------------------------------------
// /api/v1/timeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeline_merges_ledger_and_thinking_newest_first() {
    let (status, body) = get_body(router().await, "/api/v1/timeline").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let feed = body.as_array().expect("array response");
    assert_eq!(feed.len(), 2, "body={}", body);

    // Newest first: the (thinking-kind) hypothesis was created a day later.
    assert_eq!(feed[0]["kind"], "hypothesis");
    assert_eq!(feed[0]["entry_id"], "e-hypothesis-1");
    assert_eq!(feed[0]["qname"], "app.main");
    assert_eq!(feed[0]["symbol_id"], sym_id("app.main").as_str());
    assert_eq!(
        feed[0]["summary"],
        "Retries may double-charge without idempotency"
    );
    assert!(feed[0]["at"].as_str().unwrap().starts_with("2026-01-02"));

    assert_eq!(feed[1]["kind"], "decision");
    assert_eq!(feed[1]["qname"], "pay.charge_card");
    assert!(feed[1]["at"].as_str().unwrap().starts_with("2026-01-01"));
}

#[tokio::test]
async fn timeline_kinds_filter_and_limit() {
    let app = router().await;

    let (status, body) = get_body(app.clone(), "/api/v1/timeline?kinds=hypothesis").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let feed = body.as_array().unwrap();
    assert_eq!(feed.len(), 1, "body={}", body);
    assert_eq!(feed[0]["kind"], "hypothesis");

    // Unknown-only kinds filter returns empty, not the whole feed.
    let (status, body) = get_body(app.clone(), "/api/v1/timeline?kinds=bogus").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body.as_array().map(Vec::len), Some(0), "body={}", body);

    // limit=1 keeps the newest entry.
    let (status, body) = get_body(app, "/api/v1/timeline?limit=1").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let feed = body.as_array().unwrap();
    assert_eq!(feed.len(), 1);
    assert_eq!(feed[0]["entry_id"], "e-hypothesis-1");
}
