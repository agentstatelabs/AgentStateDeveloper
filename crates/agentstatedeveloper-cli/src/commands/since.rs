//! `asd since <sha>` — symbols in files changed since a commit + blast radius.
//!
//! PR/review workflow: given the base SHA of a branch, surfaces every symbol
//! that lives in a modified file, their combined transitive callers, affected
//! tests, and any invariants/hazards that apply — without the caller needing
//! to know any symbol names upfront.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command as Proc;

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, IndexStore, LedgerKind,
    LedgerStore, classify_layer_sym, estimate_tokens, git_dirty_files, intent_focus,
    load_layer_overrides, matches_any_path_glob, parse_intent, propose_test_path, resolve_scope,
    stale_warning, symbol_tier, trim_for_agent,
};

use crate::commands::{graph::build_id_map, impact::git_recent_touches_pub};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct SinceArgs {
    /// Base commit SHA (or branch/tag) to diff against HEAD.
    pub sha: String,

    /// Caller-graph BFS depth for blast radius (default: 3).
    #[arg(long, default_value = "3")]
    pub depth: usize,

    /// Number of recent git commits to scan per changed file (default: 10).
    #[arg(long, default_value = "10")]
    pub git_depth: usize,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Adjust output framing for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    #[arg(long)]
    pub intent: Option<String>,

    /// Emit token-budgeted JSON for LLM consumption.
    #[arg(long)]
    pub agent: bool,

    /// Token budget when --agent is set (default: 8000).
    #[arg(long, default_value = "8000")]
    pub agent_budget: usize,

    /// Comma-separated glob patterns to restrict touched symbols to specific paths.
    #[arg(long)]
    pub paths: Option<String>,

    /// Named scope alias from .asd/scopes.toml, e.g. --scope drift-pad.
    #[arg(long)]
    pub scope: Option<String>,

    /// Maximum number of seed symbols to include (default: unlimited).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Output is always JSON; this flag is accepted for CLI consistency.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: SinceArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let id_map = build_id_map(&engine);

    // --- Get changed files since <sha> ------------------------------------
    let changed_files = git_changed_files(&args.sha);
    if changed_files.is_empty() {
        let out = json!({
            "sha": args.sha,
            "changed_files": [],
            "touched_symbols": {},
            "callers": [],
            "affected_tests": [],
            "invariants": [],
            "hazards": [],
            "effects": [],
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let changed_set: HashSet<&str> = changed_files.iter().map(String::as_str).collect();

    // Build optional path filter from --scope / --paths.
    let mut paths_filter: Vec<String> = Vec::new();
    if let Some(ref scope) = args.scope {
        paths_filter.extend(resolve_scope(scope, &cfg.db_path));
    }
    if let Some(ref paths) = args.paths {
        paths_filter.extend(
            paths
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
        );
    }

    // --- Find all indexed symbols in changed files ------------------------
    let mut seed_symbols: Vec<&agentstatedeveloper_core::Symbol> = id_map
        .values()
        .filter(|s| changed_set.contains(s.file.as_str()))
        .filter(|s| paths_filter.is_empty() || matches_any_path_glob(&paths_filter, &s.file))
        .collect();
    if let Some(lim) = args.limit {
        seed_symbols.truncate(lim);
    }

    // Group seeds by layer for the touched_symbols output.
    let mut by_layer: HashMap<String, Vec<Value>> = HashMap::new();
    for sym in &seed_symbols {
        let tier = symbol_tier(&sym.file);
        let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
        let entry = json!({
            "qname": sym.qname,
            "file": sym.file,
            "line": sym.start.line,
            "layer": layer,
        });
        by_layer.entry(layer.to_string()).or_default().push(entry);
    }

    // --- BFS blast radius from all seeds ----------------------------------
    let mut visited: HashSet<String> = seed_symbols.iter().map(|s| s.symbol_id.clone()).collect();
    let mut queue: VecDeque<(String, usize)> = seed_symbols
        .iter()
        .map(|s| (s.symbol_id.clone(), 0))
        .collect();

    let mut caller_rows: Vec<Value> = Vec::new();
    let mut affected_test_rows: Vec<Value> = Vec::new();
    let mut touched_files: Vec<(String, usize)> =
        changed_files.iter().map(|f| (f.clone(), 0)).collect();
    let mut seen_files: HashSet<String> = changed_files.iter().cloned().collect();

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
                let tier = symbol_tier(&s.file);
                let layer = classify_layer_sym(&s.file, &s.qname, tier, &layer_overrides);
                let row = json!({
                    "qname": s.qname,
                    "file": s.file,
                    "line": s.start.line,
                    "depth": depth + 1,
                    "layer": layer,
                });
                if tier == 2 {
                    affected_test_rows.push(row);
                } else {
                    caller_rows.push(row);
                }
                if seen_files.insert(s.file.clone()) {
                    touched_files.push((s.file.clone(), depth + 1));
                }
                if depth + 1 < args.depth {
                    queue.push_back((nbr_id, depth + 1));
                }
            }
        }
    }

    // --- Aggregate effects/invariants/hazards from seeds ------------------
    let all_sym_ids: Vec<String> = seed_symbols.iter().map(|s| s.symbol_id.clone()).collect();

    let mut all_invariants: Vec<Value> = Vec::new();
    let mut all_hazards: Vec<Value> = Vec::new();
    let mut all_effects: Vec<Value> = Vec::new();
    let mut seen_inv: HashSet<String> = HashSet::new();

    for sym_id in &all_sym_ids {
        let entries = ledger_store
            .list_entries(&engine.ref_name, sym_id)
            .unwrap_or_default();
        for entry in entries {
            let key = entry.summary.clone();
            let sym_qname = id_map.get(sym_id).map(|s| s.qname.as_str()).unwrap_or("");
            match entry.kind {
                LedgerKind::Invariant => {
                    if seen_inv.insert(key) {
                        all_invariants.push(json!({
                            "summary": entry.summary,
                            "source": sym_qname,
                        }));
                    }
                }
                LedgerKind::Hazard => {
                    all_hazards.push(json!({
                        "summary": entry.summary,
                        "source": sym_qname,
                    }));
                }
                _ => {}
            }
        }
        if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, sym_id) {
            for eff in &decl.declared {
                let cat = format!("{:?}", eff.effect);
                let sym_qname = id_map
                    .get(sym_id)
                    .map(|s| s.qname.clone())
                    .unwrap_or_default();
                all_effects.push(json!({ "category": cat, "source": sym_qname }));
            }
        }
    }

    // --- Recent git touches for changed files (up to 5) ------------------
    let top_files: Vec<(String, usize)> = touched_files.iter().take(5).cloned().collect();
    let recently_touched = git_recent_touches_pub(&top_files, args.git_depth);

    // --- Staleness warnings -----------------------------------------------
    let dirty = git_dirty_files();
    let stale_symbols: Vec<&str> = changed_files
        .iter()
        .filter(|f| dirty.contains(f.as_str()))
        .map(String::as_str)
        .collect();

    // --- Test-gap detection -----------------------------------------------
    let test_gap = affected_test_rows.is_empty();
    let proposed_test_path = test_gap
        .then(|| changed_files.first().map(|f| propose_test_path(f)))
        .flatten();

    let focus = intent_focus(intent);
    let out = json!({
        "sha": args.sha,
        "intent": if intent.is_empty() { Value::Null } else { json!(intent) },
        "focus": if focus.is_empty() { Value::Null } else { json!(focus) },
        "changed_files": changed_files,
        "touched_symbols": by_layer,
        "caller_count": caller_rows.len(),
        "test_count": affected_test_rows.len(),
        "test_gap": test_gap,
        "proposed_test_path": proposed_test_path,
        "stale_symbols": stale_symbols,
        "callers": caller_rows,
        "affected_tests": affected_test_rows,
        "invariants": all_invariants,
        "hazards": all_hazards,
        "effects": all_effects,
        "recently_touched": recently_touched,
    });
    let out = if args.agent {
        let max_list = (args.agent_budget / 500).max(3).min(20);
        let trimmed = trim_for_agent(&out, max_list);
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

/// Run `git diff --name-only <sha>..HEAD` and return the list of changed paths.
fn git_changed_files(sha: &str) -> Vec<String> {
    let output = Proc::new("git")
        .args(["diff", "--name-only", &format!("{sha}..HEAD")])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        }
        _ => vec![],
    }
}
