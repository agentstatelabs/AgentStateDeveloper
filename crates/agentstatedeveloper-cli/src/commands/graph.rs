//! `asd callers <qname>` / `asd callees <qname>` — call graph traversal.
//!
//! Direct callers/callees are shown by default (`--depth 1`).  Pass
//! `--depth N` for transitive BFS expansion up to N hops.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{AsgIndexStore, Engine, IndexStore, Symbol};

use crate::config::Config;

// ── shared helper ────────────────────────────────────────────────────────────

/// Build a `symbol_id → Symbol` lookup map.
///
/// Fast path: reads from `asd_symbols_cache` via the engine's shared FTS connection.
/// Fallback: walks the `/asd/v1/index/by-qname` git tree.
pub fn build_id_map(engine: &Engine) -> HashMap<String, Symbol> {
    // Fast path: SQLite symbol cache — reuse engine's already-open connection.
    if let Some(fts) = engine.fts.as_ref() {
        if fts.symbols_cached_for(&engine.ref_name) {
            let map = fts.build_id_map_cached(&engine.ref_name);
            if !map.is_empty() {
                return map;
            }
        }
    }
    // Authoritative git fallback.
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

    /// Print per-phase timing to stderr.
    #[arg(long)]
    pub timing: bool,
}

pub fn run_callers(cfg: &Config, args: CallersArgs) -> Result<()> {
    let t = AsdTimer::new(args.timing);
    let mut t = t;
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    t.phase("open_engine");
    let index_store = AsgIndexStore::from_engine(&engine);

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;
    t.phase("symbol_lookup");

    let id_map = index_store.build_id_map(&engine);
    t.phase("build_id_map");

    let results = traverse(
        &engine,
        &index_store,
        &symbol.symbol_id,
        args.depth,
        Direction::Callers,
        &id_map,
    );
    t.phase("traverse");

    let qid = crate::commands::brief::query_id(
        "callers",
        &[&args.qname, &args.depth.to_string()],
    );
    if cfg.brief {
        let lines = crate::commands::brief::brief_call_list(&results);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "callers": lines,
                "query_id": qid,
            }))?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "symbol": args.qname,
                "depth": args.depth,
                "count": results.len(),
                "callers": results,
                "query_id": qid,
            }))?
        );
    }
    t.total("callers");
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

    /// Print per-phase timing to stderr.
    #[arg(long)]
    pub timing: bool,
}

pub fn run_callees(cfg: &Config, args: CalleesArgs) -> Result<()> {
    let t = AsdTimer::new(args.timing);
    let mut t = t;
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    t.phase("open_engine");
    let index_store = AsgIndexStore::from_engine(&engine);

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;
    t.phase("symbol_lookup");

    let id_map = index_store.build_id_map(&engine);
    t.phase("build_id_map");

    let results = traverse(
        &engine,
        &index_store,
        &symbol.symbol_id,
        args.depth,
        Direction::Callees,
        &id_map,
    );
    t.phase("traverse");

    let qid = crate::commands::brief::query_id(
        "callees",
        &[&args.qname, &args.depth.to_string()],
    );
    if cfg.brief {
        let lines = crate::commands::brief::brief_call_list(&results);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "callees": lines,
                "query_id": qid,
            }))?
        );
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "symbol": args.qname,
                "depth": args.depth,
                "count": results.len(),
                "callees": results,
                "query_id": qid,
            }))?
        );
    }
    t.total("callees");
    Ok(())
}

// ── phase timer ──────────────────────────────────────────────────────────────

pub(crate) struct AsdTimer {
    start: Instant,
    last: Instant,
    enabled: bool,
}

impl AsdTimer {
    pub(crate) fn new(enabled: bool) -> Self {
        let now = Instant::now();
        Self { start: now, last: now, enabled }
    }
    pub(crate) fn phase(&mut self, name: &str) {
        if self.enabled {
            let now = Instant::now();
            eprintln!(
                "[timing] {:20} {:5.0}ms  (total {:5.0}ms)",
                name,
                (now - self.last).as_secs_f64() * 1000.0,
                (now - self.start).as_secs_f64() * 1000.0,
            );
            self.last = now;
        }
    }
    pub(crate) fn total(&self, label: &str) {
        if self.enabled {
            eprintln!(
                "[timing] {:20}         total {:5.0}ms",
                label,
                self.start.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }
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
