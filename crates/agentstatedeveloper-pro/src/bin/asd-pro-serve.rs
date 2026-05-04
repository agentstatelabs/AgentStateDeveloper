//! `asd-pro-serve` — HTTP server for the ASD Lens (Team + Enterprise tier).
//!
//! Drop-in replacement for `asd-serve` that installs the two commercial overrides:
//! - [`JsonlFileSink`]: hash-chained audit events when `ASD_AUDIT_LOG` is set.
//! - [`RatifyOpsImpl`]: real ledger approve/reject/withdraw via the Lens UI.
//!
//! Env: same as `asd-serve` — `ASD_DB`, `ASD_SERVE_ADDR`, `ASD_AUDIT_LOG`,
//! `ASD_LENS_DIR`, `ASD_POLICY`.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_audit_pro::JsonlFileSink;
use agentstatedeveloper_core::Engine;
use agentstatedeveloper_mcp::build_router;
use agentstatedeveloper_ratify::RatifyOpsImpl;
use anyhow::{Context, Result};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tower_http=info")),
        )
        .init();

    let db_path = PathBuf::from(
        std::env::var("ASD_DB").unwrap_or_else(|_| "./.asd-state.db".to_string()),
    );
    let addr = std::env::var("ASD_SERVE_ADDR").unwrap_or_else(|_| "0.0.0.0:4120".to_string());
    let lens_dir = std::env::var("ASD_LENS_DIR").ok().map(PathBuf::from);
    let cors_permissive = std::env::var("ASD_CORS_PERMISSIVE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    tracing::info!(?db_path, %addr, ?lens_dir, "starting asd-pro-serve");

    let mut engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;

    if let Ok(policy_path) = std::env::var("ASD_POLICY") {
        let path = PathBuf::from(&policy_path);
        engine
            .load_policy_file(&path)
            .with_context(|| format!("failed to load ASD_POLICY at {}", path.display()))?;
        tracing::info!(policy = %path.display(), "loaded ASD policy file");
    }

    let audit_log_path: Option<PathBuf> = std::env::var("ASD_AUDIT_LOG").ok().map(PathBuf::from);
    if let Some(ref path) = audit_log_path {
        engine.set_audit_sink(Arc::new(JsonlFileSink::new(path.clone())));
        tracing::info!(audit_log = %path.display(), "hash-chained audit sink installed");
    }

    engine.set_ratify_ops(Arc::new(RatifyOpsImpl));
    tracing::info!("ratify ops installed (Team tier)");

    let shared = Arc::new(Mutex::new(engine));
    let app = build_router(shared, db_path, lens_dir, audit_log_path, cors_permissive);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {}", addr))?;
    tracing::info!("listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
