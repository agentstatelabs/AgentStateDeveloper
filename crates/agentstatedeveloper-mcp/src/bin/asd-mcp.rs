//! `asd-mcp` — AgentStateDeveloper MCP stdio server.
//!
//! Opens an ASD SQLite database and serves the MCP tool surface over stdio.
//! MCP uses stdout for protocol frames, so all logging is routed to stderr.
//!
//! Env:
//! - `ASD_DB` — path to SQLite db. When unset, asd-mcp resolves the active
//!   repo from `~/.config/asd/repos.toml` (see `asd repo use <name>`).
//! - `ASD_POLICY` — optional path to a policy JSON file. When set, the
//!   engine's `PolicyGate` is swapped to a `FilePolicyGate` loaded from that
//!   file. Matches the `asd` CLI contract.
//! - `ASD_AUDIT_LOG` — commercial feature (Enterprise tier). In the
//!   OSS `asd-mcp` binary this is recognized for read-only `audit_tail`
//!   over logs produced by `asd-pro`, but no new events are written.

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

    let db_path = match std::env::var("ASD_DB") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => resolve_db_from_registry()?,
    };

    tracing::info!(?db_path, "starting asd-mcp stdio server");

    let mut engine = Engine::open_sqlite(&db_path)
        .with_context(|| format!("failed to open ASD db at {}", db_path.display()))?;

    // Optional policy file. Fail loudly if set but unloadable — we never want
    // a configured-but-silent permissive gate.
    if let Ok(policy_path) = std::env::var("ASD_POLICY") {
        let path = PathBuf::from(&policy_path);
        engine.load_policy_file(&path).with_context(|| {
            format!(
                "failed to load ASD_POLICY policy file at {}",
                path.display()
            )
        })?;
        tracing::info!(policy = %path.display(), "loaded ASD policy file");
    }

    // ASD_AUDIT_LOG is recognized for read-only tailing of logs
    // produced by asd-pro. OSS asd-mcp does not write new chain-signed
    // events — the engine stays on NullSink.
    let audit_log_path: Option<PathBuf> = std::env::var("ASD_AUDIT_LOG").ok().map(PathBuf::from);
    if audit_log_path.is_some() {
        tracing::warn!(
            "ASD_AUDIT_LOG set but asd-mcp is OSS — no new events \
             will be written (tamper-evident sink is commercial). \
             Existing logs are readable via audit_tail."
        );
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

/// Look up the active repo in `~/.config/asd/repos.toml` and return its db
/// path. Errors with a clear, actionable message if the registry is missing,
/// empty, or has no active entry — we deliberately do NOT silently fall back
/// to `./.asd-state.db`, because that masks misconfiguration with what looks
/// like a successful startup on the wrong db.
fn resolve_db_from_registry() -> Result<PathBuf> {
    use agentstatedeveloper_core::registry::Registry;

    let reg = Registry::load().context(
        "ASD_DB not set and could not read repo registry. Run `asd repo add` then \
         `asd repo use <name>`, or start with ASD_DB=<path>.",
    )?;
    if let Some(active) = reg.active() {
        tracing::info!(name = %active.name, "resolved db from registry active repo");
        return Ok(active.path.clone());
    }
    let known: Vec<String> = reg.list().iter().map(|e| e.name.clone()).collect();
    let hint = if known.is_empty() {
        "Registry is empty. Run `asd repo add` then `asd repo use <name>`, \
         or start with ASD_DB=<path>."
            .to_string()
    } else {
        format!(
            "No active repo. Run `asd repo use <name>` (known: {}), \
             or start with ASD_DB=<path>.",
            known.join(", ")
        )
    };
    Err(anyhow::anyhow!(hint))
}
