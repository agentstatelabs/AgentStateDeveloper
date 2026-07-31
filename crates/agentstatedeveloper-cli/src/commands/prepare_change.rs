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
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, CandidateAggregates,
    EffectStore, Engine, FeedbackStore, FtsFilters, IndexStore, LedgerKind, LedgerStore,
    SearchFtsDb, Symbol, aggregate_candidate_data, apply_feedback_adjustments, classify_file_role,
    classify_layer_sym, compute_trust_score, compute_uncertainty, confidence_scores,
    derive_cold_hints, detect_ambiguous_tokens, detect_possible_misses, estimate_tokens,
    explain_match, extract_summary, fetch_all_test_file_paths, finalize_file_scores,
    find_candidates, gather_recency, git_dirty_files, glob_match, intent_focus, intent_layer_order,
    load_layer_overrides, parse_intent, parse_query, propagate_caller_invariants,
    propose_test_path, propose_test_stub, resolve_scope, result_bucket, stale_warning, symbol_tier,
    test_files_for_source, trim_for_agent,
};

use crate::commands::impact::git_recent_touches_pub;
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

    /// Comma-separated terms to exclude (alias: --avoid). Fights lexical
    /// over-matching, e.g. --avoid MIDI when "connect" pulls in MIDI code. Also
    /// supports inline minus-prefix syntax in the description, e.g. "drift
    /// playhead -sample -waveform".
    #[arg(long, visible_alias = "avoid")]
    pub exclude: Option<String>,

    /// Comma-separated glob patterns to restrict results to specific paths,
    /// e.g. --paths 'AcmeFlowCore/*Bus*'. This is the path-scope hint; use
    /// --scope for a named alias from .asd/scopes.toml.
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

    /// Compare predictions against the actual files changed in this commit.
    /// Emits edit_precision_metrics: precision, recall, F1, true/false positives/negatives.
    /// Example: --check-commit HEAD or --check-commit abc123f
    #[arg(long)]
    pub check_commit: Option<String>,

    /// AcmeFlow refinement (1.0.76): minimum confidence for a
    /// Hypothesis to surface in `prior_thinking`. Defaults to 0.3
    /// (matches `core::thinking::DEFAULT_CONFIDENCE_FLOOR`). Lower
    /// to see speculative hypotheses; raise to suppress noise.
    /// Hypotheses below the floor still appear in
    /// `thinking_summary.by_kind_dropped` for visibility.
    #[arg(long)]
    pub thinking_floor: Option<f64>,
}

