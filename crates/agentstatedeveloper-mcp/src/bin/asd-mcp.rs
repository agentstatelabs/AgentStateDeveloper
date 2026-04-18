//! `asd-mcp` — AgentStateDeveloper MCP stdio server.
//!
//! Opens an ASD SQLite database and serves the MCP tool surface over stdio.
//! MCP uses stdout for protocol frames, so all logging is routed to stderr.
//!
//! Env:
//! - `ASD_DB` — path to SQLite db (default: `./.asd-state.db`)

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

    let engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;
    let shared = Arc::new(Mutex::new(engine));

    let server = AsdMcpServer::new(shared, db_path.clone());
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    service.waiting().await?;
    tracing::info!("asd-mcp shut down");
    Ok(())
}
