//! `POST /api/v1/gc/sweep` — a policy-aware GC preview that must never mutate.
//!
//! `asd-serve` binds every interface and has no authentication, so deleting and
//! vacuuming are CLI-only. The property worth guarding is that no request to
//! this route can change the store, not merely that the happy path previews.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::Engine;
use agentstatedeveloper_mcp::build_router;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// An engine with history to reclaim: one value overwritten several times.
fn engine_with_history() -> Engine {
    let engine = Engine::open_in_memory().expect("in-memory engine");
    for n in 0..5 {
        engine
            .repo
            .set_json(
                &engine.ref_name,
                "/test/value",
                &serde_json::json!({ "n": n }),
                CommitOptions::new("alice", IntentCategory::Refine, format!("edit {n}")),
            )
            .expect("commit");
    }
    engine
}

fn router(engine: Engine) -> axum::Router {
    build_router(
        Arc::new(Mutex::new(engine)),
        PathBuf::from(":memory:"),
        None,
        None,
        true,
    )
}

async fn post(app: axum::Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/gc/sweep")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn an_empty_body_previews_under_asds_defaults() {
    let (status, body) = post(router(engine_with_history()), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dry_run"], true, "{body}");
    assert_eq!(body["mutated"], false, "{body}");
    // ASD's defaults, not AgentStateGraph's: no sparse checkpoints.
    assert_eq!(body["policy"]["keep_recent"], 100, "{body}");
    assert_eq!(body["policy"]["checkpoint_every"], 0, "{body}");
    assert_eq!(body["policy"]["keep_milestones"], true, "{body}");
    assert!(
        body["would_reclaim"]["total_objects"].as_i64().unwrap() > 0,
        "{body}"
    );
}

#[tokio::test]
async fn the_policy_is_honoured() {
    let (status, body) = post(
        router(engine_with_history()),
        serde_json::json!({ "keep_recent": 1, "checkpoint_every": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["policy"]["keep_recent"], 1, "{body}");
    assert!(
        body["would_reclaim"]["reclaimable_objects"]
            .as_i64()
            .unwrap()
            > 0,
        "keeping only the newest state must leave the overwritten values reclaimable; {body}"
    );
}

/// Asking to delete or vacuum is refused, and refusing means the store is
/// untouched — checked by counting objects before and after, not by trusting
/// the status code.
#[tokio::test]
async fn deleting_and_vacuuming_are_refused_and_change_nothing() {
    let app = router(engine_with_history());
    let (_, before) = post(app.clone(), serde_json::json!({ "keep_recent": 1 })).await;

    for req in [
        serde_json::json!({ "mutate": true }),
        serde_json::json!({ "vacuum": true }),
        serde_json::json!({ "mutate": true, "vacuum": true, "keep_recent": 1 }),
    ] {
        let (status, body) = post(app.clone(), req.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{req} -> {body}");
        assert!(
            body["error"].as_str().unwrap().contains("asd gc --sweep"),
            "the refusal must say where to go instead; {body}"
        );
    }

    let (_, after) = post(app, serde_json::json!({ "keep_recent": 1 })).await;
    assert_eq!(
        before["would_reclaim"]["total_objects"], after["would_reclaim"]["total_objects"],
        "a refused request must not have deleted anything"
    );
}

#[tokio::test]
async fn dropping_milestone_pins_is_rejected() {
    let (status, body) = post(
        router(engine_with_history()),
        serde_json::json!({ "keep_milestones": false }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("--unpin-legacy"),
        "{body}"
    );
}
