//! In-process integration tests for `GET /api/v1/events` (SSE).
//!
//! Same style as `http_lens_api.rs`: an in-memory engine + `oneshot`
//! dispatch, no network binding. The SSE body is a never-ending stream, so
//! instead of `collect()`ing it we read frames incrementally and stop once
//! the expected event shows up.
//!
//! Timing: the events poller ticks every `events::POLL_INTERVAL` (2s), so
//! each test that triggers a change waits up to a few seconds for the
//! matching frame. The connect-then-write ordering is race-free by
//! construction — `subscribe()` snapshots the baseline head before the
//! handler returns the response, so a write made after `oneshot` resolves
//! is always "new" to the poller.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, AuditEvent, Author, AuthorKind, Effect,
    EffectCategory, EffectDecl, EffectStore, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind,
};
use agentstatedeveloper_mcp::build_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Generous ceiling for "event arrives within the polling window":
/// a couple of 2s poll ticks plus CI scheduling slack.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

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
        signature: None,
        doc: None,
    }
}

fn make_entry(qname: &str, entry_id: &str, kind: LedgerKind, summary: &str) -> LedgerEntry {
    LedgerEntry {
        entry_id: entry_id.to_string(),
        symbol_id: sym_id(qname),
        kind,
        summary: summary.to_string(),
        body: None,
        author: Author {
            kind: AuthorKind::Agent,
            id: "sse-test".to_string(),
        },
        confidence: Some(0.6),
        evidence: Vec::new(),
        supersedes: Vec::new(),
        created_at: chrono::Utc::now(),
        tags: Vec::new(),
        matched_policy: None,
        role: None,
        command: None,
    }
}

/// Engine with one pre-seeded symbol (`app.main`) so qname resolution has
/// something to resolve. Returns the shared handle the tests write through.
fn fixture_engine() -> Arc<Mutex<Engine>> {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    AsgIndexStore::new(&engine.repo)
        .put_symbol(
            &engine.ref_name,
            &make_symbol("app.main", "app.py", 1),
            "sse-test",
        )
        .expect("put symbol");
    Arc::new(Mutex::new(engine))
}

fn router_for(engine: &Arc<Mutex<Engine>>, audit_log_path: Option<PathBuf>) -> axum::Router {
    build_router(
        engine.clone(),
        PathBuf::from(":memory:"),
        None,
        audit_log_path,
        true,
    )
}

async fn connect_sse(app: axum::Router) -> (StatusCode, Option<String>, Body) {
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (status, content_type, resp.into_body())
}

