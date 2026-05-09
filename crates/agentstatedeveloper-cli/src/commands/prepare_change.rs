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

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine,
    FeedbackStore, FtsFilters, IndexStore, LedgerKind, LedgerStore, SearchFtsDb,
    apply_feedback_adjustments,
    classify_layer_sym, confidence_scores, derive_cold_hints, detect_ambiguous_tokens,
    detect_possible_misses, estimate_tokens, explain_match, extract_summary, find_candidates,
    find_indexed_test_files, gather_recency, git_dirty_files, glob_match, intent_focus,
    intent_layer_order, load_layer_overrides, parse_intent, parse_query, propose_test_path,
    resolve_scope, result_bucket, stale_warning, symbol_tier, trim_for_agent,
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

    /// Print per-file classification reasoning: file_role, surface_demoted,
    /// domain_anchor_retained, matched stem words, and the rule that won.
    #[arg(long)]
    pub debug_classification: bool,
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
    // Auto-loads from CTXONE_PLAN / CTXONE_TASK env vars when --task-context is absent.
    let auto_ctx_plan = std::env::var("CTXONE_PLAN").ok().filter(|s| !s.is_empty());
    let auto_ctx_task = std::env::var("CTXONE_TASK").ok().filter(|s| !s.is_empty());
    let ctx_text = args.task_context.clone().or_else(|| {
        let parts: Vec<&str> = [
            auto_ctx_plan.as_deref(),
            auto_ctx_task.as_deref(),
        ].iter().filter_map(|x| *x).collect();
        if parts.is_empty() { None } else { Some(parts.join(" ")) }
    });
    if let Some(ref ctx) = ctx_text {
        let (ctx_tokens, _) = parse_query(ctx);
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
    let _has_paths_filter = !paths_filter.is_empty();
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
            let file_role = classify_file_role(file);
            let conflict_risk = dirty_files.contains(file.as_str());
            let conflict_detail = if conflict_risk { explain_conflict_risk(file) } else { None };
            json!({
                "file": file,
                "layer": layer,
                "score": score,
                "last_touched_days": days,
                "hot": hot,
                "file_role": file_role,
                "conflict_risk": conflict_risk,
                "conflict_detail": conflict_detail,
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

    // ---- Blast-radius: caller/callee layer distribution + concrete call chains ----
    let blast_radius = {
        let mut caller_layers: HashMap<String, usize> = HashMap::new();
        let mut callee_layers: HashMap<String, usize> = HashMap::new();
        let mut total_callers = 0usize;
        let mut total_callees = 0usize;
        let top_sids: Vec<String> = candidates.iter().take(5)
            .filter_map(|(_, q)| index_store.get_symbol_by_qname(&engine.ref_name, q).ok().flatten())
            .map(|s| s.symbol_id)
            .collect();

        // t-003: BFS tracking paths so we can emit concrete caller chains.
        // Each path is stored root-first: [outer_caller, ..., direct_caller, our_symbol].
        let mut top_caller_chains: Vec<Vec<String>> = Vec::new();

        for sid in &top_sids {
            let anchor_qname = id_map.get(sid).map(|s| s.qname.clone()).unwrap_or_default();
            let mut visited: HashSet<String> = HashSet::new();
            // Queue: (current_id, path_from_this_node_to_anchor)
            let mut q: VecDeque<(String, Vec<String>)> = VecDeque::new();
            visited.insert(sid.clone());
            q.push_back((sid.clone(), vec![anchor_qname.clone()]));
            while let Some((cid, path)) = q.pop_front() {
                if path.len() > 4 { continue; }
                for caller_id in index_store.get_callers(&engine.ref_name, &cid).unwrap_or_default() {
                    if visited.insert(caller_id.clone()) {
                        if let Some(sym) = id_map.get(&caller_id) {
                            let tier = symbol_tier(&sym.file);
                            let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
                            *caller_layers.entry(layer.to_string()).or_default() += 1;
                            total_callers += 1;
                            // Build path: prepend this caller.
                            let mut new_path = vec![sym.qname.clone()];
                            new_path.extend_from_slice(&path);
                            if top_caller_chains.len() < 5 {
                                top_caller_chains.push(new_path.clone());
                            }
                            q.push_back((caller_id, new_path));
                        }
                    }
                }
            }
            for callee_id in index_store.get_callees(&engine.ref_name, sid).unwrap_or_default() {
                if let Some(sym) = id_map.get(&callee_id) {
                    let tier = symbol_tier(&sym.file);
                    let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
                    *callee_layers.entry(layer.to_string()).or_default() += 1;
                    total_callees += 1;
                }
            }
        }
        json!({
            "total_callers": total_callers,
            "total_callees": total_callees,
            "caller_layer_distribution": caller_layers,
            "callee_layer_distribution": callee_layers,
            "top_caller_chains": top_caller_chains,
        })
    };

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
    // t-001: broad_query = at least half the query tokens are flagged as ambiguous.
    let broad_query = !ambiguous_terms.is_empty() && {
        let amb_set: HashSet<&str> = ambiguous_terms.iter().map(|s| s.as_str()).collect();
        let amb_count = tokens.iter().filter(|t| amb_set.contains(t.as_str())).count();
        amb_count * 2 >= tokens.len().max(1)
    };

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

    // Likely omitted: files that score well in an unscoped search but were filtered
    // by the active scope/path filter. We run a quick unscoped FTS pass to find them,
    // then merge with caller/callee-based omissions from the graph.
    let fts_omitted_files: Vec<Value> = if scope_narrowed && !filters.paths_filter.is_empty() {
        let unscoped_filters = FtsFilters {
            kind: filters.kind.clone(),
            language: filters.language.clone(),
            include_tests: filters.include_tests,
            exclude_terms: filters.exclude_terms.clone(),
            paths_filter: vec![],  // no path filter
        };
        let unscoped_hits = SearchFtsDb::open(&cfg.db_path)
            .ok()
            .filter(|fts| fts.has_data())
            .and_then(|fts| fts.search(&args.description, &unscoped_filters, 20).ok())
            .unwrap_or_default();
        let scoped_file_set: std::collections::HashSet<&str> = file_scores.iter()
            .map(|(_, f, _, _, _)| f.as_str())
            .collect();
        unscoped_hits.iter()
            .filter(|h| !filters.paths_filter.iter().any(|p| glob_match(p, &h.file)))
            .filter(|h| !scoped_file_set.contains(h.file.as_str()))
            .take(5)
            .map(|h| {
                let dir = std::path::Path::new(&h.file)
                    .parent()
                    .map(|d| format!("{}/**", d.to_string_lossy()))
                    .unwrap_or_else(|| h.file.clone());
                json!({
                    "file": h.file,
                    "relation": "unscoped_fts_match",
                    "score": h.bm25_score,
                    "suggested_paths": dir,
                    "note": "matched query in broad search but excluded by current scope",
                })
            })
            .collect()
    } else {
        vec![]
    };

    // Graph-based omissions: callers/callees outside scope.
    let likely_omitted_files: Vec<Value> = if scope_narrowed && !filters.paths_filter.is_empty() {
        let top_sids_omit: Vec<String> = candidates.iter().take(3)
            .filter_map(|(_, q)| index_store.get_symbol_by_qname(&engine.ref_name, q).ok().flatten())
            .map(|s| s.symbol_id)
            .collect();
        let mut omitted: Vec<Value> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        for sid in &top_sids_omit {
            let anchor_qname = id_map.get(sid).map(|s| s.qname.clone()).unwrap_or_default();
            // Check callers outside scope.
            for caller_id in index_store.get_callers(&engine.ref_name, sid).unwrap_or_default() {
                if let Some(sym) = id_map.get(&caller_id) {
                    let in_scope = filters.paths_filter.iter().any(|p| glob_match(p, &sym.file));
                    if !in_scope && seen_files.insert(sym.file.clone()) {
                        let dir = std::path::Path::new(&sym.file)
                            .parent()
                            .map(|d| format!("{}/**", d.to_string_lossy()))
                            .unwrap_or_else(|| sym.file.clone());
                        omitted.push(json!({
                            "file": sym.file,
                            "relation": "caller",
                            "of_symbol": anchor_qname,
                            "suggested_paths": dir,
                        }));
                    }
                }
            }
            // Check callees outside scope.
            for callee_id in index_store.get_callees(&engine.ref_name, sid).unwrap_or_default() {
                if let Some(sym) = id_map.get(&callee_id) {
                    let in_scope = filters.paths_filter.iter().any(|p| glob_match(p, &sym.file));
                    if !in_scope && seen_files.insert(sym.file.clone()) {
                        let dir = std::path::Path::new(&sym.file)
                            .parent()
                            .map(|d| format!("{}/**", d.to_string_lossy()))
                            .unwrap_or_else(|| sym.file.clone());
                        omitted.push(json!({
                            "file": sym.file,
                            "relation": "callee",
                            "of_symbol": anchor_qname,
                            "suggested_paths": dir,
                        }));
                    }
                }
            }
        }
        omitted
    } else {
        vec![]
    };
    // Merge graph-based and FTS-based omissions, deduplicating by file.
    let mut seen_omit: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut likely_omitted_files = likely_omitted_files;
    for item in fts_omitted_files {
        if let Some(f) = item["file"].as_str() {
            if seen_omit.insert(f.to_string()) {
                likely_omitted_files.push(item);
            }
        }
    }
    for item in &likely_omitted_files {
        if let Some(f) = item["file"].as_str() {
            seen_omit.insert(f.to_string());
        }
    }

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
    // t-005: Find files where a matching symbol has a WrongLayer verdict for
    // the current query family. Those impl files are demoted to recipe_reference.
    use std::collections::HashSet as _HSet;
    let wrong_layer_files: _HSet<String> = {
        let all_fb = feedback_store.list_all(&engine.ref_name).unwrap_or_default();
        let desc_norm = args.description.to_lowercase();
        let desc_tokens: std::collections::HashSet<String> = desc_norm
            .split(|c: char| !c.is_alphabetic())
            .filter(|t: &&str| t.len() > 2)
            .map(|t| t.to_string())
            .collect();
        let mut wl_files = _HSet::new();
        for entry in &all_fb {
            if !matches!(entry.verdict, agentstatedeveloper_core::FeedbackVerdict::WrongLayer) { continue; }
            // Query-family match: share at least one token.
            let fb_tokens: std::collections::HashSet<String> = entry.query
                .split(|c: char| !c.is_alphabetic())
                .filter(|t: &&str| t.len() > 2)
                .map(|t: &str| t.to_string())
                .collect();
            let overlaps = desc_tokens.iter().any(|t| fb_tokens.contains(t));
            if !overlaps { continue; }
            // Look up the symbol's file via qname.
            if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, &entry.symbol_qname) {
                wl_files.insert(sym.file);
            }
        }
        wl_files
    };
    // t-005: Build a map of file → test files that cover it (from affected_tests).
    let mut file_to_tests: HashMap<String, Vec<String>> = HashMap::new();
    for test in &affected_tests {
        if let Some(test_file) = test["file"].as_str() {
            for (_, file, _, _, _) in &file_scores {
                let entry = file_to_tests.entry(file.clone()).or_default();
                let tf = test_file.to_string();
                if !entry.contains(&tf) { entry.push(tf); }
            }
        }
    }

    // edit: only impl files not flagged as wrong-layer or view-only-on-broad-query.
    // Rendering surfaces (Canvas, Overlay, Layer etc.) are demoted unconditionally
    // unless the query explicitly names them. Other view-like files are demoted only
    // on broad queries. A file is retained when ≥2 of its stem words appear in the
    // query tokens (generalised domain anchor — no hardcoded token list needed).
    let recipe_edit: Vec<Value> = likely_edit_files.iter()
        .filter(|f| {
            let file = f["file"].as_str().unwrap_or("");
            let layer = f["layer"].as_str().unwrap_or("");
            let names_file = query_names_file(&tokens, file);
            // Domain anchor: retain when query shares ≥2 stem words with the file.
            let stem_words = split_camel_lower(
                std::path::Path::new(file).file_stem().and_then(|n| n.to_str()).unwrap_or("")
            );
            let domain_overlap = stem_words.iter()
                .filter(|w| tokens.iter().any(|t| t == *w))
                .count();
            let has_domain_anchor = domain_overlap >= 2;
            // Rendering surfaces: unconditional demotion unless query names them.
            let surface_demote = is_rendering_surface(file) && !names_file;
            // View-like: demote on broad queries unless domain anchor present.
            let broad_demote = broad_query
                && is_view_like_file(file, layer)
                && !names_file
                && !has_domain_anchor;
            let demote = !wrong_layer_files.contains(file) && (surface_demote || broad_demote);
            f["file_role"].as_str() == Some("impl") && !wrong_layer_files.contains(file) && !demote
        })
        .map(|f| {
            // t-005: attach covering tests and run command to each edit entry.
            let file = f["file"].as_str().unwrap_or("");
            let mut indexed = find_indexed_test_files(&cfg.db_path, file);
            if let Some(extra) = file_to_tests.get(file) {
                for t in extra {
                    if !indexed.contains(t) { indexed.push(t.clone()); }
                }
            }
            let run_cmd = detect_test_command(file);
            let mut entry = f.clone();
            if let Some(obj) = entry.as_object_mut() {
                // Use explicit none-found metadata instead of empty arrays/null
                // so agents can distinguish "no tests found" from "not checked".
                let covered = if indexed.is_empty() {
                    json!([{"note": "none found — no test callers discovered for this file"}])
                } else {
                    json!(indexed)
                };
                let run = if run_cmd.is_none() {
                    json!("none found — add test targets for this file")
                } else {
                    json!(run_cmd)
                };
                obj.insert("covered_by_tests".into(), covered);
                obj.insert("run_command".into(), run);
            }
            entry
        })
        .collect();
    // reference: example/doc files + WrongLayer demotions + view/surface demotions.
    let recipe_reference: Vec<Value> = likely_edit_files.iter()
        .filter(|f| {
            let file = f["file"].as_str().unwrap_or("");
            let layer = f["layer"].as_str().unwrap_or("");
            let names_file = query_names_file(&tokens, file);
            let stem_words = split_camel_lower(
                std::path::Path::new(file).file_stem().and_then(|n| n.to_str()).unwrap_or("")
            );
            let domain_overlap = stem_words.iter()
                .filter(|w| tokens.iter().any(|t| t == *w))
                .count();
            let has_domain_anchor = domain_overlap >= 2;
            let surface_demote = is_rendering_surface(file) && !names_file;
            let broad_demote = broad_query && is_view_like_file(file, layer)
                && !names_file && !has_domain_anchor;
            let view_demote = surface_demote || broad_demote;
            matches!(f["file_role"].as_str(), Some("example") | Some("reference"))
                || (f["file_role"].as_str() == Some("impl") && wrong_layer_files.contains(file))
                || (f["file_role"].as_str() == Some("impl") && view_demote)
        })
        .cloned()
        .collect();
    // t-002: include exact build/test commands for each affected test file.
    let mut recipe_run: Vec<Value> = affected_tests.iter()
        .map(|t| {
            let file = t["file"].as_str().unwrap_or("");
            let cmd = detect_test_command(file);
            json!({
                "qname": t["qname"],
                "file": file,
                "covers_invariants": t["covers_invariants"],
                "run_command": cmd,
            })
        })
        .collect();
    // If no test files discovered, still emit a command for the top impl file.
    if recipe_run.is_empty() {
        if let Some((_, top_file, _, _, _)) = file_scores.first() {
            if let Some(cmd) = detect_test_command(top_file) {
                recipe_run.push(json!({
                    "qname": Value::Null,
                    "file": top_file,
                    "covers_invariants": [],
                    "run_command": cmd,
                }));
            }
        }
    }
    // t-005: manually_validate with concrete step/expected pairs.
    let mut recipe_manually_validate: Vec<Value> = validation_scenarios_ledger.iter()
        .map(|vs| {
            let text = vs["scenario"].as_str().unwrap_or("");
            let scenario = invariant_to_scenario(text);
            json!({
                "source": vs["source"],
                "kind": "validation_scenario",
                "step": scenario["step"],
                "expected": scenario["expected"],
                "raw": text,
            })
        })
        .collect();
    for inv in &design_invariants {
        let text = inv["summary"].as_str().unwrap_or("");
        let sl = text.to_lowercase();
        if CONSTRAINT_WORDS.iter().any(|w| sl.contains(w)) {
            let scenario = invariant_to_scenario(text);
            recipe_manually_validate.push(json!({
                "source": inv["source"],
                "kind": "constraint_check",
                "step": scenario["step"],
                "expected": scenario["expected"],
                "raw": text,
            }));
        }
    }
    for eff in &effects_summary {
        let cat = eff["category"].as_str().unwrap_or("").to_lowercase();
        recipe_manually_validate.push(json!({
            "source": eff["source"],
            "kind": "effect_check",
            "step": format!("Execute the code path that triggers {}", cat),
            "expected": format!("{} side-effect behaves correctly after change", cat),
            "raw": format!("verify {} effect", cat),
        }));
    }
    // --debug-classification: per-file reasoning for edit/reference classification.
    let classification_debug: Vec<Value> = if args.debug_classification {
        likely_edit_files.iter().map(|f| {
            let file = f["file"].as_str().unwrap_or("");
            let layer = f["layer"].as_str().unwrap_or("");
            let file_role = f["file_role"].as_str().unwrap_or("unknown");
            let names_file = query_names_file(&tokens, file);
            let stem_words = split_camel_lower(
                std::path::Path::new(file).file_stem().and_then(|n| n.to_str()).unwrap_or("")
            );
            let matched_stem_words: Vec<&str> = stem_words.iter()
                .filter(|w| tokens.iter().any(|t| t == *w))
                .map(|w| w.as_str())
                .collect();
            let domain_overlap = matched_stem_words.len();
            let has_domain_anchor = domain_overlap >= 2;
            let surface_demoted = is_rendering_surface(file) && !names_file;
            let broad_demoted = broad_query && is_view_like_file(file, layer)
                && !names_file && !has_domain_anchor;
            let is_wrong_layer = wrong_layer_files.contains(file);
            let rule_that_won = if file_role == "test" {
                "test"
            } else if is_wrong_layer {
                "wrong-layer → reference"
            } else if surface_demoted {
                "surface → reference"
            } else if broad_demoted {
                "broad-query view → reference"
            } else if file_role == "impl" {
                "edit"
            } else {
                file_role
            };
            json!({
                "file": file,
                "file_role": file_role,
                "surface_demoted": surface_demoted,
                "domain_anchor_retained": has_domain_anchor,
                "matched_stem_words": matched_stem_words,
                "domain_overlap": domain_overlap,
                "names_file": names_file,
                "is_wrong_layer": is_wrong_layer,
                "broad_query": broad_query,
                "rule_that_won": rule_that_won,
            })
        }).collect()
    } else {
        vec![]
    };

    let safe_change_recipe = json!({
        "inspect": recipe_inspect,
        "preserve": recipe_preserve,
        "edit": recipe_edit,
        "reference_only": recipe_reference,
        "run": recipe_run,
        "manually_validate": recipe_manually_validate,
        "blast_radius": blast_radius,
        "likely_omitted_files": likely_omitted_files,
    });

    let focus = intent_focus(intent);
    let ctx_context_val = match (auto_ctx_plan.as_deref(), auto_ctx_task.as_deref()) {
        (None, None) => Value::Null,
        _ => json!({
            "plan": auto_ctx_plan,
            "task": auto_ctx_task,
            "injected": ctx_text.is_some(),
        }),
    };
    let out = json!({
        "description": args.description,
        "task_context": args.task_context,
        "ctx_context": ctx_context_val,
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
        "classification_debug": if args.debug_classification { json!(classification_debug) } else { Value::Null },
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

// ---------------------------------------------------------------------------
// t-001: expanded file role classification
// ---------------------------------------------------------------------------

fn classify_file_role(file: &str) -> &'static str {
    let fl = file.to_lowercase();
    if fl.contains("/test") || fl.contains("/spec")
        || fl.contains("_test.") || fl.contains("spec.")
        || fl.ends_with("tests.swift") || fl.contains("/tests/")
    {
        return "test";
    }
    if fl.contains("/example") || fl.contains("/examples")
        || fl.contains("/sample") || fl.contains("/samples")
        || fl.contains("/demo") || fl.contains("/demos")
    {
        return "example";
    }
    if fl.contains("/fixture") || fl.contains("/fixtures")
        || fl.contains("/seed") || fl.contains("/seeds")
    {
        return "fixture";
    }
    if fl.contains("/script") || fl.contains("/scripts")
        || fl.contains("/tool/") || fl.contains("/tools/")
        || fl.contains("/bin/") || fl.contains("/hack/")
    {
        return "script";
    }
    if fl.contains("/generated") || fl.contains("/gen/")
        || fl.contains(".generated.") || fl.contains(".pb.")
        || fl.contains(".pb.swift") || fl.contains("_generated")
    {
        return "generated";
    }
    if fl.contains("/doc") || fl.contains("/docs")
        || fl.contains("/reference") || fl.contains("readme")
        || fl.ends_with(".md") || fl.ends_with(".rst") || fl.ends_with(".adoc")
    {
        return "reference";
    }
    "impl"
}

// ---------------------------------------------------------------------------
// t-002: detect the test command for a given source file
// ---------------------------------------------------------------------------

fn detect_test_command(file: &str) -> Option<String> {
    use std::path::Path;
    let p = Path::new(file);
    // Walk up to find a recognisable project root marker.
    let mut dir = p.parent()?;
    loop {
        if dir.join("Cargo.toml").exists() {
            // Try to extract the package name for a precise `cargo test -p` command.
            let pkg_name = std::fs::read_to_string(dir.join("Cargo.toml")).ok()
                .and_then(|s| {
                    s.lines()
                        .skip_while(|l| !l.trim_start().starts_with("[package]"))
                        .find(|l| l.trim_start().starts_with("name"))
                        .and_then(|l| l.splitn(2, '=').nth(1))
                        .map(|v| v.trim().trim_matches('"').to_string())
                });
            return Some(match pkg_name {
                Some(name) => format!("cargo test -p {}", name),
                None => "cargo test".to_string(),
            });
        }
        if dir.join("package.json").exists() {
            let has_yarn = dir.join("yarn.lock").exists();
            let has_pnpm = dir.join("pnpm-lock.yaml").exists();
            return Some(if has_pnpm {
                "pnpm test".to_string()
            } else if has_yarn {
                "yarn test".to_string()
            } else {
                "npm test".to_string()
            });
        }
        if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists()
            || dir.join("pytest.ini").exists() || dir.join("setup.cfg").exists()
        {
            let rel = p.strip_prefix(dir).unwrap_or(p).to_string_lossy();
            return Some(format!("pytest {}", rel));
        }
        if dir.join("Gemfile").exists() {
            let rel = p.strip_prefix(dir).unwrap_or(p).to_string_lossy();
            return Some(format!("bundle exec rspec {}", rel));
        }
        if dir.join("build.gradle").exists() || dir.join("build.gradle.kts").exists() {
            return Some("./gradlew test".to_string());
        }
        if dir.join("pom.xml").exists() {
            return Some("mvn test".to_string());
        }
        if dir.join("go.mod").exists() {
            let pkg_dir = p.parent().map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());
            return Some(format!("go test {}/...", pkg_dir));
        }
        if dir.join("Makefile").exists() || dir.join("makefile").exists() {
            return Some("make test".to_string());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// t-004: explain why a dirty file is a conflict risk
// ---------------------------------------------------------------------------

fn explain_conflict_risk(file: &str) -> Option<String> {
    // Check for unresolved merge conflict markers.
    if let Ok(content) = std::fs::read_to_string(file) {
        if content.contains("<<<<<<<") {
            return Some("file contains unresolved merge conflict markers".to_string());
        }
    }
    // Check git status for staged/unstaged changes.
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--", file])
        .output()
        .ok()?;
    let status_str = String::from_utf8_lossy(&status.stdout);
    let code = status_str.chars().take(2).collect::<String>();
    let reason = match code.trim() {
        "M" | "MM" => "has unstaged modifications",
        "A"        => "is newly staged",
        "D"        => "is staged for deletion",
        "R"        => "has been renamed (staged)",
        "UU"       => "has unmerged changes",
        s if s.contains('M') => "has staged and/or unstaged modifications",
        _ => "has uncommitted changes",
    };
    Some(reason.to_string())
}

// ---------------------------------------------------------------------------
// t-001: detect view/UI files for broad-query demotion
// ---------------------------------------------------------------------------

/// Generic tokens that appear in many UI files but don't anchor a specific domain concept.
/// Files whose stem is composed entirely of these tokens + "state"/"view"/"model" are
/// demoted to reference_only on broad queries unless the query names them directly.
const GENERIC_FILE_STEMS: &[&str] = &[
    "playhead", "state", "update", "position", "value", "cursor", "progress",
    "indicator", "status", "mode", "flag", "current", "local",
];

/// Returns true for files that are pure rendering surfaces regardless of query.
/// These are demoted unconditionally unless the query explicitly names them.
fn is_rendering_surface(file: &str) -> bool {
    let name = std::path::Path::new(file)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    name.ends_with("Canvas") || name.ends_with("Overlay") || name.ends_with("Surface")
        || name.ends_with("Roll") || name.ends_with("Sheet") || name.ends_with("Layer")
        || name.ends_with("Renderer") || name.ends_with("Drawable")
}

fn is_view_like_file(file: &str, layer: &str) -> bool {
    if layer == "view" { return true; }
    let name = std::path::Path::new(file)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    // Common view/UI naming conventions in iOS/macOS/web/Android.
    if name.ends_with("View") || name.ends_with("ViewController") || name.ends_with("Screen")
        || name.ends_with("Widget") || name.ends_with("Panel") || name.ends_with("Cell")
        || name.ends_with("Button") || name.ends_with("Label") || name.ends_with("Row")
        || name.ends_with("Canvas") || name.ends_with("Overlay") || name.ends_with("Surface")
        || name.ends_with("Roll") || name.ends_with("Sheet") || name.ends_with("Layer")
        || name.ends_with("Renderer") || name.ends_with("Drawable")
        || name.contains("ViewController") || name.contains("Renderer")
        || name.ends_with("Page") || name.ends_with("Fragment")
    {
        return true;
    }
    // Demote generic-stem files (PlayheadState.swift, UpdateView.swift, etc.)
    // that are in a UI-adjacent layer.
    if matches!(layer, "view" | "viewmodel" | "ui") {
        let name_lower = name.to_lowercase();
        let stem_words = split_camel_lower(&name_lower);
        if !stem_words.is_empty()
            && stem_words.iter().all(|w| GENERIC_FILE_STEMS.iter().any(|g| *g == w.as_str()))
        {
            return true;
        }
    }
    false
}

/// Returns true when the query tokens explicitly name the file's stem, preventing
/// demotion when the user is specifically asking about that file.
fn query_names_file(query_tokens: &[String], file: &str) -> bool {
    let name = std::path::Path::new(file)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    if name.is_empty() { return false; }
    let stem_words = split_camel_lower(&name);
    let matches = stem_words.iter().filter(|w| query_tokens.iter().any(|t| t == *w)).count();
    matches >= 2.min(stem_words.len())
}

/// Split a camelCase/PascalCase identifier into lowercase words.
fn split_camel_lower(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if ch.is_uppercase() && !current.is_empty() {
            words.push(current.to_lowercase());
            current = ch.to_string();
        } else if ch == '_' || ch == '-' || ch == '.' {
            if !current.is_empty() {
                words.push(current.to_lowercase());
                current = String::new();
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() { words.push(current.to_lowercase()); }
    words
}

// ---------------------------------------------------------------------------
// t-004: convert invariant / constraint text into a step + expected pair
// ---------------------------------------------------------------------------

/// Extract the key subject noun phrase from an invariant sentence.
/// Returns the first 3–6 words after stripping leading modal/constraint words.
fn extract_subject(text: &str) -> String {
    const SKIP: &[&str] = &[
        "the", "a", "an", "this", "that", "it",
        "must", "never", "cannot", "shall", "always", "only", "not", "no",
        "should", "will", "would", "is", "are", "be", "been",
        "require", "ensure", "prevent", "guarantee", "invariant",
    ];
    text.split_whitespace()
        .filter(|w| {
            let wl = w.to_lowercase();
            let base: String = wl.chars().filter(|c| c.is_alphabetic()).collect();
            !SKIP.contains(&base.as_str())
        })
        .take(5)
        .collect::<Vec<_>>()
        .join(" ")
}

fn invariant_to_scenario(text: &str) -> Value {
    let tl = text.to_lowercase();
    let subject = extract_subject(text);
    let subj = if subject.is_empty() { text.to_string() } else { subject };

    let (step, expected) = if tl.contains("must not") || tl.contains("never") || tl.contains("cannot") || tl.contains("forbidden") {
        (
            format!("Trigger conditions that would violate: {}", subj),
            format!("System rejects or prevents the violation — «{}» holds", text),
        )
    } else if tl.contains("must") || tl.contains("shall") || tl.contains("always") {
        (
            format!("Exercise the happy path for: {}", subj),
            format!("Observe that «{}» is satisfied after execution", text),
        )
    } else if tl.contains("ensure") || tl.contains("guarantee") {
        (
            format!("Run the scenario that exercises: {}", subj),
            format!("Post-condition confirmed: «{}»", text),
        )
    } else if tl.contains("require") {
        (
            format!("Attempt to call without satisfying the precondition for: {}", subj),
            format!("Guard fires or error raised before violating «{}»", text),
        )
    } else if tl.contains("only") || tl.contains("prevent") {
        (
            format!("Attempt a bypass around: {}", subj),
            format!("Prevention still enforced — «{}»", text),
        )
    } else {
        (
            format!("Verify the observable behaviour of: {}", subj),
            format!("Constraint «{}» holds after the change", text),
        )
    };
    json!({ "step": step, "expected": expected })
}
