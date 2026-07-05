//! `asd architecture` — a one-call "orient me" overview for a cold agent.
//!
//! Assembles existing index data into a single snapshot: languages, top
//! packages, architectural layers, HTTP routes, call-graph communities, and
//! hotspots. Read-only. The JSON body is built by
//! [`agentstatedeveloper_core::architecture_overview`] so the CLI and the
//! `asd-mcp` `architecture` tool return byte-identical payloads.

use anyhow::Result;
use clap::Args;
use serde_json::Value;

use agentstatedeveloper_core::{Engine, architecture_overview};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct ArchitectureArgs {
    /// Machine-readable JSON.
    #[arg(long)]
    pub agent: bool,

    /// How many packages / hotspots to list.
    #[arg(long, default_value = "12")]
    pub top: usize,
}

pub fn run(cfg: &Config, args: ArchitectureArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let out = architecture_overview(&engine, args.top);

    if args.agent {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    print_human(&out);
    Ok(())
}

fn print_human(out: &Value) {
    println!(
        "Architecture overview — {} ({} symbols)",
        out["repo_id"].as_str().unwrap_or("?"),
        out["symbols"].as_u64().unwrap_or(0)
    );
    let row = |label: &str, items: &Value| {
        if let Some(arr) = items.as_array() {
            let parts: Vec<String> = arr
                .iter()
                .map(|e| {
                    let name = e
                        .get("name")
                        .or_else(|| e.get("layer"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    format!("{} ({})", name, e["symbols"].as_u64().unwrap_or(0))
                })
                .collect();
            println!("  {label}: {}", parts.join(", "));
        }
    };
    row("languages", &out["languages"]);
    row("layers", &out["layers"]);
    row("packages", &out["packages"]);

    if let Some(cl) = out["clusters"].as_array() {
        if !cl.is_empty() {
            println!("  clusters (call-graph communities):");
            for c in cl.iter().take(8) {
                println!(
                    "    {:>4} symbols  {}  (repr: {})",
                    c["size"].as_u64().unwrap_or(0),
                    c["package"].as_str().unwrap_or("?"),
                    c["representative"].as_str().unwrap_or("?")
                );
            }
        }
    }

    let inbound = out["routes"]["inbound"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    println!(
        "  routes: {} inbound, {} outbound consumers",
        inbound.len(),
        out["routes"]["outbound_consumers"].as_u64().unwrap_or(0)
    );
    for r in inbound.iter().take(10) {
        println!(
            "    {}  ({})",
            r["contract"].as_str().unwrap_or("?"),
            r["qname"].as_str().unwrap_or("?")
        );
    }

    if let Some(hs) = out["hotspots"].as_array() {
        println!("  hotspots (call-graph degree):");
        for h in hs.iter().take(10) {
            println!(
                "    {:>3}  {}  ({})",
                h["degree"].as_u64().unwrap_or(0),
                h["qname"].as_str().unwrap_or("?"),
                h["file"].as_str().unwrap_or("?")
            );
        }
    }
}