/// Read SSE frames until a `data:` JSON event satisfying `pred` arrives.
/// Panics (with the frames seen so far) if the stream stalls past
/// `EVENT_TIMEOUT` or ends.
async fn wait_for_event(body: &mut Body, pred: impl Fn(&Value) -> bool) -> Value {
    let mut buf = String::new();
    let mut seen: Vec<String> = Vec::new();
    loop {
        let frame = tokio::time::timeout(EVENT_TIMEOUT, body.frame())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for SSE event; seen: {:?}", seen))
            .unwrap_or_else(|| panic!("SSE stream ended; seen: {:?}", seen))
            .expect("SSE frame error");
        if let Ok(data) = frame.into_data() {
            buf.push_str(&String::from_utf8_lossy(&data));
        }
        // Consume every complete event block ("...\n\n") in the buffer.
        while let Some(pos) = buf.find("\n\n") {
            let block: String = buf.drain(..pos + 2).collect();
            for line in block.lines() {
                if let Some(json) = line.strip_prefix("data: ") {
                    let v: Value = serde_json::from_str(json)
                        .unwrap_or_else(|e| panic!("bad event JSON {:?}: {}", json, e));
                    seen.push(json.to_string());
                    if pred(&v) {
                        return v;
                    }
                }
                // Comment lines (": keep-alive") and blanks are skipped.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ledger (incl. thinking kinds) — the primary feed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_emits_ledger_event_within_polling_window() {
    let engine = fixture_engine();
    let app = router_for(&engine, None);

    let (status, content_type, mut body) = connect_sse(app).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.as_deref().unwrap_or("").starts_with("text/event-stream"),
        "content-type={:?}",
        content_type
    );

    // Write AFTER the subscription is live (baseline already snapshotted).
    {
        let g = engine.lock().await;
        AsgLedgerStore::new(&g.repo)
            .append_entry(
                &g.ref_name,
                &make_entry(
                    "app.main",
                    "e-live-1",
                    LedgerKind::Decision,
                    "Adopt idempotency keys",
                ),
                "sse-test",
            )
            .expect("append entry");
    }

    let event = wait_for_event(&mut body, |v| v["entry_id"] == "e-live-1").await;
    assert_eq!(event["kind"], "decision");
    assert_eq!(event["qname"], "app.main");
    assert_eq!(event["symbol_id"], sym_id("app.main").as_str());
    assert_eq!(event["summary"], "Adopt idempotency keys");
    assert!(event["at"].as_str().is_some(), "event={}", event);
}

#[tokio::test]
async fn sse_emits_thinking_kind_ledger_event() {
    let engine = fixture_engine();
    let app = router_for(&engine, None);
    let (_, _, mut body) = connect_sse(app).await;

    {
        let g = engine.lock().await;
        AsgLedgerStore::new(&g.repo)
            .append_entry(
                &g.ref_name,
                &make_entry(
                    "app.main",
                    "e-think-1",
                    LedgerKind::Hypothesis,
                    "Retries may double-charge",
                ),
                "sse-test",
            )
            .expect("append entry");
    }

    let event = wait_for_event(&mut body, |v| v["entry_id"] == "e-think-1").await;
    assert_eq!(event["kind"], "hypothesis");
    assert_eq!(event["summary"], "Retries may double-charge");
}

// ---------------------------------------------------------------------------
// Effect declarations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_emits_effect_declaration_event() {
    let engine = fixture_engine();
    let app = router_for(&engine, None);
    let (_, _, mut body) = connect_sse(app).await;

    {
        let g = engine.lock().await;
        let decl = EffectDecl {
            symbol_id: sym_id("app.main"),
            declared: vec![Effect::new(EffectCategory::IoNetOut)],
            transitive: Vec::new(),
            verification: None,
            confidence: None,
            runtime: None,
            matched_policy: None,
        };
        AsgEffectStore::new(&g.repo)
            .put_effects(&g.ref_name, &sym_id("app.main"), &decl, "sse-test")
            .expect("put effects");
    }

    let event = wait_for_event(&mut body, |v| v["kind"] == "effect_declared").await;
    assert_eq!(event["qname"], "app.main");
    assert_eq!(event["symbol_id"], sym_id("app.main").as_str());
    assert!(
        event["summary"].as_str().unwrap().contains("io.net.out"),
        "event={}",
        event
    );
}

// ---------------------------------------------------------------------------
// Index runs are coalesced, not spammed per-commit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_coalesces_index_commits_into_one_index_run_event() {
    let engine = fixture_engine();
    let app = router_for(&engine, None);
    let (_, _, mut body) = connect_sse(app).await;

    // put_symbol lands TWO commits ("index symbol …" + "qname index …") —
    // the stream must fold them into a single index_run event.
    {
        let g = engine.lock().await;
        AsgIndexStore::new(&g.repo)
            .put_symbol(
                &g.ref_name,
                &make_symbol("pay.charge_card", "pay.py", 10),
                "sse-test",
            )
            .expect("put symbol");
    }

    let event = wait_for_event(&mut body, |v| v["kind"] == "index_run").await;
    let summary = event["summary"].as_str().unwrap();
    assert!(summary.contains("(2 commits)"), "event={}", event);
}

// ---------------------------------------------------------------------------
// Audit JSONL feed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_emits_audit_event_from_jsonl_growth() {
    let audit_path =
        std::env::temp_dir().join(format!("asd-sse-audit-{}.jsonl", uuid::Uuid::new_v4()));
    std::fs::write(&audit_path, "").expect("create audit file");

    let engine = fixture_engine();
    let app = router_for(&engine, Some(audit_path.clone()));
    let (_, _, mut body) = connect_sse(app).await;

    // Append a line the way external writers do (JSONL, one event per line).
    let evt = AuditEvent::new("ledger.approve", "tester", "human", "approved")
        .with_subject("e-audited-1");
    let line = format!("{}\n", serde_json::to_string(&evt).unwrap());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&audit_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
        .expect("append audit line");

    let event = wait_for_event(&mut body, |v| v["kind"] == "audit").await;
    assert_eq!(event["entry_id"], "e-audited-1");
    let summary = event["summary"].as_str().unwrap();
    assert!(
        summary.contains("ledger.approve") && summary.contains("approved"),
        "event={}",
        event
    );

    let _ = std::fs::remove_file(&audit_path);
}

// ---------------------------------------------------------------------------
// Fan-out: multiple concurrent subscribers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_delivers_to_multiple_concurrent_subscribers() {
    let engine = fixture_engine();
    let app = router_for(&engine, None);

    let (status_a, _, mut body_a) = connect_sse(app.clone()).await;
    let (status_b, _, mut body_b) = connect_sse(app).await;
    assert_eq!(status_a, StatusCode::OK);
    assert_eq!(status_b, StatusCode::OK);

    {
        let g = engine.lock().await;
        AsgLedgerStore::new(&g.repo)
            .append_entry(
                &g.ref_name,
                &make_entry(
                    "app.main",
                    "e-fanout-1",
                    LedgerKind::Rationale,
                    "Both subscribers must see this",
                ),
                "sse-test",
            )
            .expect("append entry");
    }

    let (ev_a, ev_b) = tokio::join!(
        wait_for_event(&mut body_a, |v| v["entry_id"] == "e-fanout-1"),
        wait_for_event(&mut body_b, |v| v["entry_id"] == "e-fanout-1"),
    );
    assert_eq!(ev_a["kind"], "rationale");
    assert_eq!(ev_b, ev_a);
}
