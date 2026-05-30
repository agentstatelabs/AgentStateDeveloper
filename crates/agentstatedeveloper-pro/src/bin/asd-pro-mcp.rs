//! `asd-pro-mcp` — AgentStateDeveloper MCP stdio server (Team + Enterprise tier).
//!
//! Drop-in replacement for `asd-mcp` that installs the two commercial overrides:
//! - [`JsonlFileSink`]: hash-chained audit events when `ASD_AUDIT_LOG` is set.
//! - [`RatifyOpsImpl`]: real ledger approve/reject/withdraw.
//!
//! Env: same as `asd-mcp` — `ASD_DB`, `ASD_POLICY`, `ASD_AUDIT_LOG`.

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_audit_pro::JsonlFileSink;
use agentstatedeveloper_core::Engine;
use agentstatedeveloper_mcp::AsdMcpServer;
use agentstatedeveloper_ratify::RatifyOpsImpl;
use anyhow::{Context, Result};
use rmcp::ServiceExt;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let db_path =
        PathBuf::from(std::env::var("ASD_DB").unwrap_or_else(|_| "./.asd-state.db".to_string()));

    tracing::info!(?db_path, "starting asd-pro-mcp stdio server");

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
    let server = AsdMcpServer::with_audit_log(shared, db_path.clone(), audit_log_path);
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    service.waiting().await?;
    tracing::info!("asd-pro-mcp shut down");
    Ok(())
}
