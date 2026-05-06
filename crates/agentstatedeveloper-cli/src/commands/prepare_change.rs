//! `asd prepare-change "<description>"` — agent-ready context in one call.
//!
//! Composes investigate + impact + checklist into a single compact JSON package:
//!   design_invariants  — constraints that must hold across the change
//!   known_hazards      — ledger hazards from matching symbols
//!   entry_points       — by_layer grouped map of top matching symbols
//!   likely_edit_files  — ranked unique files with recency annotation
//!   affected_tests     — test callers within BFS depth of top entry point
//!   effects_summary    — declared effects from top entry points
//!   recently_touched   — per-file git log from the top entry point's file

use std::collections::{HashSet, VecDeque};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, FtsFilters, IndexStore,
    LedgerKind, LedgerStore, classify_layer, estimate_tokens, extract_summary, gather_recency,
    intent_focus, intent_layer_order, load_layer_overrides, parse_intent, stale_warning,
    symbol_tier, trim_for_agent,
};

use crate::commands::{
    graph::build_id_map,
    impact::git_recent_touches_pub,
    investigate::find_candidates,
    search::query_tokens,
};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct PrepareChangeArgs {
    /// Free-form description of the intended change (treated as a search query).
    pub description: String,

    /// Number of top entry-point symbols to expand (default: 7).
    #[arg(long, default_value = "7")]
    pub depth: usize,

    /// Filter by symbol kind.
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by language.
    #[arg(long)]
    pub language: Option<String>,

    /// Include test-file symbols as entry-point candidates.
    #[arg(long)]
    pub include_tests: bool,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Adjust output for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    #[arg(long)]
    pub intent: Option<String>,

    /// BFS depth for finding affected tests from the top entry point (default: 2).
    #[arg(long, default_value = "2")]
    pub test_depth: usize,

    /// Number of recent git commits to scan per file (default: 10).
    #[arg(long, default_value = "10")]
    pub git_depth: usize,

    /// Emit token-budgeted JSON for LLM consumption. Trims bodies,
    /// collapses low-signal fields, adds token_estimate.
    #[arg(long)]
    pub agent: bool,

    /// Token budget when --agent is set (default: 8000).
    #[arg(long, default_value = "8000")]
    pub agent_budget: usize,
}

