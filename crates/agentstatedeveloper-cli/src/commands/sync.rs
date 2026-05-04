//! `asd sync [--dir <path>] [--prune]` — mirror live ASG state into the
//! `.asd/v1/` on-disk sidecar. The sidecar travels with `git commit`,
//! letting a fresh `git clone` hydrate an ASD repo without a network
//! registry.
//!
//! `--prune` removes orphaned sidecar files for symbols that no longer
//! exist in the index (renamed or deleted). The pre-commit hook runs
//! `asd sync --prune` automatically.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{prune_sidecar, sync_to_dir, Engine};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Project root to sync into. `.asd/v1/` is appended internally.
    /// Defaults to the current working directory.
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Remove orphaned `.asd/v1/` files for symbols that no longer exist
    /// in the index. Safe to use on every commit via the pre-commit hook.
    #[arg(long, default_value_t = false)]
    pub prune: bool,
}

pub fn run(cfg: &Config, args: SyncArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let dir = resolve_dir(args.dir)?;

    let mut summary = sync_to_dir(&engine.repo, &engine.ref_name, &dir)?;

    if args.prune {
        summary.pruned = prune_sidecar(&engine.repo, &engine.ref_name, &dir)?;
    }

    let out = json!({
        "dir": dir.join(".asd/v1").display().to_string(),
        "effects_written": summary.effects_written,
        "ledger_entries_written": summary.ledger_entries_written,
        "symbols_written": summary.symbols_written,
        "schema_version": summary.schema_version,
        "pruned": summary.pruned,
        "note": "current-state only; ASG commit history is not carried in the sidecar",
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn resolve_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p),
        None => Ok(std::env::current_dir()?),
    }
}
