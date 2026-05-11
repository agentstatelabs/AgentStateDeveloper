//! `asd trust` — emit the State Trust Score for the current workspace.
//!
//! Quick machine-readable answer to: "Can I rely on ASD for this task?"
//!
//! # Output (JSON)
//!
//! ```json
//! {
//!   "score": 0.92,
//!   "level": "high",
//!   "reasons": ["fresh_index", "ledger_annotated", "sidecar_hydrated"],
//!   "blocking": false,
//!   "signals": { ... }
//! }
//! ```
//!
//! Exit code is 0 unless `--fail-blocked` is set and `blocking` is true.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::compute_trust_score;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct TrustArgs {
    /// Exit 1 when the trust score is in the "blocked" level.
    /// Useful for CI pre-flight checks before expensive agent runs.
    #[arg(long)]
    pub fail_blocked: bool,

    /// Suppress the human-readable summary and emit only the JSON object.
    /// (JSON is always emitted; this suppresses the extra narrative line.)
    #[arg(long)]
    pub quiet: bool,
}

pub fn run(cfg: &Config, args: TrustArgs) -> Result<()> {
    let trust = compute_trust_score(&cfg.db_path);

    let json_str = serde_json::to_string_pretty(&trust.to_json())?;
    println!("{json_str}");

    if !args.quiet {
        eprintln!(
            "asd trust: {} ({:.0}%)  — {}",
            trust.level.to_uppercase(),
            trust.score * 100.0,
            trust.reasons.join(", ")
        );
    }

    if args.fail_blocked && trust.blocking {
        std::process::exit(1);
    }

    Ok(())
}
