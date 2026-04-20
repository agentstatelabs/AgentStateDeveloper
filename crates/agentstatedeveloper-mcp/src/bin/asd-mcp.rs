//! `asd-mcp` — AgentStateDeveloper MCP stdio server.
//!
//! Opens an ASD SQLite database and serves the MCP tool surface over stdio.
//! MCP uses stdout for protocol frames, so all logging is routed to stderr.
//!
//! Env:
//! - `ASD_DB` — path to SQLite db (default: `./.asd-state.db`)
//! - `ASD_POLICY` — optional path to a policy JSON file. When set, the
//!   engine's `PolicyGate` is swapped to a `FilePolicyGate` loaded from that
//!   file. Matches the `asd` CLI contract.
//! - `ASD_AUDIT_LOG` — optional path to a JSONL audit log file. When set,
//!   the engine's `AuditSink` is swapped from `NullSink` to a
//!   `JsonlFileSink` appending one event per line. Matches the `asd` CLI
//!   `--audit-log` / `ASD_AUDIT_LOG` contract.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::Engine;
use agentstatedeveloper_mcp::AsdMcpServer;
use anyhow::{Context, Result};
use rmcp::ServiceExt;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    // Write all tracing output to stderr — stdout is reserved for MCP frames.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let db_path = PathBuf::from(
        std::env::var("ASD_DB").unwrap_or_else(|_| "./.asd-state.db".to_string()),
    );

    tracing::info!(?db_path, "starting asd-mcp stdio server");

    let mut engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;

    // Optional policy file. Fail loudly if set but unloadable — we never want
    // a configured-but-silent permissive gate.
    if let Ok(policy_path) = std::env::var("ASD_POLICY") {
        let path = PathBuf::from(&policy_path);
        engine
            .load_policy_file(&path)
            .with_context(|| format!("failed to load ASD_POLICY policy file at {}", path.display()))?;
        tracing::info!(policy = %path.display(), "loaded ASD policy file");
    }

    // Optional audit log. Same fail-loudly semantics as ASD_POLICY — if the
    // operator configured a forensic sink, a silent fallback to NullSink
    // would be worse than crashing on startup.
    let mut audit_log_path: Option<PathBuf> = None;
    if let Ok(audit_path) = std::env::var("ASD_AUDIT_LOG") {
        let path = PathBuf::from(&audit_path);
        engine
            .set_audit_log_file(&path)
            .with_context(|| format!("failed to open ASD_AUDIT_LOG audit log at {}", path.display()))?;
        tracing::info!(audit_log = %path.display(), "loaded ASD audit log");
        audit_log_path = Some(path);
    }

    let shared = Arc::new(Mutex::new(engine));

    let server = AsdMcpServer::with_audit_log(shared, db_path.clone(), audit_log_path);
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    service.waiting().await?;
    tracing::info!("asd-mcp shut down");
    Ok(())
}
