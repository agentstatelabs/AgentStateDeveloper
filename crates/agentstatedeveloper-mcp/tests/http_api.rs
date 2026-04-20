//! In-process integration test that exercises the HTTP router by opening the
//! sample-py-repo's pre-indexed SQLite db and dispatching requests via
//! `tower::ServiceExt::oneshot`. No network binding — works inside the
//! sandbox where loopback bind is denied.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::Engine;
use agentstatedeveloper_mcp::build_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::Mutex;
use tower::ServiceExt;

fn sample_db() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample-py-repo/.asd-state.db")
}

async fn router() -> axum::Router {
    let db = sample_db();
    let engine = Engine::open_sqlite(&db).expect("open sqlite");
    build_router(Arc::new(Mutex::new(engine)), db, None, None)
}

async fn get_body(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::json!({"_raw": String::from_utf8_lossy(&bytes).to_string()}));
    (status, value)
}

#[tokio::test]
async fn health_endpoint() {
    if !sample_db().exists() {
        eprintln!("skipping: {} not found — run `asd init && asd index .`", sample_db().display());
        return;
    }
    let (status, body) = get_body(router().await, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["status"], "ok");
    assert!(body["symbol_count"].as_u64().unwrap() > 0, "body={}", body);
    eprintln!("health: {}", body);
}

#[tokio::test]
async fn list_symbols_endpoint() {
    if !sample_db().exists() {
        return;
    }
    let (status, body) = get_body(router().await, "/api/v1/symbols").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    assert!(!arr.is_empty());
    // Sorted by qname
    let qnames: Vec<&str> = arr.iter().map(|s| s["qname"].as_str().unwrap()).collect();
    let mut sorted = qnames.clone();
    sorted.sort();
    assert_eq!(qnames, sorted);
    eprintln!("symbols ({}): {:?}", qnames.len(), qnames);
}

#[tokio::test]
async fn symbol_detail_endpoint() {
    if !sample_db().exists() {
        return;
    }
    let (status, body) = get_body(router().await, "/api/v1/symbols/payments.charge_card").await;
    assert_eq!(status, StatusCode::OK, "body={}", body);
    assert_eq!(body["symbol"]["qname"], "payments.charge_card");
    assert!(body["ledger"].is_array());
    eprintln!("symbol detail: {}", body);
}

#[tokio::test]
async fn symbol_not_found() {
    if !sample_db().exists() {
        return;
    }
    let (status, body) = get_body(router().await, "/api/v1/symbols/does.not.exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("not found"));
}
