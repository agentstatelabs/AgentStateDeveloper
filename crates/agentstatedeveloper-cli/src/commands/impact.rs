//! `asd impact <qname>` — blast-radius view before editing a symbol.
//!
//! Combines transitive callers, aggregated effects, invariants/hazards,
//! affected test symbols, and recent git touches in one pass.

use std::collections::{HashSet, VecDeque};
use std::process::Command as Proc;

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, IndexStore, LedgerKind,
    LedgerStore, classify_layer, estimate_tokens, intent_focus, load_layer_overrides, parse_intent,
    stale_warning, symbol_tier, trim_for_agent,
};

use crate::commands::graph::build_id_map;
use crate::config::Config;

#[derive(Debug, Args)]
pub struct ImpactArgs {
    /// Fully-qualified symbol name to analyse.
    pub qname: String,

    /// Caller-graph traversal depth (default: 3).
    #[arg(long, default_value = "3")]
    pub depth: usize,

    /// Number of recent git commits to look back per touched file (default: 20).
    #[arg(long, default_value = "20")]
    pub git_depth: usize,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Adjust output framing for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    #[arg(long)]
    pub intent: Option<String>,

    /// Emit token-budgeted JSON for LLM consumption. Trims bodies,
    /// collapses low-signal fields, adds token_estimate.
    #[arg(long)]
    pub agent: bool,
}

pub fn run(cfg: &Config, args: ImpactArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let id_map = build_id_map(&engine);

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let tier = symbol_tier(&symbol.file);
    let layer = classify_layer(&symbol.file, tier, &layer_overrides);

    // --- Transitive callers (BFS) -----------------------------------------
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    visited.insert(symbol.symbol_id.clone());
    queue.push_back((symbol.symbol_id.clone(), 0));

    let mut caller_rows: Vec<Value> = Vec::new();
    let mut affected_test_rows: Vec<Value> = Vec::new();
    // File set for recently_touched — start with the symbol's own file.
    let mut touched_files: IndexMap = IndexMap::new();
    touched_files.insert(symbol.file.clone(), 0usize);

    while let Some((sym_id, depth)) = queue.pop_front() {
        if depth >= args.depth {
            continue;
        }
        let neighbors = index_store
            .get_callers(&engine.ref_name, &sym_id)
            .unwrap_or_default();
        for nbr_id in neighbors {
            if visited.contains(&nbr_id) {
                continue;
            }
            visited.insert(nbr_id.clone());
            if let Some(s) = id_map.get(&nbr_id) {
                let t = symbol_tier(&s.file);
                let l = classify_layer(&s.file, t, &layer_overrides);
                let row = json!({
                    "qname": s.qname,
                    "file": s.file,
                    "line": s.start.line,
                    "depth": depth + 1,
                    "layer": l,
                });
                if t == 2 {
                    // test tier
                    affected_test_rows.push(row);
                } else {
                    caller_rows.push(row.clone());
                }
                touched_files.entry(s.file.clone()).or_insert(depth + 1);
                if depth + 1 < args.depth {
                    queue.push_back((nbr_id, depth + 1));
                }
            }
        }
    }

    // --- Effects from target symbol ----------------------------------------
    let effects_raw = effect_store
        .get_effects(&engine.ref_name, &symbol.symbol_id)
        .unwrap_or_default();
    let effects_out: Value = serde_json::to_value(&effects_raw).unwrap_or(json!(null));

    // --- Invariants/hazards from target + all callers ----------------------
    let mut all_invariants: Vec<Value> = Vec::new();
    let mut all_hazards: Vec<Value> = Vec::new();
    let mut seen_inv: HashSet<String> = HashSet::new();

    let all_sym_ids: Vec<String> = std::iter::once(symbol.symbol_id.clone())
        .chain(visited.iter().cloned())
        .collect();

    for sym_id in &all_sym_ids {
        let entries = ledger_store
            .list_entries(&engine.ref_name, sym_id)
            .unwrap_or_default();
        for entry in entries {
            let key = entry.summary.clone();
            match entry.kind {
                LedgerKind::Invariant => {
                    if seen_inv.insert(key) {
                        let mut v = serde_json::to_value(&entry).unwrap_or(json!(null));
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("source_symbol_id".to_string(), json!(sym_id));
                        }
                        all_invariants.push(v);
                    }
                }
                LedgerKind::Hazard => {
                    let mut v = serde_json::to_value(&entry).unwrap_or(json!(null));
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("source_symbol_id".to_string(), json!(sym_id));
                    }
                    all_hazards.push(v);
                }
                _ => {}
            }
        }
    }

    // --- Recent git touches -----------------------------------------------
    let recently_touched = git_recent_touches(&touched_files, args.git_depth);

    // --- Symbol summary ---------------------------------------------------
    let mut sym_val = serde_json::to_value(&symbol)?;
    if let Some(obj) = sym_val.as_object_mut() {
        obj.remove("body");
    }

    let focus = intent_focus(intent);
    let out = json!({
        "symbol": sym_val,
        "layer": layer,
        "intent": if intent.is_empty() { Value::Null } else { Value::String(intent.to_string()) },
        "focus": if focus.is_empty() { Value::Null } else { Value::String(focus.to_string()) },
        "caller_count": caller_rows.len(),
        "test_count": affected_test_rows.len(),
        "invariants": all_invariants,
        "hazards": all_hazards,
        "effects": effects_out,
        "callers": caller_rows,
        "affected_tests": affected_test_rows,
        "recently_touched": recently_touched,
    });
    let out = if args.agent {
        let trimmed = trim_for_agent(&out, 5);
        let json_str = serde_json::to_string_pretty(&trimmed)?;
        let token_est = estimate_tokens(&json_str);
        let mut v = trimmed;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("token_estimate".into(), json!(token_est));
        }
        v
    } else {
        out
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

// Simple ordered map using Vec to preserve insertion order.
type IndexMap = std::collections::HashMap<String, usize>;

/// Public wrapper for use in prepare_change — takes a slice directly.
pub(crate) fn git_recent_touches_pub(files: &[(String, usize)], git_depth: usize) -> Value {
    let map: IndexMap = files.iter().cloned().collect();
    git_recent_touches(&map, git_depth)
}

/// Run `git log --follow -n <depth> --pretty=format:'...' -- <file>` for each
/// touched file. Returns an array of `{file, commits:[{sha,author,date,msg}]}`.
fn git_recent_touches(files: &IndexMap, git_depth: usize) -> Value {
    let mut result: Vec<Value> = Vec::new();
    // Sort by depth (ascending) so the primary file comes first.
    let mut sorted: Vec<(&String, &usize)> = files.iter().collect();
    sorted.sort_by_key(|(_, d)| **d);

    for (file, _) in sorted {
        let output = Proc::new("git")
            .args([
                "log",
                "--follow",
                &format!("-n{git_depth}"),
                "--pretty=format:%H\x1f%an\x1f%ad\x1f%s",
                "--date=short",
                "--",
                file,
            ])
            .output();

        let commits: Vec<Value> = match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.lines()
                    .filter(|l| !l.is_empty())
                    .filter_map(|line| {
                        let parts: Vec<&str> = line.splitn(4, '\x1f').collect();
                        if parts.len() == 4 {
                            Some(json!({
                                "sha": &parts[0][..8.min(parts[0].len())],
                                "author": parts[1],
                                "date": parts[2],
                                "message": parts[3],
                            }))
                        } else {
                            None
                        }
                    })
                    .collect()
            }
            _ => vec![],
        };

        if !commits.is_empty() {
            result.push(json!({ "file": file, "commits": commits }));
        }
    }
    json!(result)
}
