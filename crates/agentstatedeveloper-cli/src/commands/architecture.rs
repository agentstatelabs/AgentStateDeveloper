//! `asd architecture` — a one-call "orient me" overview for a cold agent.
//!
//! Assembles existing index data into a single snapshot: languages, top
//! packages, architectural layers, HTTP routes (from the cross-service endpoint
//! registry), and call-graph hotspots. Read-only. Functional clusters are
//! approximated by layer for now; true community detection is plan task t-009.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    Direction, Engine, classify_layer_sym, endpoints_from_tree, load_layer_overrides,
    resolve_repo_id, symbol_tier,
};

use crate::commands::graph::build_id_map;
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
    let id_map = build_id_map(&engine);
    let overrides = load_layer_overrides(&cfg.db_path);

    // Languages, packages, layers — one pass over all symbols.
    let mut languages: HashMap<String, usize> = HashMap::new();
    let mut packages: HashMap<String, usize> = HashMap::new();
    let mut layers: BTreeMap<&'static str, usize> = BTreeMap::new();
    for s in id_map.values() {
        *languages.entry(s.language.clone()).or_default() += 1;
        *packages.entry(package_of(&s.file)).or_default() += 1;
        let layer = classify_layer_sym(&s.file, &s.qname, symbol_tier(&s.file), &overrides);
        *layers.entry(layer).or_default() += 1;
    }

    // Hotspots: call-graph degree = |callers| + |callees|.
    let degree = symbol_degree(&engine);
    let mut hot: Vec<(&String, usize)> = degree.iter().map(|(k, v)| (k, *v)).collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let hotspots: Vec<Value> = hot
        .iter()
        .take(args.top)
        .filter_map(|(id, deg)| {
            id_map.get(*id).map(|s| {
                json!({ "qname": s.qname, "file": s.file, "degree": deg })
            })
        })
        .collect();

    // Routes: inbound endpoints this repo serves (+ outbound consumer count).
    let ep_tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
        .unwrap_or(Value::Null);
    let endpoints = endpoints_from_tree(&ep_tree);
    let mut inbound: Vec<Value> = endpoints
        .iter()
        .filter(|e| e.direction == Direction::Inbound)
        .map(|e| json!({ "contract": e.contract, "qname": e.qname }))
        .collect();
    inbound.sort_by(|a, b| a["contract"].as_str().cmp(&b["contract"].as_str()));
    let outbound_count = endpoints
        .iter()
        .filter(|e| e.direction == Direction::Outbound)
        .count();

    let repo_id = resolve_repo_id(std::env::var("ASD_REPO_ID").ok().as_deref(), &cwd());

    let out = json!({
        "repo_id": repo_id,
        "symbols": id_map.len(),
        "languages": sorted_counts(languages, usize::MAX),
        "packages": sorted_counts(packages, args.top),
        "layers": layers.iter().map(|(k, v)| json!({ "layer": k, "symbols": v })).collect::<Vec<_>>(),
        "routes": { "inbound": inbound, "outbound_consumers": outbound_count },
        "hotspots": hotspots,
        "note": "functional clusters approximated by layer; community detection is task t-009",
    });

    if args.agent {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    print_human(&out);
    Ok(())
}

/// Parent directory of a file, used as a coarse package/module grouping.
fn package_of(file: &str) -> String {
    match file.rfind('/') {
        Some(i) => file[..i].to_string(),
        None => ".".to_string(),
    }
}

/// Call-graph degree per symbol_id from the callers/callees registry trees.
fn symbol_degree(engine: &Engine) -> HashMap<String, usize> {
    let mut degree: HashMap<String, usize> = HashMap::new();
    for (path, key) in [
        ("/asd/v1/index/callees", "callees"),
        ("/asd/v1/index/callers", "callers"),
    ] {
        let tree = engine
            .repo
            .get_tree(&engine.ref_name, path)
            .unwrap_or(Value::Null);
        if let Some(obj) = tree.as_object() {
            for (sym_id, v) in obj {
                let n = v.get(key).and_then(|x| x.as_array()).map_or(0, |a| a.len());
                *degree.entry(sym_id.clone()).or_default() += n;
            }
        }
    }
    degree
}

/// `[{name, count}]` sorted by count desc then name, truncated to `limit`.
/// The count key is `symbols` to match the overview's vocabulary.
fn sorted_counts(map: HashMap<String, usize>, limit: usize) -> Vec<Value> {
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter()
        .take(limit)
        .map(|(name, n)| json!({ "name": name, "symbols": n }))
        .collect()
}

fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_of_uses_parent_dir() {
        assert_eq!(package_of("crates/core/src/lib.rs"), "crates/core/src");
        assert_eq!(package_of("main.rs"), ".");
    }

    #[test]
    fn sorted_counts_orders_desc_then_name_and_truncates() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), 1);
        m.insert("b".to_string(), 3);
        m.insert("c".to_string(), 3);
        let v = sorted_counts(m, 2);
        // Ties broken by name: b before c; both before a; truncated to 2.
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["name"], "b");
        assert_eq!(v[0]["symbols"], 3);
        assert_eq!(v[1]["name"], "c");
    }
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
                    let name = e.get("name").or_else(|| e.get("layer")).and_then(|v| v.as_str()).unwrap_or("?");
                    format!("{} ({})", name, e["symbols"].as_u64().unwrap_or(0))
                })
                .collect();
            println!("  {label}: {}", parts.join(", "));
        }
    };
    row("languages", &out["languages"]);
    row("layers", &out["layers"]);
    row("packages", &out["packages"]);

    let inbound = out["routes"]["inbound"].as_array().cloned().unwrap_or_default();
    println!(
        "  routes: {} inbound, {} outbound consumers",
        inbound.len(),
        out["routes"]["outbound_consumers"].as_u64().unwrap_or(0)
    );
    for r in inbound.iter().take(10) {
        println!("    {}  ({})", r["contract"].as_str().unwrap_or("?"), r["qname"].as_str().unwrap_or("?"));
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
