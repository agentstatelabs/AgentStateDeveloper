//! `asd-serve` — HTTP server for the ASD Lens.
//!
//! Reads an ASD SQLite database and exposes a read-only JSON API plus a static
//! file fallback for the built Lens UI.
//!
//! Env:
//! - `ASD_DB` — path to SQLite db (default: `./.asd-state.db`)
//! - `ASD_SERVE_ADDR` — bind address (default: `0.0.0.0:4120`)
//! - `ASD_AUDIT_LOG` — commercial feature (Enterprise tier). In the
//!   OSS `asd-serve` binary this path is recorded for read-only tail
//!   of logs produced by `asd-pro`, but no new events are written —
//!   the engine stays on `NullSink`.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::Engine;
use agentstatedeveloper_mcp::build_router;
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

    tracing::info!(?db_path, %addr, ?lens_dir, "starting asd-serve");

    let engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;

    // ASD_AUDIT_LOG is recognized for read-only /audit tailing so OSS
    // `asd-serve` can display logs produced by asd-pro. Writing new
    // chained events requires asd-pro.
    let audit_log_path: Option<PathBuf> = std::env::var("ASD_AUDIT_LOG")
        .ok()
        .map(PathBuf::from);
    if audit_log_path.is_some() {
        tracing::warn!(
            "ASD_AUDIT_LOG set but asd-serve is OSS — no new events \
             will be written (tamper-evident sink is a commercial \
             feature; install asd-pro). Existing logs are readable \
             via /audit."
        );
    }

    let shared = Arc::new(Mutex::new(engine));

    let app = build_router(shared, db_path, lens_dir, audit_log_path);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {}", addr))?;
    tracing::info!("listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
