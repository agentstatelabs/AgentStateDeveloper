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
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine,
    FeedbackStore, FtsFilters, IndexStore, LedgerKind, LedgerStore, apply_feedback_adjustments,
    classify_layer_sym, confidence_scores, derive_cold_hints, detect_ambiguous_tokens,
    detect_possible_misses, estimate_tokens, explain_match, extract_summary, find_candidates,
    gather_recency, git_dirty_files, intent_focus, intent_layer_order, load_layer_overrides,
    find_indexed_test_files, parse_intent, parse_query, propose_test_path, resolve_scope,
    result_bucket, stale_warning,
    symbol_tier, trim_for_agent,
};

use crate::commands::{
    graph::build_id_map,
    impact::git_recent_touches_pub,
};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct PrepareChangeArgs {
    /// Free-form description of the intended change (treated as a search query).
    pub description: String,

    /// Number of top entry-point symbols to expand (default: 10).
    #[arg(long, default_value = "10")]
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

    /// Comma-separated terms to exclude. Also supports inline minus-prefix
    /// syntax in the description, e.g. "drift playhead -sample -waveform".
    #[arg(long)]
    pub exclude: Option<String>,

    /// Comma-separated glob patterns to restrict results to specific paths.
    #[arg(long)]
    pub paths: Option<String>,

    /// Named scope alias from .asd/scopes.toml, e.g. --scope drift-pad.
    #[arg(long)]
    pub scope: Option<String>,

    /// Inject active task context to enrich the search query.
    /// Pass the description of the current CTX task (or any relevant context)
    /// and its tokens are appended to the description before candidate scoring.
    /// Example: --task-context "$(ctx task show --format plain)"
    #[arg(long)]
    pub task_context: Option<String>,
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

    let (mut tokens, mut exclusions) = parse_query(&args.description);
    if let Some(ref excl) = args.exclude {
        for term in excl.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
            exclusions.push(term);
        }
    }
    // Enrich query with active task context (CTX task description, etc.).
    if let Some(ref ctx_text) = args.task_context {
        let (ctx_tokens, _) = parse_query(ctx_text);
        for t in ctx_tokens {
            if !tokens.contains(&t) {
                tokens.push(t);
            }
        }
    }
    if tokens.is_empty() {
        println!("{}", json!({"description": args.description, "entry_points": {}}));
        return Ok(());
    }

    let mut paths_filter: Vec<String> = Vec::new();
    if let Some(ref scope) = args.scope {
        paths_filter.extend(resolve_scope(scope, &cfg.db_path));
    }
    if let Some(ref paths) = args.paths {
        paths_filter.extend(paths.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
    }
    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
        exclude_terms: exclusions,
        paths_filter,
    };

    let mut candidates = find_candidates(
        &engine,
        &cfg.db_path,
        &args.description,
        &tokens,
        &filters,
        &ledger_store,
        &index_store,
        args.depth,
    );

    // Apply durable feedback adjustments (Useful/Noisy/WrongLayer verdicts).
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };
    let feedback_verdicts = feedback_store.flat_verdicts(&engine.ref_name).unwrap_or_default();
    apply_feedback_adjustments(&engine, &index_store, &args.description, &mut candidates, &feedback_verdicts);

    // Recency pass (one git call for all files).
    let recency = gather_recency(200, 14.0);

    // ---- Build entry points + aggregate data ----------------------------
    let layer_order = intent_layer_order(intent);
    let raw_scores: Vec<f64> = candidates.iter().map(|(s, _)| *s).collect();
    let confidences = confidence_scores(&raw_scores);
    let mut by_layer: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut design_invariants: Vec<Value> = Vec::new();
    let mut known_hazards: Vec<Value> = Vec::new();
    let mut validation_scenarios_ledger: Vec<Value> = Vec::new();
    let mut effects_summary: Vec<Value> = Vec::new();
    let mut seen_inv: HashSet<String> = HashSet::new();
    let mut seen_vs: HashSet<String> = HashSet::new();
    let mut seen_effect: HashSet<String> = HashSet::new();
    // Only include effects from symbols scoring ≥25% of the top score to reduce noise.
    let effect_score_floor = candidates.first().map(|(s, _)| s * 0.25).unwrap_or(0.0);

    // likely_edit_files: file → (score, layer, recency)
    let mut file_scores: Vec<(f64, String, String, Option<f64>, bool)> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();

    // Top entry point symbol id for impact BFS.
    let mut top_sym_id: Option<String> = None;

    for (idx, (score, qname)) in candidates.iter().enumerate() {
        let conf = confidences.get(idx).copied().unwrap_or(0.5);
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let tier = symbol_tier(&sym.file);
        let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
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
                LedgerKind::ValidationScenario => {
                    if seen_vs.insert(key) {
                        validation_scenarios_ledger.push(json!({
                            "scenario": entry.summary,
                            "source": sym.qname,
                        }));
                    }
                }
                _ => {}
            }
        }

        // Effects — only from sufficiently-scoring candidates to reduce noise.
        // Low-signal effects (throw, random, log, pure, time.read, time.sleep)
        // are suppressed unless they are the only effects declared on the symbol.
        if *score >= effect_score_floor {
            if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, &sym.symbol_id) {
                let has_high_signal = decl.declared.iter().any(|e| !e.effect.is_low_signal());
                for eff in &decl.declared {
                    if has_high_signal && eff.effect.is_low_signal() { continue; }
                    let cat = eff.effect.as_str().to_string();
                    let key = format!("{}:{}", cat, sym.qname);
                    if seen_effect.insert(key) {
                        effects_summary.push(json!({
                            "category": cat,
                            "source": sym.qname,
                        }));
                    }
                }
            }
        }

        // Add to by_layer.
        let has_ledger = !entries.is_empty();
        let match_reasons = explain_match(&sym, &tokens, &entries, hot);
        let bucket = result_bucket(&sym.file, &match_reasons, has_ledger, hot);
        let ep_val = json!({
            "score": score,
            "confidence": conf,
            "bucket": bucket,
            "match_reasons": match_reasons,
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
    let dirty_files = git_dirty_files();
    let likely_edit_files: Vec<Value> = file_scores
        .iter()
        .map(|(score, file, layer, days, hot)| {
            let fl = file.to_lowercase();
            let file_role = if fl.contains("/example") || fl.contains("/examples")
                || fl.contains("/sample") || fl.contains("/demo")
            { "example" } else if fl.contains("/test") || fl.contains("/spec")
                || fl.contains("_test.") || fl.contains("spec.") || fl.ends_with("tests.swift")
            { "test" } else if fl.contains("/reference") || fl.contains("/doc")
                || fl.contains("readme") || fl.ends_with(".md")
            { "reference" } else { "impl" };
            let conflict_risk = dirty_files.contains(file.as_str());
            json!({
                "file": file,
                "layer": layer,
                "score": score,
                "last_touched_days": days,
                "hot": hot,
                "file_role": file_role,
                "conflict_risk": conflict_risk,
            })
        })
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
                        // Use both qname words and doc comment words for behavioral matching
                        // so "test_plays_silence_at_loop_end" and a doc saying "verifies
                        // loop boundary" both surface the relevant invariant.
                        let qname_words: Vec<String> = s.qname
                            .split(|c: char| !c.is_alphabetic())
                            .filter(|t| t.len() > 2)
                            .map(|t| t.to_lowercase())
                            .collect();
                        let doc_words: Vec<String> = s.doc.as_deref().unwrap_or("")
                            .split(|c: char| !c.is_alphabetic())
                            .filter(|t| t.len() > 2)
                            .map(|t| t.to_lowercase())
                            .collect();
                        let test_tokens: Vec<&str> = qname_words.iter()
                            .chain(doc_words.iter())
                            .map(|s| s.as_str())
                            .collect();
                        let covers: Vec<&str> = design_invariants.iter()
                            .filter_map(|inv| inv.get("summary").and_then(Value::as_str))
                            .filter(|summary| {
                                let sl = summary.to_lowercase();
                                test_tokens.iter().any(|t| sl.contains(*t))
                            })
                            .collect();
                        affected_tests.push(json!({
                            "qname": s.qname,
                            "file": s.file,
                            "line": s.start.line,
                            "covers_invariants": covers,
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

    // --- Staleness warnings -----------------------------------------------
    let dirty = git_dirty_files();
    let stale_symbols: Vec<&str> = file_scores
        .iter()
        .filter(|(_, f, _, _, _)| dirty.contains(f.as_str()))
        .map(|(_, f, _, _, _)| f.as_str())
        .collect();

    // --- Test-gap detection -----------------------------------------------
    let test_gap = affected_tests.is_empty();
    // Try to find a real indexed test file before falling back to a suggested path.
    let proposed_test_path = test_gap.then(|| {
        let source = file_scores.first().map(|(_, f, _, _, _)| f.as_str()).unwrap_or("");
        if source.is_empty() { return None; }
        let real = find_indexed_test_files(&cfg.db_path, source);
        if real.is_empty() {
            Some(format!("no known test target (suggested: {})", propose_test_path(source)))
        } else {
            Some(real.join(", "))
        }
    }).flatten();
    let suggested_test_coverage: Vec<String> = if test_gap {
        let mut hints: Vec<String> = design_invariants.iter()
            .filter_map(|inv| inv.get("summary").and_then(Value::as_str))
            .map(|s| s.to_string())
            .collect();
        for eff in &effects_summary {
            if let Some(cat) = eff.get("category").and_then(Value::as_str) {
                let hint = format!("verify {} after change", cat.to_lowercase());
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
            }
        }
        // Cold-start fallback: when no invariants are recorded, derive hints
        // from the top candidate symbol's name, signature, and doc comment.
        if design_invariants.is_empty() {
            if let Some((_, qname)) = candidates.first() {
                if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, qname) {
                    for h in derive_cold_hints(&sym.qname, sym.signature.as_deref(), sym.doc.as_deref()) {
                        if !hints.contains(&h) {
                            hints.push(h);
                        }
                    }
                }
            }
        }
        hints
    } else {
        vec![]
    };

    const CONSTRAINT_WORDS: &[&str] = &[
        "must", "never", "shall", "always", "only", "cannot", "no ", "not ",
        "require", "ensure", "prevent", "guarantee", "invariant", "forbidden",
    ];
    let scenario_tests: Vec<Value> = design_invariants.iter()
        .filter_map(|inv| inv.get("summary").and_then(Value::as_str))
        .filter(|s| {
            let sl = s.to_lowercase();
            CONSTRAINT_WORDS.iter().any(|w| sl.contains(w))
        })
        .map(|s| json!(s))
        .collect();

    let ambiguous_terms = detect_ambiguous_tokens(&tokens, &cfg.db_path, &filters);
    let layers_present: std::collections::HashSet<&str> = file_scores.iter()
        .map(|(_, _, layer, _, _)| layer.as_str())
        .collect();
    // Suppress possible-miss warnings when the user explicitly narrowed scope.
    let scope_narrowed = !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
    let possible_misses = if scope_narrowed {
        vec![]
    } else {
        detect_possible_misses(&args.description, &layers_present, file_scores.len())
    };

    // T1: safe-change recipe — actionable sections for an agent or developer.
    // T4: manually_validate includes concrete ValidationScenario entries.
    let recipe_inspect: Vec<Value> = file_scores.iter()
        .map(|(score, file, layer, days, hot)| json!({
            "file": file, "layer": layer, "score": score,
            "last_touched_days": days, "hot": hot,
        }))
        .collect();
    let recipe_preserve: Vec<Value> = design_invariants.iter()
        .map(|inv| json!({ "constraint": inv["summary"], "source": inv["source"], "kind": "invariant" }))
        .chain(known_hazards.iter().map(|h| json!({ "constraint": h["summary"], "source": h["source"], "kind": "hazard" })))
        .collect();
    // edit: only impl files — example/reference/demo files move to recipe_reference
    let recipe_edit: Vec<Value> = likely_edit_files.iter()
        .filter(|f| f["file_role"].as_str() == Some("impl"))
        .cloned()
        .collect();
    // reference: example/demo/doc files that matched but should not be edited
    let recipe_reference: Vec<Value> = likely_edit_files.iter()
        .filter(|f| matches!(
            f["file_role"].as_str(),
            Some("example") | Some("reference")
        ))
        .cloned()
        .collect();
    let recipe_run: Vec<Value> = affected_tests.iter()
        .map(|t| json!({ "qname": t["qname"], "file": t["file"], "covers_invariants": t["covers_invariants"] }))
        .collect();
    // manually_validate: concrete ValidationScenario entries (T4) + constraint-word invariants + effects
    let mut recipe_manually_validate: Vec<Value> = validation_scenarios_ledger.clone();
    for s in &scenario_tests {
        recipe_manually_validate.push(json!({ "scenario": s, "source": "invariant", "kind": "constraint_check" }));
    }
    for eff in &effects_summary {
        let desc = format!("verify {} side-effect still correct after change",
            eff["category"].as_str().unwrap_or("").to_lowercase());
        recipe_manually_validate.push(json!({ "scenario": desc, "source": eff["source"], "kind": "effect_check" }));
    }
    let safe_change_recipe = json!({
        "inspect": recipe_inspect,
        "preserve": recipe_preserve,
        "edit": recipe_edit,
        "reference_only": recipe_reference,
        "run": recipe_run,
        "manually_validate": recipe_manually_validate,
    });

    let focus = intent_focus(intent);
    let out = json!({
        "description": args.description,
        "task_context": args.task_context,
        "intent": if intent.is_empty() { Value::Null } else { json!(intent) },
        "focus": if focus.is_empty() { Value::Null } else { json!(focus) },
        "ambiguous_terms": ambiguous_terms,
        "possible_misses": possible_misses,
        "scope_narrowed": scope_narrowed,
        "safe_change_recipe": safe_change_recipe,
        "design_invariants": design_invariants,
        "known_hazards": known_hazards,
        "validation_scenarios": validation_scenarios_ledger,
        "entry_points": { "by_layer": ordered_by_layer },
        "likely_edit_files": likely_edit_files,
        "affected_tests": affected_tests,
        "test_gap": test_gap,
        "proposed_test_path": proposed_test_path,
        "suggested_test_coverage": suggested_test_coverage,
        "scenario_tests": scenario_tests,
        "stale_symbols": stale_symbols,
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
