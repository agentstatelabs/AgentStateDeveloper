//! `asd callers <qname>` / `asd callees <qname>` — call graph traversal.
//!
//! Direct callers/callees are shown by default (`--depth 1`).  Pass
//! `--depth N` for transitive BFS expansion up to N hops.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{AsgIndexStore, Engine, IndexStore, Symbol};

use crate::config::Config;

// ── shared helper ────────────────────────────────────────────────────────────

/// Build a `symbol_id → Symbol` lookup map from the indexed by-qname tree.
/// Used by `read`, `callers`, and `callees` commands.
pub fn build_id_map(engine: &Engine) -> HashMap<String, Symbol> {
    let tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap_or(Value::Object(Default::default()));
    tree.as_object()
        .map(|m| {
            m.values()
                .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                .map(|s| (s.symbol_id.clone(), s))
                .collect()
        })
        .unwrap_or_default()
}

// ── callers command ───────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct CallersArgs {
    /// Fully-qualified symbol name, e.g. `MyModule.myFunc`.
    pub qname: String,

    /// Traversal depth (1 = direct callers only).
    #[arg(long, default_value = "1")]
    pub depth: usize,
}

pub fn run_callers(cfg: &Config, args: CallersArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let id_map = build_id_map(&engine);
    let results = traverse(
        &engine,
        &index_store,
        &symbol.symbol_id,
        args.depth,
        Direction::Callers,
        &id_map,
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "symbol": args.qname,
            "depth": args.depth,
            "count": results.len(),
            "callers": results,
        }))?
    );
    Ok(())
}

// ── callees command ───────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct CalleesArgs {
    /// Fully-qualified symbol name, e.g. `MyModule.myFunc`.
    pub qname: String,

    /// Traversal depth (1 = direct callees only).
    #[arg(long, default_value = "1")]
    pub depth: usize,
}

pub fn run_callees(cfg: &Config, args: CalleesArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let id_map = build_id_map(&engine);
    let results = traverse(
        &engine,
        &index_store,
        &symbol.symbol_id,
        args.depth,
        Direction::Callees,
        &id_map,
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "symbol": args.qname,
            "depth": args.depth,
            "count": results.len(),
            "callees": results,
        }))?
    );
    Ok(())
}

// ── BFS traversal ─────────────────────────────────────────────────────────────

enum Direction {
    Callers,
    Callees,
}

fn traverse(
    engine: &Engine,
    index_store: &AsgIndexStore<'_>,
    start_id: &str,
    max_depth: usize,
    direction: Direction,
    id_map: &HashMap<String, Symbol>,
) -> Vec<Value> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut results: Vec<Value> = Vec::new();

    visited.insert(start_id.to_string());
    queue.push_back((start_id.to_string(), 0));

    while let Some((sym_id, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let neighbors = match direction {
            Direction::Callers => index_store
                .get_callers(&engine.ref_name, &sym_id)
                .unwrap_or_default(),
            Direction::Callees => index_store
                .get_callees(&engine.ref_name, &sym_id)
                .unwrap_or_default(),
        };
        for neighbor_id in neighbors {
            if visited.contains(&neighbor_id) {
                continue;
            }
            visited.insert(neighbor_id.clone());

            let entry = if let Some(s) = id_map.get(&neighbor_id) {
                json!({
                    "qname": s.qname,
                    "file": s.file,
                    "line": s.start.line,
                    "depth": depth + 1,
                })
            } else {
                json!({ "symbol_id": neighbor_id, "depth": depth + 1 })
            };
            results.push(entry);

            if depth + 1 < max_depth {
                queue.push_back((neighbor_id, depth + 1));
            }
        }
    }

    results
}
