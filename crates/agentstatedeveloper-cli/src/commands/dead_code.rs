//! `asd dead-code` — functions/methods with no inbound call edges in the index.
//!
//! Candidate list, not a verdict. The computation (including the route-handler /
//! test / runtime-entry exclusions) lives in
//! [`agentstatedeveloper_core::dead_code_report`] so the CLI and the `asd-mcp`
//! `dead_code` tool return byte-identical payloads.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{Engine, dead_code_report};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct DeadCodeArgs {
    /// Machine-readable JSON.
    #[arg(long)]
    pub agent: bool,

    /// Max candidates to list (the total count is always reported).
    #[arg(long, default_value = "50")]
    pub limit: usize,

    /// Include test functions (excluded by default).
    #[arg(long)]
    pub include_tests: bool,
}

pub fn run(cfg: &Config, args: DeadCodeArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let out = dead_code_report(&engine, args.limit, args.include_tests);

    if args.agent {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let total = out["count"].as_u64().unwrap_or(0) as usize;
    let candidates = out["candidates"].as_array().cloned().unwrap_or_default();
    let excluded_handlers = out["excluded"]["route_handlers"].as_u64().unwrap_or(0);
    let excluded_tests = out["excluded"]["test_symbols"].as_u64().unwrap_or(0);

    println!(
        "{} unreferenced function/method{} (no inbound call edges).",
        total,
        if total == 1 { "" } else { "s" }
    );
    if excluded_handlers > 0 || excluded_tests > 0 {
        println!(
            "  (excluded {} route handler(s), {} test symbol(s))",
            excluded_handlers, excluded_tests
        );
    }
    for c in candidates.iter() {
        println!(
            "  {}:{}  {}",
            c["file"].as_str().unwrap_or("?"),
            c["line"].as_u64().unwrap_or(0),
            c["qname"].as_str().unwrap_or("?")
        );
    }
    if total > candidates.len() {
        println!("  … {} more (use --limit)", total - candidates.len());
    }
    println!(
        "\nNote: candidates only — static call graph misses public API, dynamic dispatch, and framework callbacks."
    );
    Ok(())
}
