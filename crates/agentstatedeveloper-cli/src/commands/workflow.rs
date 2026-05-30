//! `asd workflow` — view task workflow session history.
//!
//! Reads `.asd/workflow-sessions.jsonl` (written by `asd task-close`) and
//! presents the most recent sessions either as a summary table (default) or
//! as raw JSON (`--json`).
//!
//! ## Usage
//!
//! ```text
//! asd workflow [--last <n>] [--json]
//! ```

use anyhow::Result;
use clap::Args;
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct WorkflowArgs {
    /// Number of recent sessions to show (default: 20).
    #[arg(long, default_value = "20")]
    pub last: usize,

    /// Emit raw JSON array instead of the summary table.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: WorkflowArgs) -> Result<()> {
    let dot_asd = cfg
        .db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(".asd"))
        .unwrap_or_else(|| std::path::PathBuf::from(".asd"));

    let sessions_path = dot_asd.join("workflow-sessions.jsonl");

    let raw = std::fs::read_to_string(&sessions_path).unwrap_or_default();
    let mut sessions: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Most-recent first.
    sessions.reverse();
    sessions.truncate(args.last);

    if args.json {
        let out = serde_json::json!({
            "sessions": sessions,
            "count": sessions.len(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if sessions.is_empty() {
        eprintln!("asd: no workflow sessions recorded yet — run `asd task-close` first");
        return Ok(());
    }

    // Human-readable table.
    println!(
        "{:<28}  {:<22}  {:<8}  {:<6}  {}",
        "closed_at", "workflow_type", "ev_score", "syms", "missing_steps"
    );
    println!("{}", "-".repeat(90));
    for s in &sessions {
        let closed_at = s.get("closed_at").and_then(Value::as_str).unwrap_or("—");
        let wf_type = s
            .get("workflow_type")
            .and_then(Value::as_str)
            .unwrap_or("—");
        let ev_score = s
            .get("evidence_score")
            .and_then(Value::as_f64)
            .map(|f| format!("{:.2}", f))
            .unwrap_or_else(|| "—".to_string());
        let syms = s
            .get("symbols_annotated")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".to_string());
        let missing = s
            .get("missing_steps")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let missing_display = if missing.is_empty() {
            "—".to_string()
        } else {
            missing
        };

        // Trim closed_at to a readable length.
        let ts = if closed_at.len() > 27 {
            &closed_at[..27]
        } else {
            closed_at
        };
        println!(
            "{:<28}  {:<22}  {:<8}  {:<6}  {}",
            ts, wf_type, ev_score, syms, missing_display
        );
    }

    Ok(())
}
