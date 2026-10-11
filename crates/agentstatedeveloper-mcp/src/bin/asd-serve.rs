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
    // Logs, traces and metrics through oxtel; nothing is exported unless
    // built with the `otlp` feature and OTEL_EXPORTER_OTLP_ENDPOINT is set.
    // Text on standard output, as before: CTXone's AsdProcessPool reads the
    // "listening on host:port" line from this process's stdout to find the
    // port it was given. OXTEL_LOG_FORMAT and OXTEL_LOG_OUTPUT still win.
    let _telemetry = oxtel::builder()
        .with_default_service_name("asd-serve")
        .with_resource_attribute(oxtel::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_local_logs(oxtel::sdk::logs::LocalFormat::Text)
        .with_local_logs_output(oxtel::LogOutput::Stdout)
        .install()?;

    let db_path =
        PathBuf::from(std::env::var("ASD_DB").unwrap_or_else(|_| "./.asd-state.db".to_string()));
    let addr = std::env::var("ASD_SERVE_ADDR").unwrap_or_else(|_| "0.0.0.0:4120".to_string());
    let lens_dir = std::env::var("ASD_LENS_DIR").ok().map(PathBuf::from);
    let cors_permissive = std::env::var("ASD_CORS_PERMISSIVE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    tracing::info!(?db_path, %addr, ?lens_dir, "starting asd-serve");

    let engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;

    // ASD_AUDIT_LOG is recognized for read-only /audit tailing so OSS
    // `asd-serve` can display logs produced by asd-pro. Writing new
    // chained events requires asd-pro.
    let audit_log_path: Option<PathBuf> = std::env::var("ASD_AUDIT_LOG").ok().map(PathBuf::from);
    if audit_log_path.is_some() {
        tracing::warn!(
            "ASD_AUDIT_LOG set but asd-serve is OSS — no new events \
             will be written (tamper-evident sink is a commercial \
             feature; install asd-pro). Existing logs are readable \
             via /audit."
        );
    }

    let shared = Arc::new(Mutex::new(engine));

    let app = build_router(shared, db_path, lens_dir, audit_log_path, cors_permissive);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {}", addr))?;
    // Log the *resolved* address. Callers that asked for `:0` need this to
    // recover the OS-assigned port; logging the bind template `host:0` was
    // silently wrong for supervisors like CTXone's AsdProcessPool.
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    tracing::info!("listening on {}", bound);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM, which supervisors and
/// container runtimes send. The server finishes what's in flight and `main`
/// returns, so pending telemetry is sent.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}