pub fn run(cfg: &Config, args: PrepareChangeArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let id_map = build_id_map(&engine);

    let tokens = query_tokens(&args.description);
    if tokens.is_empty() {
        println!("{}", json!({"description": args.description, "entry_points": {}}));
        return Ok(());
    }

    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
    };

    let candidates = find_candidates(
        &engine,
        &cfg.db_path,
        &args.description,
        &tokens,
        &filters,
        &ledger_store,
        &index_store,
        args.depth,
    );

    // Recency pass (one git call for all files).
    let recency = gather_recency(200, 14.0);

    // ---- Build entry points + aggregate data ----------------------------
    let layer_order = intent_layer_order(intent);
    let mut by_layer: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut design_invariants: Vec<Value> = Vec::new();
    let mut known_hazards: Vec<Value> = Vec::new();
    let mut effects_summary: Vec<Value> = Vec::new();
    let mut seen_inv: HashSet<String> = HashSet::new();
    let mut seen_effect: HashSet<String> = HashSet::new();

    // likely_edit_files: file → (score, layer, recency)
    let mut file_scores: Vec<(f64, String, String, Option<f64>, bool)> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();

    // Top entry point symbol id for impact BFS.
    let mut top_sym_id: Option<String> = None;

    for (score, qname) in &candidates {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let tier = symbol_tier(&sym.file);
        let layer = classify_layer(&sym.file, tier, &layer_overrides);
        let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
        let rec = recency.get(&sym.file);
        let last_touched_days = rec.and_then(|r| r.last_touched_days);
        let hot = rec.map(|r| r.hot).unwrap_or(false);

        if top_sym_id.is_none() {
            top_sym_id = Some(sym.symbol_id.clone());
        }

        // File tracking.
        if seen_files.insert(sym.file.clone()) {
            file_scores.push((*score, sym.file.clone(), layer.to_string(), last_touched_days, hot));
        }

        // Ledger entries.
        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();
        for entry in &entries {
            let key = entry.summary.clone();
            match entry.kind {
                LedgerKind::Invariant => {
                    if seen_inv.insert(key) {
                        design_invariants.push(json!({
                            "summary": entry.summary,
                            "source": sym.qname,
                        }));
                    }
                }
                LedgerKind::Hazard => {
                    known_hazards.push(json!({
                        "summary": entry.summary,
                        "source": sym.qname,
                    }));
                }
                _ => {}
            }
        }

        // Effects.
        if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, &sym.symbol_id) {
            for eff in &decl.declared {
                let cat = format!("{:?}", eff.effect);
                let key = format!("{}:{}", cat, sym.qname);
                if seen_effect.insert(key) {
                    effects_summary.push(json!({
                        "category": cat,
                        "source": sym.qname,
                    }));
                }
            }
        }

        // Add to by_layer.
        let ep_val = json!({
            "score": score,
            "qname": sym.qname,
            "file": sym.file,
            "line": sym.start.line,
            "layer": layer,
            "summary": summary,
            "last_touched_days": last_touched_days,
            "hot": hot,
        });
        by_layer
            .entry(layer.to_string())
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .unwrap()
            .push(ep_val);
    }

    // Reorder by_layer keys according to layer_order.
    let mut ordered_by_layer: serde_json::Map<String, Value> = serde_json::Map::new();
    for lk in layer_order {
        if let Some(v) = by_layer.remove(*lk) {
            ordered_by_layer.insert(lk.to_string(), v);
        }
    }

    // Sort likely_edit_files: score desc, then hot files first.
    file_scores.sort_by(|a, b| {
        b.4.cmp(&a.4) // hot first
            .then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
    });
    let likely_edit_files: Vec<Value> = file_scores
        .iter()
        .map(|(score, file, layer, days, hot)| json!({
            "file": file,
            "layer": layer,
            "score": score,
            "last_touched_days": days,
            "hot": hot,
        }))
        .collect();

    // ---- Affected tests via BFS from the top entry point ----------------
    let mut affected_tests: Vec<Value> = Vec::new();
    if let Some(start_id) = top_sym_id {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut seen_test_names: HashSet<String> = HashSet::new();
        visited.insert(start_id.clone());
        queue.push_back((start_id, 0));
        while let Some((sid, depth)) = queue.pop_front() {
            if depth >= args.test_depth { continue; }
            let callers = index_store
                .get_callers(&engine.ref_name, &sid)
                .unwrap_or_default();
            for cid in callers {
                if visited.contains(&cid) { continue; }
                visited.insert(cid.clone());
                if let Some(s) = id_map.get(&cid) {
                    if symbol_tier(&s.file) == 2 && seen_test_names.insert(s.qname.clone()) {
                        affected_tests.push(json!({
                            "qname": s.qname,
                            "file": s.file,
                            "line": s.start.line,
                        }));
                    }
                    if depth + 1 < args.test_depth {
                        queue.push_back((cid, depth + 1));
                    }
                }
            }
        }
    }

    // ---- Recent git touches for the top files (up to 3) ----------------
    let top_files: Vec<(String, usize)> = file_scores
        .iter()
        .take(3)
        .map(|(_, f, _, _, _)| (f.clone(), 0))
        .collect();
    let recently_touched = git_recent_touches_pub(&top_files, args.git_depth);

    let focus = intent_focus(intent);
    let out = json!({
        "description": args.description,
        "intent": if intent.is_empty() { Value::Null } else { json!(intent) },
        "focus": if focus.is_empty() { Value::Null } else { json!(focus) },
        "design_invariants": design_invariants,
        "known_hazards": known_hazards,
        "entry_points": { "by_layer": ordered_by_layer },
        "likely_edit_files": likely_edit_files,
        "affected_tests": affected_tests,
        "effects_summary": effects_summary,
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
