//! `asd audit …` — read back the JSONL audit log.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use agentstatedeveloper_core::{read_jsonl, AuditEvent};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum AuditCmd {
    /// Print audit events from the configured audit log. Supports
    /// filtering by event type and by "since" event id (exclusive).
    Tail(TailArgs),

    /// Verify the hash-chain integrity of the audit log. Commercial
    /// feature — requires `asd-pro` (Enterprise tier).
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Override the audit log path. Defaults to `--audit-log` /
    /// `ASD_AUDIT_LOG` from config.
    #[arg(long)]
    pub log: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct TailArgs {
    /// Override the audit log path. Defaults to `--audit-log` /
    /// `ASD_AUDIT_LOG` from config.
    #[arg(long)]
    pub log: Option<PathBuf>,

    /// Filter by event type substring (e.g., `ledger.approve`,
    /// `ledger.` for all ledger events).
    #[arg(long)]
    pub event_type: Option<String>,

    /// Return only events AFTER this `event_id` (exclusive). Useful
    /// for incremental polling.
    #[arg(long)]
    pub since: Option<String>,

    /// Filter by actor id.
    #[arg(long)]
    pub actor: Option<String>,

    /// Filter by outcome (success, denied, awaiting-approval,
    /// already-resolved, error, unauthorized).
    #[arg(long)]
    pub outcome: Option<String>,

    /// Max events to return (default: 200).
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
}

pub fn run(cfg: &Config, cmd: AuditCmd) -> Result<()> {
    match cmd {
        AuditCmd::Tail(args) => tail(cfg, args),
        AuditCmd::Verify(args) => verify(cfg, args),
    }
}

fn verify(_cfg: &Config, _args: VerifyArgs) -> Result<()> {
    anyhow::bail!(
        "audit verify is a commercial feature (Enterprise tier) — \
         install asd-pro to enable tamper-evident chain verification. \
         See https://agentstatedeveloper.dev/pricing"
    )
}

fn tail(cfg: &Config, args: TailArgs) -> Result<()> {
    let path = args
        .log
        .as_ref()
        .or(cfg.audit_log_path.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no audit log configured — pass --log <path>, \
                 set --audit-log <path> globally, or export ASD_AUDIT_LOG"
            )
        })?;

    let events = read_jsonl(path)
        .with_context(|| format!("read audit log {}", path.display()))?;

    // Apply `since` cursor first (drop up to and including the matching id).
    let start_idx = match args.since {
        Some(ref id) => events
            .iter()
            .position(|e| &e.event_id == id)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };

    let filtered: Vec<&AuditEvent> = events[start_idx..]
        .iter()
        .filter(|e| {
            if let Some(ref t) = args.event_type {
                if !e.event_type.contains(t) {
                    return false;
                }
            }
            if let Some(ref a) = args.actor {
                if &e.actor_id != a {
                    return false;
                }
            }
            if let Some(ref o) = args.outcome {
                if &e.outcome != o {
                    return false;
                }
            }
            true
        })
        .take(args.limit)
        .collect();

    let out = serde_json::json!({
        "path": path.display().to_string(),
        "count": filtered.len(),
        "events": filtered,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