pub fn run(cfg: &Config, args: PrepareChangeArgs) -> Result<()> {
    // AcmeFlow refinement (1.0.77): use the 24h soft threshold +
    // classify severity. Only print Critical to stderr unconditionally;
    // Soft warnings (just-past-threshold but FTS healthy) are demoted
    // into the JSON output's `stale_severity` field, where downstream
    // UIs can render them quietly — or skip rendering if the query
    // resolved successfully.
    if !args.quiet {
        if let Some(warn) = agentstatedeveloper_core::stale_warning_classified(
            &cfg.db_path,
            agentstatedeveloper_core::SOFT_STALE_THRESHOLD_SECS,
        ) {
            if warn.severity == agentstatedeveloper_core::StaleSeverity::Critical {
                eprintln!("{}", warn.message);
            }
            // Soft severity: suppressed from stderr; surfaces in the
            // response JSON's `stale` + `stale_severity` fields so the
            // agent can read it without it bullhorning every run.
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);
    let id_map = index_store.build_id_map(&engine);

    let (mut tokens, mut exclusions) = parse_query(&args.description);
    if let Some(ref excl) = args.exclude {
        for term in excl
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
        {
            exclusions.push(term);
        }
    }
    // Enrich query with active task context (CTX task description, etc.).
    // Auto-loads from CTXONE_PLAN / CTXONE_TASK env vars when --task-context is absent.
    let auto_ctx_plan = std::env::var("CTXONE_PLAN").ok().filter(|s| !s.is_empty());
    let auto_ctx_task = std::env::var("CTXONE_TASK").ok().filter(|s| !s.is_empty());
    let ctx_text = args.task_context.clone().or_else(|| {
        let parts: Vec<&str> = [auto_ctx_plan.as_deref(), auto_ctx_task.as_deref()]
            .iter()
            .filter_map(|x| *x)
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
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
        println!(
            "{}",
            json!({"description": args.description, "entry_points": {}})
        );
        return Ok(());
    }

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
    let _has_paths_filter = !paths_filter.is_empty();
    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
        tests_only: false,
        exclude_terms: exclusions,
        paths_filter,
        exclude_paths: Vec::new(),
        exclude_languages: Vec::new(),
    };

    let mut candidates = find_candidates(
        &engine,
        &args.description,
        &tokens,
        &filters,
        &ledger_store,
        &index_store,
        args.depth,
    );

    // Apply durable feedback adjustments (Useful/Noisy/WrongLayer verdicts).
    // list_all() is hoisted here and reused for wrong_layer_files below — avoids two separate
    // git-object reads for the same feedback data.
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    let all_fb = feedback_store
        .list_all(&engine.ref_name)
        .unwrap_or_default();
    // Derive flat_verdicts from the hoisted list (same logic as the
    // FeedbackStore default impl). Plan J t-016: tuple includes
    // created_at so the boost arithmetic can decay by age.
    let feedback_verdicts: Vec<(
        String,
        String,
        agentstatedeveloper_core::FeedbackVerdict,
        chrono::DateTime<chrono::Utc>,
    )> = all_fb
        .iter()
        .filter(|e| e.file_scope.is_none())
        .map(|e| {
            (
                e.symbol_id.clone(),
                e.query.clone(),
                e.verdict,
                e.created_at,
            )
        })
        .collect();
    let feedback_metrics = apply_feedback_adjustments(
        &engine,
        &index_store,
        &args.description,
        &mut candidates,
        &feedback_verdicts,
    );

    // Recency pass (one git call for all files).
    let recency = gather_recency(200, 14.0);

    // ---- Build entry points + aggregate data ----------------------------
    // Plan M t-003 (1.0.96): main candidate loop extracted to
    // aggregate_candidate_data(). The 6 parallel accumulators (by_layer,
    // design_invariants, known_hazards, validation_scenarios_ledger,
    // effects_summary, file_scores) plus top_sym_id and seen_inv now live
    // in CandidateAggregates. seen_inv is returned so the subsequent
    // caller-invariant propagation pass dedups against the main loop.
    let layer_order = intent_layer_order(intent);
    let CandidateAggregates {
        mut by_layer,
        mut design_invariants,
        known_hazards,
        validation_scenarios_ledger,
        effects_summary,
        mut file_scores,
        top_sym_id,
        mut seen_inv,
    } = aggregate_candidate_data(
        &engine,
        &index_store,
        &ledger_store,
        &effect_store,
        &candidates,
        &tokens,
        &recency,
        &layer_overrides,
    );

    // Plan J t-001: invariant propagation from callers.
    //
    // The original gap (field note: "invariants are silently dropped
    // when you query the callee"): an agent changing function B
    // should see invariants attached to functions A that call B —
    // because a contract A relies on could be broken by B's change.
    //
    // The candidate loop above only collects invariants from the
    // matched symbols themselves. Here we walk each candidate's
    // direct callers (depth=1) and add their invariants too,
    // deduplicating against the same `seen_inv` set so duplicates
    // don't accumulate. Each propagated entry is tagged
    // `from_caller: <caller_qname>` so the agent can tell which
    // upstream contract is at stake.
    //
    // Depth=1 is conservative — depth=2+ tends to surface invariants
    // too far removed from the planned change to be actionable.
    // `impact` (which does walk transitive callers) is the right
    // tool for full blast-radius reasoning; prepare_change stays
    // focused on edit-time signal.
    // Plan M t-003 (1.0.95): extracted to propagate_caller_invariants().
    let propagated = propagate_caller_invariants(
        &engine,
        &index_store,
        &ledger_store,
        &cfg.db_path,
        &candidates,
        &mut seen_inv,
    );
    design_invariants.extend(propagated);

    // Reorder by_layer keys according to layer_order.
    let mut ordered_by_layer: serde_json::Map<String, Value> = serde_json::Map::new();
    for lk in layer_order {
        if let Some(v) = by_layer.remove(*lk) {
            ordered_by_layer.insert(lk.to_string(), v);
        }
    }

    // Hoist git_dirty_files() — reused for conflict_risk below and stale_symbols further down.
    let dirty_files = git_dirty_files();
    // Plan M t-003 (1.0.97): sort + cliff cut + likely_edit_files build
    // extracted to finalize_file_scores(). Mutates file_scores in place
    // (sorts hot-first, applies cliff retain) so downstream stale_symbols
    // sees the same filtered set.
    let likely_edit_files = finalize_file_scores(&mut file_scores, &dirty_files, true);

    // ---- Affected tests via BFS from the top entry point ----------------
    // Plan M t-003 (1.0.93): extracted to gather_affected_tests().
    let affected_tests = gather_affected_tests(
        &engine,
        &index_store,
        &id_map,
        top_sym_id.as_deref(),
        args.test_depth,
        &design_invariants,
    );

    // ---- Blast-radius: caller/callee layer distribution + concrete call chains ----
    // Plan M t-003 (1.0.93): extracted to compute_blast_radius().
    let blast_radius = compute_blast_radius(
        &engine,
        &index_store,
        &id_map,
        &candidates,
        &layer_overrides,
    );

    // ---- Recent git touches for the top files (up to 3) ----------------
    let top_files: Vec<(String, usize)> = file_scores
        .iter()
        .take(3)
        .map(|(_, f, _, _, _, _, _)| (f.clone(), 0))
        .collect();
    let recently_touched = git_recent_touches_pub(&top_files, args.git_depth);

    // --- Staleness warnings -----------------------------------------------
    // Reuse dirty_files hoisted above — no second git status run.
    let stale_symbols: Vec<&str> = file_scores
        .iter()
        .filter(|(_, f, _, _, _, _, _)| dirty_files.contains(f.as_str()))
        .map(|(_, f, _, _, _, _, _)| f.as_str())
        .collect();

    // --- Test-gap detection -----------------------------------------------
    // Pre-fetch all indexed test file paths once — reused for proposed_test_path
    // and the recipe_edit per-file coverage lookup, avoiding N separate DB opens.
    let all_test_file_paths = fetch_all_test_file_paths(&cfg.db_path);
    // Plan M t-003 (1.0.94): extracted to detect_test_gap().
    let (test_gap, proposed_test_path, proposed_test_stub) =
        detect_test_gap(&file_scores, &affected_tests, &all_test_file_paths);
    // 1.0.86: suggested_test_coverage was emitting bare summary
    // strings duplicated from design_invariants. AcmeFlow probe
    // 2 confirmed the overlap. Now emits structured entries:
    //   { ref: "<entry_id>" }   for invariant-derived hints
    //   { hint: "<text>" }      for effect-derived / cold-start hints
    // Agent resolves refs against design_invariants. Saves the
    // summary-text duplication (~200-800 chars on rich-ledger
    // responses) while preserving the cold-start + effect-derived
    // semantics that AREN'T duplicates of anything.
    let suggested_test_coverage: Vec<Value> = if test_gap {
        let mut out: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for inv in &design_invariants {
            if let Some(eid) = inv.get("entry_id").and_then(Value::as_str) {
                if seen.insert(format!("ref:{eid}")) {
                    out.push(json!({ "ref": eid }));
                }
            }
        }
        for eff in &effects_summary {
            if let Some(cat) = eff.get("category").and_then(Value::as_str) {
                let hint = format!("verify {} after change", cat.to_lowercase());
                if seen.insert(format!("hint:{hint}")) {
                    out.push(json!({ "hint": hint }));
                }
            }
        }
        // Cold-start fallback: derive hints from the top candidate
        // symbol when no invariants exist. These are genuinely new
        // (not duplicates), emit as text hints.
        if design_invariants.is_empty() {
            if let Some((_, qname)) = candidates.first() {
                if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, qname) {
                    for h in
                        derive_cold_hints(&sym.qname, sym.signature.as_deref(), sym.doc.as_deref())
                    {
                        if seen.insert(format!("hint:{h}")) {
                            out.push(json!({ "hint": h }));
                        }
                    }
                }
            }
        }
        out
    } else {
        vec![]
    };

    const CONSTRAINT_WORDS: &[&str] = &[
        "must",
        "never",
        "shall",
        "always",
        "only",
        "cannot",
        "no ",
        "not ",
        "require",
        "ensure",
        "prevent",
        "guarantee",
        "invariant",
        "forbidden",
    ];
    // 1.0.86: scenario_tests was emitting bare summary strings
    // (a filtered subset of design_invariants[].summary). Now
    // emits { ref: "<entry_id>" } references — agent resolves
    // against design_invariants. The CONSTRAINT_WORDS filter is
    // preserved: only invariants phrased as constraints
    // (must/never/shall/...) become scenario_tests.
    let scenario_tests: Vec<Value> = design_invariants
        .iter()
        .filter_map(|inv| {
            let summary = inv.get("summary").and_then(Value::as_str)?;
            let entry_id = inv.get("entry_id").and_then(Value::as_str)?;
            let sl = summary.to_lowercase();
            if CONSTRAINT_WORDS.iter().any(|w| sl.contains(w)) {
                Some(json!({ "ref": entry_id }))
            } else {
                None
            }
        })
        .collect();

    let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
    // t-001: broad_query = at least half the query tokens are flagged as ambiguous.
    let broad_query = !ambiguous_terms.is_empty() && {
        let amb_set: HashSet<&str> = ambiguous_terms.iter().map(|s| s.as_str()).collect();
        let amb_count = tokens
            .iter()
            .filter(|t| amb_set.contains(t.as_str()))
            .count();
        amb_count * 2 >= tokens.len().max(1)
    };

    let layers_present: std::collections::HashSet<&str> = file_scores
        .iter()
        .map(|(_, _, layer, _, _, _, _)| layer.as_str())
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
            tests_only: false,
            kind: filters.kind.clone(),
            language: filters.language.clone(),
            include_tests: filters.include_tests,
            exclude_terms: filters.exclude_terms.clone(),
            paths_filter: vec![], // no path filter
            exclude_paths: filters.exclude_paths.clone(),
            exclude_languages: filters.exclude_languages.clone(),
        };
        let unscoped_hits = SearchFtsDb::open(&cfg.db_path)
            .ok()
            .filter(|fts| fts.has_data())
            .and_then(|fts| fts.search(&args.description, &unscoped_filters, 20).ok())
            .unwrap_or_default();
        let scoped_file_set: std::collections::HashSet<&str> = file_scores
            .iter()
            .map(|(_, f, _, _, _, _, _)| f.as_str())
            .collect();
        unscoped_hits
            .iter()
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
        let top_sids_omit: Vec<String> = candidates
            .iter()
            .take(3)
            .filter_map(|(_, q)| {
                index_store
                    .get_symbol_by_qname(&engine.ref_name, q)
                    .ok()
                    .flatten()
            })
            .map(|s| s.symbol_id)
            .collect();
        let mut omitted: Vec<Value> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        for sid in &top_sids_omit {
            let anchor_qname = id_map.get(sid).map(|s| s.qname.clone()).unwrap_or_default();
            // Check callers outside scope.
            for caller_id in index_store
                .get_callers(&engine.ref_name, sid)
                .unwrap_or_default()
            {
                if let Some(sym) = id_map.get(&caller_id) {
                    let in_scope = filters
                        .paths_filter
                        .iter()
                        .any(|p| glob_match(p, &sym.file));
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
            for callee_id in index_store
                .get_callees(&engine.ref_name, sid)
                .unwrap_or_default()
            {
                if let Some(sym) = id_map.get(&callee_id) {
                    let in_scope = filters
                        .paths_filter
                        .iter()
                        .any(|p| glob_match(p, &sym.file));
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
    let recipe_inspect: Vec<Value> = file_scores
        .iter()
        .map(|(score, file, layer, days, hot, top_symbol, why)| {
            json!({
                "file": file, "layer": layer, "score": score,
                "last_touched_days": days, "hot": hot,
                "top_symbol": top_symbol, "why": why,
            })
        })
        .collect();
    // 1.0.86: recipe_preserve emitted `constraint` (== invariant
    // summary) + source + kind. The constraint text was identical
    // to design_invariants[].summary. Now emits refs for
    // invariants; hazards stay inline because they're sourced from
    // known_hazards (a separate canonical list — no dedupe target
    // there yet).
    let recipe_preserve: Vec<Value> = design_invariants
        .iter()
        .filter_map(|inv| {
            inv.get("entry_id")
                .and_then(Value::as_str)
                .map(|eid| json!({ "ref": eid, "kind": "invariant" }))
        })
        .chain(known_hazards.iter().map(|h| {
            json!({
                "constraint": h["summary"],
                "source": h["source"],
                "kind": "hazard",
            })
        }))
        .collect();
    // t-005: Find files where a matching symbol has a WrongLayer verdict for
    // the current query family. Those impl files are demoted to recipe_reference.
    // Uses all_fb hoisted above — no second list_all() call needed.
    use std::collections::HashSet as _HSet;
    let wrong_layer_files: _HSet<String> = {
        let desc_norm = args.description.to_lowercase();
        let desc_tokens: std::collections::HashSet<String> = desc_norm
            .split(|c: char| !c.is_alphabetic())
            .filter(|t: &&str| t.len() > 2)
            .map(|t| t.to_string())
            .collect();
        let mut wl_files = _HSet::new();
        for entry in &all_fb {
            if !matches!(
                entry.verdict,
                agentstatedeveloper_core::FeedbackVerdict::WrongLayer
            ) {
                continue;
            }
            // Query-family match: share at least one token.
            let fb_tokens: std::collections::HashSet<String> = entry
                .query
                .split(|c: char| !c.is_alphabetic())
                .filter(|t: &&str| t.len() > 2)
                .map(|t: &str| t.to_string())
                .collect();
            let overlaps = desc_tokens.iter().any(|t| fb_tokens.contains(t));
            if !overlaps {
                continue;
            }
            // Look up the symbol's file via qname.
            if let Ok(Some(sym)) =
                index_store.get_symbol_by_qname(&engine.ref_name, &entry.symbol_qname)
            {
                wl_files.insert(sym.file);
            }
        }
        wl_files
    };
    // t-005: Build a map of file → test files that cover it (from affected_tests).
    let mut file_to_tests: HashMap<String, Vec<String>> = HashMap::new();
    for test in &affected_tests {
        if let Some(test_file) = test["file"].as_str() {
            for (_, file, _, _, _, _, _) in &file_scores {
                let entry = file_to_tests.entry(file.clone()).or_default();
                let tf = test_file.to_string();
                if !entry.contains(&tf) {
                    entry.push(tf);
                }
            }
        }
    }

    // edit: only impl files not flagged as wrong-layer or view-only-on-broad-query.
    // Rendering surfaces (Canvas, Overlay, Layer etc.) are demoted unconditionally
    // unless the query explicitly names them. Other view-like files are demoted only
    // on broad queries. A file is retained when ≥2 of its stem words appear in the
    // query tokens (generalised domain anchor — no hardcoded token list needed).
    let recipe_edit: Vec<Value> = likely_edit_files
        .iter()
        .filter(|f| {
            let file = f["file"].as_str().unwrap_or("");
            let layer = f["layer"].as_str().unwrap_or("");
            let names_file = query_names_file(&tokens, file);
            // Domain anchor: retain when query shares ≥2 stem words with the file.
            let stem_words = split_camel_lower(
                std::path::Path::new(file)
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or(""),
            );
            let domain_overlap = stem_words
                .iter()
                .filter(|w| tokens.iter().any(|t| t == *w))
                .count();
            let has_domain_anchor = domain_overlap >= 2;
            // Rendering surfaces: unconditional demotion unless query names them.
            let surface_demote = is_rendering_surface(file) && !names_file;
            // View-like: demote on broad queries unless domain anchor present.
            let broad_demote =
                broad_query && is_view_like_file(file, layer) && !names_file && !has_domain_anchor;
            // Anchor-missing: view-like file with zero domain overlap is always
            // reference-only regardless of broad_query. A drift/playhead query should
            // never put PianoRollView or SheetMusicView (notation views, zero overlap)
            // into edit — they are unrelated surfaces even when the query is specific.
            let anchor_missing_demote =
                is_view_like_file(file, layer) && !names_file && domain_overlap == 0;
            let demote = !wrong_layer_files.contains(file)
                && (surface_demote || broad_demote || anchor_missing_demote);
            f["file_role"].as_str() == Some("impl") && !wrong_layer_files.contains(file) && !demote
        })
        .map(|f| {
            // t-005: attach covering tests and run command to each edit entry.
            let file = f["file"].as_str().unwrap_or("");
            let mut indexed = test_files_for_source(&all_test_file_paths, file);
            if let Some(extra) = file_to_tests.get(file) {
                for t in extra {
                    if !indexed.contains(t) {
                        indexed.push(t.clone());
                    }
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
    let recipe_reference: Vec<Value> = likely_edit_files
        .iter()
        .filter(|f| {
            let file = f["file"].as_str().unwrap_or("");
            let layer = f["layer"].as_str().unwrap_or("");
            let names_file = query_names_file(&tokens, file);
            let stem_words = split_camel_lower(
                std::path::Path::new(file)
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or(""),
            );
            let domain_overlap = stem_words
                .iter()
                .filter(|w| tokens.iter().any(|t| t == *w))
                .count();
            let has_domain_anchor = domain_overlap >= 2;
            let surface_demote = is_rendering_surface(file) && !names_file;
            let broad_demote =
                broad_query && is_view_like_file(file, layer) && !names_file && !has_domain_anchor;
            let anchor_missing_demote =
                is_view_like_file(file, layer) && !names_file && domain_overlap == 0;
            let view_demote = surface_demote || broad_demote || anchor_missing_demote;
            matches!(f["file_role"].as_str(), Some("example") | Some("reference"))
                || (f["file_role"].as_str() == Some("impl") && wrong_layer_files.contains(file))
                || view_demote
        })
        .cloned()
        .collect();
    // Rebuild likely_edit_files to only include files that made it into recipe_edit.
    // Files demoted to reference_only must not appear in likely_edit_files — the raw
    // file_scores list is built before the edit/reference split, so without this step
    // surface-demoted files (WaveformCanvas etc.) would linger in the raw list.
    //
    // Keep the full pre-split list for classification_debug + rationale computation
    // (we need rationale for reference_only files too).
    let all_candidate_files: Vec<Value> = likely_edit_files.clone();
    let edit_file_set: HashSet<&str> = recipe_edit
        .iter()
        .filter_map(|e| e["file"].as_str())
        .collect();
    let likely_edit_files: Vec<Value> = likely_edit_files
        .into_iter()
        .filter(|e| {
            e["file"]
                .as_str()
                .map_or(false, |f| edit_file_set.contains(f))
        })
        .collect();

    // 1.0.88: cliff cut at the FINAL rebuilt list. The earlier
    // 1.0.87 cliff (on file_scores pre-recipe-split) missed cases
    // where intermediate scores from soon-to-be-demoted files
    // smoothed the gradient. After recipe_edit demotes
    // reference-only files, the remaining edit-list often shows the
    // cliff cleanly. AcmeFlow case: file_scores might have
    // 42/31/29/27/25/19/18 (no cliff at file-scores time), but
    // post-demotion only the impl-layer files remain — 42/31/19/18
    // with the 19/31=0.61 cliff visible.
    let mut likely_edit_files = likely_edit_files;
    let scores: Vec<f64> = likely_edit_files
        .iter()
        .map(|e| e["score"].as_f64().unwrap_or(0.0))
        .collect();
    let mut sorted_desc = scores.clone();
    sorted_desc.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let cliff_cut2 = agentstatedeveloper_core::cliff_cutoff_index(sorted_desc.iter().copied());
    if cliff_cut2 < likely_edit_files.len() {
        let cutoff = sorted_desc[cliff_cut2 - 1];
        likely_edit_files.retain(|e| e["score"].as_f64().unwrap_or(0.0) >= cutoff);
    }

    // t-002: include exact build/test commands for each affected test file.
    let mut recipe_run: Vec<Value> = affected_tests
        .iter()
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
        if let Some((_, top_file, _, _, _, _, _)) = file_scores.first() {
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
    let mut recipe_manually_validate: Vec<Value> = validation_scenarios_ledger
        .iter()
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
    // Plan J t-002: surface test-gap warning directly inside the
    // safe_change_recipe so an agent reading only the recipe gets
    // the signal — without having to cross-reference the top-level
    // `test_gap` field. Suggests the proposed_test_path target when
    // we have one.
    if test_gap {
        let top_impl_source = file_scores
            .first()
            .map(|(_, f, _, _, _, _, _)| f.as_str())
            .unwrap_or("");
        let suggestion = proposed_test_path
            .as_deref()
            .unwrap_or("(no proposed path; see proposed_test_path)");
        recipe_manually_validate.push(json!({
            "source": top_impl_source,
            "kind": "missing_test",
            "step": format!(
                "No test currently exercises this change set. Add a test \
                 covering the planned edit; suggested target: {suggestion}"
            ),
            "expected": "A new (or extended) test that exercises the changed code path and fails before the edit, passes after.",
            "raw": "test_gap: affected_tests is empty",
        }));
    }
    // Always compute classification_debug — used for classification_summary rollup
    // even when --debug-classification is not set.  The full array is only emitted
    // in the JSON when --debug-classification is explicitly requested.
    // Uses all_candidate_files (edit + reference) so the rationale map covers every file.
    let classification_debug: Vec<Value> = all_candidate_files
        .iter()
        .map(|f| {
            let file = f["file"].as_str().unwrap_or("");
            let layer = f["layer"].as_str().unwrap_or("");
            let file_role = f["file_role"].as_str().unwrap_or("unknown");
            let names_file = query_names_file(&tokens, file);
            let stem_words = split_camel_lower(
                std::path::Path::new(file)
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or(""),
            );
            let matched_stem_words: Vec<&str> = stem_words
                .iter()
                .filter(|w| tokens.iter().any(|t| t == *w))
                .map(|w| w.as_str())
                .collect();
            let domain_overlap = matched_stem_words.len();
            let has_domain_anchor = domain_overlap >= 2;
            let surface_demoted = is_rendering_surface(file) && !names_file;
            let broad_demoted =
                broad_query && is_view_like_file(file, layer) && !names_file && !has_domain_anchor;
            let anchor_missing_demoted =
                is_view_like_file(file, layer) && !names_file && domain_overlap == 0;
            let is_wrong_layer = wrong_layer_files.contains(file);
            let rule_that_won = if file_role == "test" {
                "test"
            } else if is_wrong_layer {
                "wrong-layer → reference"
            } else if surface_demoted {
                "surface → reference"
            } else if broad_demoted {
                "broad-query view → reference"
            } else if anchor_missing_demoted {
                "anchor-missing view → reference"
            } else if file_role == "impl" {
                "edit"
            } else {
                file_role
            };
            let rationale = make_file_rationale(
                rule_that_won,
                surface_demoted,
                has_domain_anchor,
                &matched_stem_words,
                names_file,
                broad_query,
            );
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
                "rationale": rationale,
            })
        })
        .collect();

    // Build file → rationale map from classification_debug.
    let file_rationale: HashMap<String, String> = classification_debug
        .iter()
        .filter_map(|e| {
            let file = e["file"].as_str()?.to_string();
            let rat = e["rationale"].as_str()?.to_string();
            Some((file, rat))
        })
        .collect();

    // Classification summary: rule_that_won counts aggregated across all files.
    // Always emitted — lets dashboards answer "is ASD classifying on real domain
    // anchors or weaker heuristics?" without needing --debug-classification.
    let mut rule_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for entry in &classification_debug {
        let rule = entry
            .get("rule_that_won")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        *rule_counts.entry(rule.to_string()).or_default() += 1;
    }

    // Edit confidence: lower when workspace has no annotation data to boost signals.
    let trust_dq = compute_trust_score(&cfg.db_path);
    let (edit_confidence, edit_confidence_note) = match trust_dq.data_quality.state.as_str() {
        "clean_room" => (
            "reduced",
            "fresh workspace — classification relies on structural signals only; run `asd annotate-commit` after each commit to improve accuracy",
        ),
        "unannotated" => (
            "reduced",
            "no annotation data — classification uses structural signals only; ledger annotations boost accuracy",
        ),
        "degraded" => (
            "low",
            "possible state loss — ledger signals may be incomplete; verify with `asd trust`",
        ),
        _ => ("normal", ""),
    };

    let mut classification_summary = serde_json::Map::new();
    for (k, v) in &rule_counts {
        classification_summary.insert(k.clone(), json!(*v));
    }
    classification_summary.insert("edit_confidence".into(), json!(edit_confidence));
    if !edit_confidence_note.is_empty() {
        classification_summary.insert("edit_confidence_note".into(), json!(edit_confidence_note));
    }
    let classification_summary = Value::Object(classification_summary);

    // Enrich likely_edit_files with per-file rationale.
    let likely_edit_files: Vec<Value> = likely_edit_files
        .into_iter()
        .map(|mut f| {
            if let Some(obj) = f.as_object_mut() {
                let file = obj
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(rat) = file_rationale.get(&file) {
                    obj.insert("rationale".into(), json!(rat));
                }
            }
            f
        })
        .collect();

    // Enrich recipe_edit and recipe_reference with per-file rationale.
    let recipe_edit: Vec<Value> = recipe_edit
        .into_iter()
        .map(|mut f| {
            if let Some(obj) = f.as_object_mut() {
                let file = obj
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(rat) = file_rationale.get(&file) {
                    obj.entry("rationale".to_string())
                        .or_insert_with(|| json!(rat));
                }
            }
            f
        })
        .collect();
    let recipe_reference: Vec<Value> = recipe_reference
        .into_iter()
        .map(|mut f| {
            if let Some(obj) = f.as_object_mut() {
                let file = obj
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(rat) = file_rationale.get(&file) {
                    obj.entry("rationale".to_string())
                        .or_insert_with(|| json!(rat));
                }
            }
            f
        })
        .collect();

    // AcmeFlow refinement #1 (1.0.84): recursively drop empty
    // sub-fields (preserve:[], reference_only:[],
    // likely_omitted_files:[], nested empty layer_distribution maps,
    // etc). This is a NESTED clean — top-level drop_empty in the
    // outer json! block can't reach into safe_change_recipe's
    // children. On a typical query without ledger annotations,
    // strips ~200 chars of empty-array clutter from the recipe.
    let safe_change_recipe = agentstatedeveloper_core::drop_empty_recursive(json!({
        "inspect": recipe_inspect,
        "preserve": recipe_preserve,
        "edit": recipe_edit,
        "reference_only": recipe_reference,
        "run": recipe_run,
        "manually_validate": recipe_manually_validate,
        "blast_radius": blast_radius,
        "likely_omitted_files": likely_omitted_files,
    }));

    // Scoped suggestions for prepare-change: use edit files as top_qnames proxy.
    let edit_file_names: Vec<String> = likely_edit_files
        .iter()
        .filter_map(|v| v.get("file").and_then(Value::as_str).map(|s| s.to_string()))
        .collect();
    let scoped_suggestions_pc: Vec<String> = if !ambiguous_terms.is_empty() {
        agentstatedeveloper_core::suggest_scoped_queries(
            &tokens,
            &ambiguous_terms,
            &edit_file_names,
        )
    } else {
        vec![]
    };
    // Re-use the db_state already computed for edit_confidence (trust score above).
    let uncertainty = compute_uncertainty(
        &tokens,
        &ambiguous_terms,
        &possible_misses,
        file_scores.len(),
        &scoped_suggestions_pc,
        engine.fts.as_ref(),
        Some(trust_dq.data_quality.state.as_str()),
    );

    let focus = intent_focus(intent);
    let ctx_context_val = match (auto_ctx_plan.as_deref(), auto_ctx_task.as_deref()) {
        (None, None) => Value::Null,
        _ => json!({
            "plan": auto_ctx_plan,
            "task": auto_ctx_task,
            "injected": ctx_text.is_some(),
        }),
    };
    // AcmeFlow refinement (1.0.76): surface captured thinking on
    // the symbols that matter for this query. Mirrors the MCP handler
    // (mcp_server.rs:4189-4201). Pull top_symbol off each
    // likely_edit_files entry; gather_prior_thinking walks the ledger
    // and projects to the compact `prior_thinking` shape, plus a
    // metadata summary that always emits even when entries is Null.
    let thinking_qnames: Vec<String> = likely_edit_files
        .iter()
        .filter_map(|f| {
            f.get("top_symbol")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect();
    let thinking_floor = args
        .thinking_floor
        .unwrap_or(agentstatedeveloper_core::thinking::DEFAULT_CONFIDENCE_FLOOR);
    let pt = agentstatedeveloper_core::thinking::gather_prior_thinking(
        &engine,
        &thinking_qnames,
        thinking_floor,
    );
    let prior_thinking = pt.entries;
    let thinking_summary = serde_json::to_value(&pt.summary).unwrap_or(Value::Null);

    // AcmeFlow refinement: compute once (cheap) for both fields.
    let stale_classified = agentstatedeveloper_core::stale_warning_classified(
        &cfg.db_path,
        agentstatedeveloper_core::SOFT_STALE_THRESHOLD_SECS,
    );
    let stale_msg = stale_classified
        .as_ref()
        .map(|w| Value::String(w.message.clone()))
        .unwrap_or(Value::Null);
    let stale_severity = stale_classified
        .as_ref()
        .map(|w| serde_json::to_value(w.severity).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);

    // Token economy (1.0.78): build feedback_summary as a Map so
    // all-zero counts skip serialization. On a call with no
    // feedback activity (the common case), the entire block
    // collapses to `{}` and the json! macro below would still emit
    // it as `{}` — so we conditionally insert only when non-empty.
    let feedback_summary = {
        let mut m = serde_json::Map::new();
        if feedback_metrics.entries_applied > 0 {
            m.insert(
                "entries_applied".into(),
                json!(feedback_metrics.entries_applied),
            );
        }
        if feedback_metrics.suppressed > 0 {
            m.insert("suppressed".into(), json!(feedback_metrics.suppressed));
        }
        if feedback_metrics.preserved_useful_siblings > 0 {
            m.insert(
                "preserved_useful_siblings".into(),
                json!(feedback_metrics.preserved_useful_siblings),
            );
        }
        if feedback_metrics.boosted > 0 {
            m.insert("boosted".into(), json!(feedback_metrics.boosted));
        }
        if feedback_metrics.recurring_fp_suppressed > 0 {
            m.insert(
                "recurring_fp_suppressed".into(),
                json!(feedback_metrics.recurring_fp_suppressed),
            );
        }
        if !feedback_metrics.rules_applied.is_empty() {
            m.insert(
                "rules_applied".into(),
                json!(feedback_metrics.rules_applied),
            );
        }
        Value::Object(m)
    };

    // t-004: stage-dump on low-confidence / empty results. The agent normally
    // sees only `likely_edit_files` (the final, stage-3 list). When confidence
    // is low or that list is empty, surface what the earlier filtering stages
    // ranked and cut — so a missing-but-expected file is diagnosable instead of
    // silently filtered. Reuses already-computed collections; null (and dropped
    // by drop_empty_top_level) when not triggered. See CLAUDE.md "Multi-stage
    // filtering: cut at the stage the agent sees".
    let round3 = |x: f64| (x * 1000.0).round() / 1000.0;
    let stage_dump = {
        let low_conf = matches!(uncertainty.level.as_str(), "high" | "critical");
        if low_conf || likely_edit_files.is_empty() {
            let stage1: Vec<Value> = candidates
                .iter()
                .map(|(score, qname)| json!({ "score": round3(*score), "qname": qname }))
                .collect();
            let stage2: Vec<Value> = file_scores
                .iter()
                .map(|(score, file, layer, _days, _hot, sym, why)| {
                    json!({
                        "file": file,
                        "score": round3(*score),
                        "layer": layer,
                        "top_symbol": sym,
                        "why": why,
                    })
                })
                .collect();
            let demoted = safe_change_recipe
                .get("reference_only")
                .cloned()
                .unwrap_or_else(|| json!([]));
            json!({
                "triggered_by": if likely_edit_files.is_empty() {
                    "empty_edit_files"
                } else {
                    "low_confidence"
                },
                "uncertainty_level": uncertainty.level,
                "note": "Agents normally see only likely_edit_files (the final stage-3 list). \
                         This result is low-confidence or empty, so here is what the earlier \
                         filtering stages ranked and cut. A file missing from likely_edit_files \
                         but present below may have been demoted, not absent.",
                "stage1_symbol_candidates": { "count": candidates.len(), "ranked": stage1 },
                "stage2_surviving_files": stage2,
                "stage3_demoted_to_reference_only": demoted,
            })
        } else {
            Value::Null
        }
    };

    let out = json!({
        "description": args.description,
        "task_context": args.task_context,
        "ctx_context": ctx_context_val,
        "stale": stale_msg,
        "stale_severity": stale_severity,
        "intent": if intent.is_empty() { Value::Null } else { json!(intent) },
        "focus": if focus.is_empty() { Value::Null } else { json!(focus) },
        "uncertainty": uncertainty.to_json(),
        "ambiguous_terms": ambiguous_terms,
        "possible_misses": possible_misses,
        "scope_narrowed": scope_narrowed,
        "feedback_summary": feedback_summary,
        "safe_change_recipe": safe_change_recipe,
        "design_invariants": design_invariants,
        "known_hazards": known_hazards,
        "prior_thinking": prior_thinking,
        "thinking_summary": thinking_summary,
        "validation_scenarios": validation_scenarios_ledger,
        "entry_points": { "by_layer": ordered_by_layer },
        "likely_edit_files": likely_edit_files,
        "stage_dump": stage_dump,
        "affected_tests": affected_tests,
        "test_gap": test_gap,
        "proposed_test_path": proposed_test_path,
        "proposed_test_stub": proposed_test_stub,
        "suggested_test_coverage": suggested_test_coverage,
        "scenario_tests": scenario_tests,
        "stale_symbols": stale_symbols,
        "effects_summary": effects_summary,
        "recently_touched": recently_touched,
        "classification_summary": classification_summary,
        "classification_debug": if args.debug_classification { json!(classification_debug) } else { Value::Null },
        "edit_precision_metrics": if let Some(ref sha) = args.check_commit {
            compute_edit_precision(&likely_edit_files, sha)
        } else {
            Value::Null
        },
    });
    // Token economy:
    //   1.0.78: agent mode emits compact JSON (no pretty-print
    //           whitespace). Token estimate matches the compact form.
    //   1.0.79: drop top-level empty/null fields + input echoes +
    //           redundant stale string in agent mode.
    //   1.0.81: AcmeFlow field-eval (2026-06-04) caught that
    //           drop_empty_top_level was --agent-only. Agents
    //           consuming the default JSON path were still getting
    //           feedback_summary:{}, intent:null, etc. Fix: apply
    //           drop_empty_top_level UNCONDITIONALLY — it strips
    //           null/[]/{} which is signal-free for both humans
    //           and agents. Input-echo fields are still
    //           agent-mode-only since human terminal users like
    //           seeing the description they typed.
    let out = agentstatedeveloper_core::drop_empty_top_level(out);

    let (out, compact_for_agent) = if args.agent {
        let max_list = (args.agent_budget / 500).max(3).min(20);
        let mut trimmed = trim_for_agent(&out, max_list);

        // 1.0.79: dedupe stale string vs stale_severity — keep only
        // the structured severity (carries enough for the agent to
        // render its own message; the string was just humanization).
        if let Some(obj) = trimmed.as_object_mut() {
            obj.remove("stale");
            // Also drop input echoes — the agent already has them.
            obj.remove("description");
            obj.remove("task_context");
            obj.remove("ctx_context");
        }

        // Re-strip after agent-mode-specific removals (an empty
        // map that survived the unconditional pass above might
        // have been re-emptied here).
        let trimmed = agentstatedeveloper_core::drop_empty_top_level(trimmed);

        let compact = serde_json::to_string(&trimmed)?;
        let token_est = estimate_tokens(&compact);
        let mut v = trimmed;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("token_estimate".into(), json!(token_est));
        }
        (v, true)
    } else {
        (out, false)
    };
    let json_str = if compact_for_agent {
        serde_json::to_string(&out)?
    } else {
        serde_json::to_string_pretty(&out)?
    };
    println!("{}", json_str);
    Ok(())
}

// ---------------------------------------------------------------------------
// t-001: expanded file role classification
// ---------------------------------------------------------------------------

// Plan J t-003: classify_file_role lifted to
// `agentstatedeveloper_core::classify_file_role` so CLI and MCP
// prepare_change paths share one canonical impl. The new shared
// version also recognizes `view` and `viewmodel` roles.

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
            let pkg_name = std::fs::read_to_string(dir.join("Cargo.toml"))
                .ok()
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
        if dir.join("pyproject.toml").exists()
            || dir.join("setup.py").exists()
            || dir.join("pytest.ini").exists()
            || dir.join("setup.cfg").exists()
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
            let pkg_dir = p
                .parent()
                .map(|d| d.to_string_lossy().to_string())
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

// Plan M t-004 (1.0.98): explain_conflict_risk lifted to
// agentstatedeveloper_core::prepare_change so MCP can use it too.

// ---------------------------------------------------------------------------
// t-001: detect view/UI files for broad-query demotion
// ---------------------------------------------------------------------------

/// Generic tokens that appear in many UI files but don't anchor a specific domain concept.
/// Files whose stem is composed entirely of these tokens + "state"/"view"/"model" are
/// demoted to reference_only on broad queries unless the query names them directly.
const GENERIC_FILE_STEMS: &[&str] = &[
    "playhead",
    "state",
    "update",
    "position",
    "value",
    "cursor",
    "progress",
    "indicator",
    "status",
    "mode",
    "flag",
    "current",
    "local",
];

/// Returns true for files that are pure rendering surfaces regardless of query.
/// These are demoted unconditionally unless the query explicitly names them.
fn is_rendering_surface(file: &str) -> bool {
    let name = std::path::Path::new(file)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    name.ends_with("Canvas")
        || name.ends_with("Overlay")
        || name.ends_with("Surface")
        || name.ends_with("Roll")
        || name.ends_with("Sheet")
        || name.ends_with("Layer")
        || name.ends_with("Renderer")
        || name.ends_with("Drawable")
}

fn is_view_like_file(file: &str, layer: &str) -> bool {
    if layer == "view" {
        return true;
    }
    let name = std::path::Path::new(file)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    // Common view/UI naming conventions in iOS/macOS/web/Android.
    if name.ends_with("View")
        || name.ends_with("ViewController")
        || name.ends_with("Screen")
        || name.ends_with("Widget")
        || name.ends_with("Panel")
        || name.ends_with("Cell")
        || name.ends_with("Button")
        || name.ends_with("Label")
        || name.ends_with("Row")
        || name.ends_with("Canvas")
        || name.ends_with("Overlay")
        || name.ends_with("Surface")
        || name.ends_with("Roll")
        || name.ends_with("Sheet")
        || name.ends_with("Layer")
        || name.ends_with("Renderer")
        || name.ends_with("Drawable")
        || name.contains("ViewController")
        || name.contains("Renderer")
        || name.ends_with("Page")
        || name.ends_with("Fragment")
    {
        return true;
    }
    // Demote generic-stem files (PlayheadState.swift, UpdateView.swift, etc.)
    // that are in a UI-adjacent layer.
    if matches!(layer, "view" | "viewmodel" | "ui") {
        let name_lower = name.to_lowercase();
        let stem_words = split_camel_lower(&name_lower);
        if !stem_words.is_empty()
            && stem_words
                .iter()
                .all(|w| GENERIC_FILE_STEMS.iter().any(|g| *g == w.as_str()))
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
    if name.is_empty() {
        return false;
    }
    let stem_words = split_camel_lower(&name);
    let matches = stem_words
        .iter()
        .filter(|w| query_tokens.iter().any(|t| t == *w))
        .count();
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
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    words
}

// ---------------------------------------------------------------------------
// t-004: convert invariant / constraint text into a step + expected pair
// ---------------------------------------------------------------------------

/// Extract the key subject noun phrase from an invariant sentence.
/// Returns the first 3–6 words after stripping leading modal/constraint words.
fn extract_subject(text: &str) -> String {
    const SKIP: &[&str] = &[
        "the",
        "a",
        "an",
        "this",
        "that",
        "it",
        "must",
        "never",
        "cannot",
        "shall",
        "always",
        "only",
        "not",
        "no",
        "should",
        "will",
        "would",
        "is",
        "are",
        "be",
        "been",
        "require",
        "ensure",
        "prevent",
        "guarantee",
        "invariant",
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
    let subj = if subject.is_empty() {
        text.to_string()
    } else {
        subject
    };

    let (step, expected) = if tl.contains("must not")
        || tl.contains("never")
        || tl.contains("cannot")
        || tl.contains("forbidden")
    {
        (
            format!("Trigger conditions that would violate: {}", subj),
            format!(
                "System rejects or prevents the violation — «{}» holds",
                text
            ),
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
            format!(
                "Attempt to call without satisfying the precondition for: {}",
                subj
            ),
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

// ---------------------------------------------------------------------------
// Change Model: per-file classification rationale
// ---------------------------------------------------------------------------

/// Convert classification signals into a plain-English rationale string.
///
/// Used to enrich `likely_edit_files` and `safe_change_recipe.reference_only`
/// so agents understand *why* each file was classified as edit or reference.
fn make_file_rationale(
    rule: &str,
    surface_demoted: bool,
    domain_anchor: bool,
    matched_stems: &[&str],
    names_file: bool,
    broad_query: bool,
) -> String {
    match rule {
        "edit" if names_file => "edit: query names this file directly".to_string(),
        "edit" if domain_anchor => {
            format!(
                "edit: domain-anchored — {} query token(s) match file name ({})",
                matched_stems.len(),
                matched_stems.join(", ")
            )
        }
        "edit" if !matched_stems.is_empty() => {
            format!(
                "edit: impl file matching query token '{}'",
                matched_stems[0]
            )
        }
        "edit" => "edit: impl file in query scope".to_string(),

        r if r.contains("surface") => {
            "reference: rendering surface — read-only layer (WaveformCanvas, SheetMusicView, etc.)"
                .to_string()
        }
        r if r.contains("broad-query view") => {
            if domain_anchor {
                format!(
                    "reference: view layer but domain-anchored ({})",
                    matched_stems.join(", ")
                )
            } else {
                format!(
                    "reference: view layer on broad query — needs ≥2 matching tokens to enter edit (matched: {})",
                    matched_stems.len()
                )
            }
        }
        r if r.contains("anchor-missing") => {
            "reference: view/surface layer with no domain overlap — unrelated rendering file"
                .to_string()
        }
        r if r.contains("wrong-layer") => {
            "reference: wrong layer for this change type — structural mismatch".to_string()
        }

        "example" | "reference" => "reference: documentation or example file".to_string(),
        "test" => "test: run to validate this change".to_string(),

        _ => {
            // Fallback: surface what we know from remaining signals
            if broad_query {
                format!("reference: broad query heuristic — {}", rule)
            } else {
                format!(
                    "classified as: {} (surface_demoted={}, domain_anchor={})",
                    rule, surface_demoted, domain_anchor
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Change Model: edit precision metrics
// ---------------------------------------------------------------------------

/// Compare `likely_edit_files` (JSON objects with a "file" key) against the
/// actual files changed in `sha` (via git diff-tree). Returns a JSON object with
/// precision, recall, F1, and TP/FP/FN file lists, or null if git fails.
fn compute_edit_precision(likely_edit_files: &[Value], sha: &str) -> Value {
    let predicted_files: Vec<String> = likely_edit_files
        .iter()
        .filter_map(|v| v.get("file").and_then(Value::as_str).map(|s| s.to_string()))
        .collect();
    let predicted_files = &predicted_files;
    let output = std::process::Command::new("git")
        .args(["diff-tree", "--no-commit-id", "-r", "--name-only", sha])
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            return json!({ "error": format!("git diff-tree failed: {}", err.trim()) });
        }
        Err(e) => return json!({ "error": format!("git not available: {}", e) }),
    };
    let actual_files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if actual_files.is_empty() {
        return json!({
            "sha": sha,
            "error": "no files changed in commit (or commit not found)",
        });
    }

    // Use suffix matching so that repo-relative paths match index-relative paths.
    // predicted: "Sources/AcmeProj/Foo.swift"  actual: "AcmeProj/Sources/AcmeProj/Foo.swift"
    let is_match = |pred: &str, actual: &str| -> bool {
        actual.ends_with(pred) || pred.ends_with(actual) || actual == pred
    };

    let mut tp: Vec<String> = Vec::new();
    let mut fp: Vec<String> = Vec::new();
    let mut fn_: Vec<String> = Vec::new();

    for pred in predicted_files {
        if actual_files.iter().any(|a| is_match(pred, a)) {
            tp.push(pred.clone());
        } else {
            fp.push(pred.clone());
        }
    }
    for actual in &actual_files {
        if !predicted_files.iter().any(|p| is_match(p, actual)) {
            fn_.push(actual.clone());
        }
    }

    let precision = if tp.len() + fp.len() > 0 {
        tp.len() as f64 / (tp.len() + fp.len()) as f64
    } else {
        0.0
    };
    let recall = if tp.len() + fn_.len() > 0 {
        tp.len() as f64 / (tp.len() + fn_.len()) as f64
    } else {
        0.0
    };
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };

    json!({
        "sha": sha,
        "predicted_count": predicted_files.len(),
        "actual_count": actual_files.len(),
        "true_positives": tp,
        "false_positives": fp,
        "false_negatives": fn_,
        "precision": (precision * 1000.0).round() / 1000.0,
        "recall": (recall * 1000.0).round() / 1000.0,
        "f1": (f1 * 1000.0).round() / 1000.0,
    })
}

// ---------------------------------------------------------------------------
// Plan M t-003 (1.0.93): stage helpers extracted from run() body.
//
// Each fn here was inlined inside run() prior to 1.0.93. Extraction
// keeps the orchestrator readable without changing behavior. State
// flows through explicit args — no shared context struct (yet); each
// helper takes only the inputs it needs and returns only the outputs
// the orchestrator consumes.
// ---------------------------------------------------------------------------

/// BFS upward from the top entry-point symbol to collect test
/// callers. For each tier-2 (test) symbol discovered, compute which
/// design invariants the test's name + doc tokens cover.
///
/// Returns a Vec of `{qname, file, line, covers_invariants}` entries.
fn gather_affected_tests(
    engine: &Engine,
    index_store: &AsgIndexStore,
    id_map: &HashMap<String, Symbol>,
    top_sym_id: Option<&str>,
    test_depth: usize,
    design_invariants: &[Value],
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let Some(start_id) = top_sym_id else {
        return out;
    };
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut seen_test_names: HashSet<String> = HashSet::new();
    visited.insert(start_id.to_string());
    queue.push_back((start_id.to_string(), 0));
    while let Some((sid, depth)) = queue.pop_front() {
        if depth >= test_depth {
            continue;
        }
        let callers = index_store
            .get_callers(&engine.ref_name, &sid)
            .unwrap_or_default();
        for cid in callers {
            if visited.contains(&cid) {
                continue;
            }
            visited.insert(cid.clone());
            if let Some(s) = id_map.get(&cid) {
                if symbol_tier(&s.file) == 2 && seen_test_names.insert(s.qname.clone()) {
                    // Use both qname words and doc comment words for
                    // behavioral matching so a test named
                    // "test_plays_silence_at_loop_end" AND a test
                    // doc'd "verifies loop boundary" both surface
                    // the relevant invariant.
                    let qname_words: Vec<String> = s
                        .qname
                        .split(|c: char| !c.is_alphabetic())
                        .filter(|t| t.len() > 2)
                        .map(|t| t.to_lowercase())
                        .collect();
                    let doc_words: Vec<String> = s
                        .doc
                        .as_deref()
                        .unwrap_or("")
                        .split(|c: char| !c.is_alphabetic())
                        .filter(|t| t.len() > 2)
                        .map(|t| t.to_lowercase())
                        .collect();
                    let test_tokens: Vec<&str> = qname_words
                        .iter()
                        .chain(doc_words.iter())
                        .map(|s| s.as_str())
                        .collect();
                    let covers: Vec<&str> = design_invariants
                        .iter()
                        .filter_map(|inv| inv.get("summary").and_then(Value::as_str))
                        .filter(|summary| {
                            let sl = summary.to_lowercase();
                            test_tokens.iter().any(|t| sl.contains(*t))
                        })
                        .collect();
                    out.push(json!({
                        "qname": s.qname,
                        "file": s.file,
                        "line": s.start.line,
                        "covers_invariants": covers,
                    }));
                }
                if depth + 1 < test_depth {
                    queue.push_back((cid, depth + 1));
                }
            }
        }
    }
    out
}

/// BFS upward from the top 5 candidate symbols to compute blast
/// radius: caller/callee layer distribution + concrete top-5 caller
/// chains. Path length capped at 4 to avoid runaway BFS on
/// highly-connected modules.
///
/// Returns a single JSON object — embedded as `safe_change_recipe.
/// blast_radius` in the prepare-change response.
fn compute_blast_radius(
    engine: &Engine,
    index_store: &AsgIndexStore,
    id_map: &HashMap<String, Symbol>,
    candidates: &[(f64, String)],
    layer_overrides: &[(String, String)],
) -> Value {
    let mut caller_layers: HashMap<String, usize> = HashMap::new();
    let mut callee_layers: HashMap<String, usize> = HashMap::new();
    let mut total_callers = 0usize;
    let mut total_callees = 0usize;
    let top_sids: Vec<String> = candidates
        .iter()
        .take(5)
        .filter_map(|(_, q)| {
            index_store
                .get_symbol_by_qname(&engine.ref_name, q)
                .ok()
                .flatten()
        })
        .map(|s| s.symbol_id)
        .collect();

    // BFS tracking paths so we can emit concrete caller chains.
    // Each path is root-first: [outer_caller, ..., direct_caller, our_symbol].
    let mut top_caller_chains: Vec<Vec<String>> = Vec::new();

    for sid in &top_sids {
        let anchor_qname = id_map.get(sid).map(|s| s.qname.clone()).unwrap_or_default();
        let mut visited: HashSet<String> = HashSet::new();
        let mut q: VecDeque<(String, Vec<String>)> = VecDeque::new();
        visited.insert(sid.clone());
        q.push_back((sid.clone(), vec![anchor_qname.clone()]));
        while let Some((cid, path)) = q.pop_front() {
            if path.len() > 4 {
                continue;
            }
            for caller_id in index_store
                .get_callers(&engine.ref_name, &cid)
                .unwrap_or_default()
            {
                if visited.insert(caller_id.clone()) {
                    if let Some(sym) = id_map.get(&caller_id) {
                        let tier = symbol_tier(&sym.file);
                        let layer =
                            classify_layer_sym(&sym.file, &sym.qname, tier, layer_overrides);
                        *caller_layers.entry(layer.to_string()).or_default() += 1;
                        total_callers += 1;
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
        for callee_id in index_store
            .get_callees(&engine.ref_name, sid)
            .unwrap_or_default()
        {
            if let Some(sym) = id_map.get(&callee_id) {
                let tier = symbol_tier(&sym.file);
                let layer = classify_layer_sym(&sym.file, &sym.qname, tier, layer_overrides);
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
}

/// Plan M t-003 (1.0.94): test-gap detection extracted from run().
/// Given current file_scores + affected_tests, returns:
///   - test_gap: true when no covering tests were found
///   - proposed_test_path: real indexed test file if any, else suggested path
///   - proposed_test_stub: language-aware test-framework body shape
fn detect_test_gap(
    file_scores: &[(f64, String, String, Option<f64>, bool, String, String)],
    affected_tests: &[Value],
    all_test_file_paths: &[String],
) -> (bool, Option<String>, Option<String>) {
    let test_gap = affected_tests.is_empty();
    if !test_gap {
        return (false, None, None);
    }
    let source = file_scores
        .first()
        .map(|(_, f, _, _, _, _, _)| f.as_str())
        .unwrap_or("");
    if source.is_empty() {
        return (true, None, None);
    }
    let real = test_files_for_source(all_test_file_paths, source);
    let proposed_test_path = if real.is_empty() {
        Some(format!(
            "no known test target (suggested: {})",
            propose_test_path(source)
        ))
    } else {
        Some(real.join(", "))
    };
    // file_scores tuple shape: (score, file, layer, days, hot, qname, why)
    let symbol = file_scores
        .first()
        .map(|(_, _, _, _, _, qname, _)| qname.as_str())
        .unwrap_or("change");
    let proposed_test_stub = Some(propose_test_stub(source, symbol));
    (true, proposed_test_path, proposed_test_stub)
}
