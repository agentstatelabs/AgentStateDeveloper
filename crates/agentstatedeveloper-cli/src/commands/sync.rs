//! `asd sync [--dir <path>] [--prune]` — mirror live ASG state into the
//! `.asd/v1/` on-disk sidecar. The sidecar travels with `git commit`,
//! letting a fresh `git clone` hydrate an ASD repo without a network
//! registry.
//!
//! `--prune` removes orphaned sidecar files for symbols that no longer
//! exist in the index (renamed or deleted). The pre-commit hook runs
//! `asd sync --prune` automatically.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{Engine, ledger_counts, prune_sidecar, sync_to_dir};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Project root to sync into. `.asd/v1/` is appended internally.
    /// Defaults to the directory holding the db (the project the store
    /// belongs to), not the current working directory.
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Remove orphaned `.asd/v1/` files for symbols that no longer exist
    /// in the index. Safe to use on every commit via the pre-commit hook.
    #[arg(long, default_value_t = false)]
    pub prune: bool,
}

pub fn run(cfg: &Config, args: SyncArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let dir = resolve_dir(args.dir, &cfg.db_path)?;

    let mut summary = sync_to_dir(&engine.repo, &engine.ref_name, &dir)?;

    if args.prune {
        summary.pruned = prune_sidecar(&engine.repo, &engine.ref_name, &dir)?;
    }

    // Sync exports the store, so an entry the store lost is silently absent
    // from the sidecar too. Count what the ledger cache acknowledged against
    // what landed, and say so when they differ.
    let ledger = ledger_counts(
        &engine.repo,
        &engine.ref_name,
        engine.fts.as_ref(),
        Some(&dir),
    )?;
    let ledger_warning = ledger.warning();
    if let Some(w) = &ledger_warning {
        eprintln!("asd sync: warning: {w}");
    }

    let out = json!({
        "db": cfg.db_path.display().to_string(),
        "dir": dir.join(".asd/v1").display().to_string(),
        "effects_written": summary.effects_written,
        "ledger_entries_written": summary.ledger_entries_written,
        "symbols_written": summary.symbols_written,
        "schema_version": summary.schema_version,
        "pruned": summary.pruned,
        "ledger": ledger,
        "ledger_warning": ledger_warning,
        "note": "current-state only; ASG commit history is not carried in the sidecar",
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// The sidecar belongs beside the store it mirrors. Defaulting to the current
/// directory paired a walk-up- or registry-resolved store with wherever `asd`
/// happened to run, exporting one project's state into another (or into a
/// subdirectory). The MCP `sync` tool already defaults to the db's directory.
fn resolve_dir(explicit: Option<PathBuf>, db_path: &Path) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    Ok(match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => cwd.join(p),
        _ => cwd,
    })
}
