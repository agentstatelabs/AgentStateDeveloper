//! In-process integration tests for `GET /api/v1/audit` filters —
//! specifically the `subject=` filter added for Plan I t-034 (approval
//! timeline / AccountabilityCard server-side lookup). The other filters
//! (`event_type`, `actor`, `outcome`) get combined-use coverage here too
//! since no prior test exercised the endpoint against a real JSONL file.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::{AuditEvent, Engine, event_types};
use agentstatedeveloper_mcp::build_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Write a small audit log:
///   0. ledger.append   subject=e-1  secondary=sym-a   (agent:bot)
///   1. ledger.approve  subject=e-1  secondary=sym-a   (human:alice)
///   2. ledger.reject   subject=e-2  secondary=sym-b   (human:bob)
///   3. ledger.withdraw subject=e-3  secondary=sym-a   (agent:bot)
fn write_fixture_log(path: &std::path::Path) {
    let events = vec![
        AuditEvent::new(
            event_types::LEDGER_APPEND,
            "bot",
            "agent",
            "awaiting-approval",
        )
        .with_subject("e-1")
        .with_secondary("sym-a"),
        AuditEvent::new(event_types::LEDGER_APPROVE, "alice", "human", "approved")
            .with_subject("e-1")
            .with_secondary("sym-a"),
        AuditEvent::new(event_types::LEDGER_REJECT, "bob", "human", "rejected")
            .with_subject("e-2")
            .with_secondary("sym-b"),
        AuditEvent::new(event_types::LEDGER_WITHDRAW, "bot", "agent", "withdrawn")
            .with_subject("e-3")
            .with_secondary("sym-a"),
    ];
    let mut out = String::new();
    for e in events {
        out.push_str(&serde_json::to_string(&e).unwrap());
        out.push('\n');
    }
    std::fs::write(path, out).expect("write audit fixture");
}

async fn router(audit_path: PathBuf) -> axum::Router {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    build_router(
        Arc::new(Mutex::new(engine)),
        PathBuf::from(":memory:"),
        None,
        Some(audit_path),
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

fn tmp_log(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "asd-audit-api-{}-{}.jsonl",
        tag,
        uuid::Uuid::new_v4()
    ))
}

#[tokio::test]
async fn audit_subject_filter_matches_subject_id() {
    let path = tmp_log("subject");
    write_fixture_log(&path);
    let (status, body) = get_body(router(path.clone()).await, "/api/v1/audit?subject=e-1").await;
    let _ = std::fs::remove_file(&path);

    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["configured"], true);
    let events = body["events"].as_array().expect("events array");
    assert_eq!(events.len(), 2, "append + approve name e-1, body={}", body);
    assert!(
        events.iter().all(|e| e["subject_id"] == "e-1"),
        "body={}",
        body
    );
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec!["ledger.append", "ledger.approve"],
        "body={}",
        body
    );
}

#[tokio::test]
async fn audit_subject_filter_matches_secondary_id() {
    let path = tmp_log("secondary");
    write_fixture_log(&path);
    let (status, body) = get_body(router(path.clone()).await, "/api/v1/audit?subject=sym-a").await;
    let _ = std::fs::remove_file(&path);

    assert_eq!(status, StatusCode::OK, "body={}", body);
    let events = body["events"].as_array().expect("events array");
    // append + approve + withdraw carry secondary_id=sym-a; the reject
    // (sym-b) must be excluded.
    assert_eq!(events.len(), 3, "body={}", body);
    assert!(
        events.iter().all(|e| e["secondary_id"] == "sym-a"),
        "body={}",
        body
    );
}

#[tokio::test]
async fn audit_subject_filter_composes_with_other_filters() {
    let path = tmp_log("compose");
    write_fixture_log(&path);
    let app = router(path.clone()).await;

    // subject + event_type substring: only the approve of e-1.
    let (status, body) = get_body(
        app.clone(),
        "/api/v1/audit?subject=e-1&event_type=ledger.approve",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "body={}", body);
    assert_eq!(events[0]["actor_id"], "alice");
    assert_eq!(events[0]["outcome"], "approved");

    // subject + actor that never touched it: empty, not an error.
    let (status, body) = get_body(app.clone(), "/api/v1/audit?subject=e-1&actor=bob").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["count"], 0, "body={}", body);

    // Unmatched subject: empty, not an error.
    let (status, body) = get_body(app, "/api/v1/audit?subject=nope").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["count"], 0, "body={}", body);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn audit_without_subject_is_unfiltered() {
    let path = tmp_log("nofilter");
    write_fixture_log(&path);
    let (status, body) = get_body(router(path.clone()).await, "/api/v1/audit").await;
    let _ = std::fs::remove_file(&path);

    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["count"], 4, "body={}", body);
}
