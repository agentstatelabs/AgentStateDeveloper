//! `asd-serve` — HTTP server for the ASD Lens.
//!
//! Reads an ASD SQLite database and exposes a read-only JSON API plus a static
//! file fallback for the built Lens UI.
//!
//! Env:
//! - `ASD_DB` — path to SQLite db (default: `./.asd-state.db`)
//! - `ASD_SERVE_ADDR` — bind address (default: `0.0.0.0:4120`)
//! - `ASD_AUDIT_LOG` — optional path to a JSONL audit log file. When set,
//!   the engine's `AuditSink` is swapped from `NullSink` to a
//!   `JsonlFileSink` appending one event per line. Matches the `asd` CLI
//!   `--audit-log` / `ASD_AUDIT_LOG` contract.

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

    let mut engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;

    // Optional audit log — fail loudly if set but unloadable so a configured
    // forensic sink never silently falls back to NullSink.
    if let Ok(audit_path) = std::env::var("ASD_AUDIT_LOG") {
        let path = PathBuf::from(&audit_path);
        engine
            .set_audit_log_file(&path)
            .with_context(|| format!("failed to open ASD_AUDIT_LOG audit log at {}", path.display()))?;
        tracing::info!(audit_log = %path.display(), "loaded ASD audit log");
    }

    let shared = Arc::new(Mutex::new(engine));

    let app = build_router(shared, db_path, lens_dir);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {}", addr))?;
    tracing::info!("listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
