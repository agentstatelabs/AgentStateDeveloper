//! `asd map` — initial-read project summary.
//!
//! Walks the indexed project, identifies package boundaries, and classifies
//! test files into `fast-test` vs `diagnostic-test`. Results land as
//! `Ownership` ledger entries so the next session inherits the project mental
//! model without re-deriving it. Idempotent: re-running overwrites prior tags.
//!
//! The logic lives in `agentstatedeveloper_core::map::run_map` so the `map`
//! MCP tool shares one implementation with this CLI command.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{Engine, map::run_map};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct MapArgs {
    /// Dry-run: emit the summary without writing any ledger entries.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(cfg: &Config, args: MapArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let db_parent = cfg.db_path.parent();
    let payload = run_map(&engine, &cfg.agent_id, db_parent, args.dry_run)?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}
