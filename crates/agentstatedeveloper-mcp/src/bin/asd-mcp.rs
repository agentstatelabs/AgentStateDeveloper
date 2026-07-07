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

    let (db_path, track_registry) = match std::env::var("ASD_DB") {
        // Explicit ASD_DB pins this process to one db; do not follow the registry.
        Ok(p) if !p.is_empty() => (PathBuf::from(p), false),
        // Plan S t-004: no pinned db — resolve from the server's own startup
        // directory first (git-style walk-up). Each agent session is spawned in
        // its project's dir, so this isolates concurrent sessions on different
        // repos without pinning. Only when the server isn't inside any ASD
        // project do we fall back to the registry's active repo (and track it).
        _ => match agentstatedeveloper_core::registry::find_db_upwards() {
            Some(db) => {
                tracing::info!(?db, "resolved db via cwd walk-up (per-session isolation)");
                (db, false)
            }
            None => (resolve_db_from_registry()?, true),
        },
    };

    tracing::info!(?db_path, track_registry, "starting asd-mcp stdio server");

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

    let server = AsdMcpServer::with_registry_tracking(
        shared,
        db_path.clone(),
        audit_log_path,
        track_registry,
    );
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
///
/// 1.1.11 (parity regression follow-up): when `./.asd-state.db` exists in
/// the cwd, the error includes the absolute path so the operator can copy-
/// paste it into `ASD_DB=...`. Detection only, no silent fallback — the
/// design intent above stands.
fn resolve_db_from_registry() -> Result<PathBuf> {
    use agentstatedeveloper_core::registry::Registry;

    let cwd_hint = cwd_db_hint();
    let reg = Registry::load().context(cwd_aware_error(
        "ASD_DB not set and could not read repo registry.",
        &cwd_hint,
    ))?;
    if let Some(active) = reg.active() {
        tracing::info!(name = %active.name, "resolved db from registry active repo");
        return Ok(active.path.clone());
    }
    let known: Vec<String> = reg.list().iter().map(|e| e.name.clone()).collect();
    let base = if known.is_empty() {
        "Registry is empty.".to_string()
    } else {
        format!(
            "No active repo. Run `asd repo use <name>` (known: {}).",
            known.join(", ")
        )
    };
    Err(anyhow::anyhow!(cwd_aware_error(&base, &cwd_hint)))
}

/// If `./.asd-state.db` exists in cwd, return its absolute path as a hint
/// for the error message. Returns None when no such file exists or the cwd
/// cannot be resolved.
fn cwd_db_hint() -> Option<PathBuf> {
    let candidate = std::env::current_dir().ok()?.join(".asd-state.db");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Build the "ASD_DB not set" error message. When the operator clearly
/// has an indexed db at `./.asd-state.db`, surface that exact path so
/// the recovery step is a literal copy-paste.
fn cwd_aware_error(base: &str, cwd_hint: &Option<PathBuf>) -> String {
    match cwd_hint {
        Some(p) => format!(
            "{base} Detected ./.asd-state.db at the current working \
             directory — start with `ASD_DB={}` to use it, or run \
             `asd repo add` then `asd repo use <name>` to register it.",
            p.display()
        ),
        None => format!(
            "{base} Run `asd repo add` then `asd repo use <name>`, \
             or start with ASD_DB=<path>."
        ),
    }
}
