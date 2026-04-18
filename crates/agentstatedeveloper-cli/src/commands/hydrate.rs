//! `asd hydrate [--dir <path>]` — read the `.asd/v1/` sidecar and write
//! its contents back into the ASG repo. Inverse of `asd sync`.
//!
//! Intended for fresh `git clone` flows: after `asd init`, `asd hydrate`
//! populates the ASG repo from the committed sidecar so the local
//! machine has full current-state without a registry call.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{hydrate_from_dir, Engine};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct HydrateArgs {
    /// Project root to hydrate from. `.asd/v1/` is appended internally.
    /// Defaults to the current working directory.
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

pub fn run(cfg: &Config, args: HydrateArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let dir = resolve_dir(args.dir)?;

    // sidecar::hydrate_from_dir returns a clear error if `.asd/v1/`
    // doesn't exist. Surface it as-is; the message already says "did
    // you mean to run `asd sync` first?".
    let summary = hydrate_from_dir(&engine.repo, &engine.ref_name, &dir, &cfg.agent_id)?;

    let out = json!({
        "dir": dir.join(".asd/v1").display().to_string(),
        "effects_loaded": summary.effects_loaded,
        "ledger_entries_loaded": summary.ledger_entries_loaded,
        "symbols_loaded": summary.symbols_loaded,
        "missing_schema_version": summary.missing_schema_version,
        "note": "commit history not restored; run `asd index` to rebuild the semantic index and call graph",
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
