//! `asd investigate <query>` — broad feature archaeology in one pass.
//!
//! 1. FTS5 hybrid search to find entry points (falls back to in-memory).
//! 2. For each top result: callers, callees, effects, invariants, hazards, notes.
//! 3. Prints a structured JSON report.

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, Engine,
    FeedbackStore, FtsFilters, IndexStore, LedgerStore, OwnershipSignal,
    apply_feedback_adjustments, build_feedback_state_from_entries, classify_layer_sym,
    compute_trust_score, compute_uncertainty, confidence_scores, detect_ambiguous_tokens,
    detect_possible_misses, discover_symbol_ownership, estimate_tokens, explain_match,
    extract_summary, find_candidates, gather_recency, git_dirty_files, intent_focus,
    intent_layer_order, load_layer_overrides, parse_intent, parse_query, resolve_scope,
    result_bucket, stale_warning, symbol_tier, trim_for_agent,
};

use crate::commands::context_for::assemble_symbol_context;
use crate::config::Config;

#[derive(Debug, Args)]
pub struct InvestigateArgs {
    /// Natural-language or keyword query. Scored across symbol name,
    /// signature, doc comment, file path, and ledger entries.
    /// Plan D t-002: multi-word queries are accepted unquoted —
    /// `asd investigate failing test store` joins to "failing test store".
    /// Quoting still works for queries containing flag-like tokens.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,

    /// Number of top entry-point symbols to fully expand (default: 10).
    /// Alias: --limit (both accepted).
    #[arg(long, default_value = "10")]
    pub depth: usize,

    /// Maximum results to expand. Overrides --depth when provided.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Filter by symbol kind: module, function, method, class, variable.
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by language (e.g. "swift", "python", "typescript", "rust").
    #[arg(long)]
    pub language: Option<String>,

    /// Include full source body of each symbol in output (can be large).
    #[arg(long, default_value = "false")]
    pub include_body: bool,

    /// Include symbols from test files in entry-point candidates.
    #[arg(long)]
    pub include_tests: bool,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Return a flat `entry_points` array instead of the default `by_layer` grouped output.
    #[arg(long)]
    pub flat: bool,

    /// Maximum entry points per layer in grouped output (default: unlimited).
    #[arg(long)]
    pub max_per_layer: Option<usize>,

    /// Adjust output ordering and guidance for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    #[arg(long)]
    pub intent: Option<String>,

    /// Emit token-budgeted JSON for LLM consumption. Trims bodies,
    /// collapses low-signal fields, adds token_estimate.
    #[arg(long)]
    pub agent: bool,

    /// Token budget when --agent is set (default: 8000).
    #[arg(long, default_value = "8000")]
    pub agent_budget: usize,

    /// Comma-separated terms to exclude. Also supports inline minus-prefix
    /// syntax in the query, e.g. "drift playhead -sample -waveform".
    #[arg(long)]
    pub exclude: Option<String>,

    /// Comma-separated glob patterns to restrict results to specific paths,
    /// e.g. --paths "App/**/DriftPad*,Packages/SequencerCore/**".
    #[arg(long)]
    pub paths: Option<String>,

    /// Named scope alias from .asd/scopes.toml, e.g. --scope drift-pad.
    #[arg(long)]
    pub scope: Option<String>,

    /// Output is always JSON; this flag is accepted for CLI consistency.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: InvestigateArgs) -> Result<()> {
    // Plan D t-002: join unquoted multi-word queries into a single string.
    // `asd investigate failing test store` now works the same as
    // `asd investigate "failing test store"`.
    let query: String = args.query.join(" ");
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    if args.intent.is_some() && intent.is_empty() {
        eprintln!(
            "asd: unknown intent {:?} — valid values: bugfix, feature, refactor, test, architecture, ui",
            args.intent.as_deref().unwrap_or("")
        );
    }
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);
    let id_map = index_store.build_id_map(&engine);

    let (tokens, mut exclusions) = parse_query(&query);
    if let Some(ref excl) = args.exclude {
        for term in excl
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
        {
            exclusions.push(term);
        }
    }
    if tokens.is_empty() {
        println!("{}", json!({ "query": query, "entry_points": [] }));
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
    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
        tests_only: false,
        exclude_terms: exclusions,
        paths_filter,
        exclude_paths: Vec::new(),
        exclude_languages: Vec::new(),    };

    // Each entry_point candidate: (combined_score, symbol_id, qname)
    // We resolve full Symbol via index_store for context assembly.
    // Returns (score, qname) pairs.
    let limit = args.limit.unwrap_or(args.depth);
    let mut candidates: Vec<(f64, String)> = find_candidates(
        &engine,
        &query,
        &tokens,
        &filters,
        &ledger_store,
        &index_store,
        limit,
    );

    // Apply durable feedback adjustments (Useful/Noisy/WrongLayer verdicts).
    // list_all() hoisted once — reused for build_feedback_state_from_entries below.
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    let all_fb = feedback_store
        .list_all(&engine.ref_name)
        .unwrap_or_default();
    // Plan J t-016: tuple includes created_at for age-decay.
    let feedback_verdicts: Vec<(
        String,
        String,
        agentstatedeveloper_core::FeedbackVerdict,
        chrono::DateTime<chrono::Utc>,
    )> = all_fb
        .iter()
        .filter(|e| e.file_scope.is_none())
        .map(|e| (e.symbol_id.clone(), e.query.clone(), e.verdict, e.created_at))
        .collect();
    let feedback_metrics = apply_feedback_adjustments(
        &engine,
        &index_store,
        &query,
        &mut candidates,
        &feedback_verdicts,
    );

    // One git pass to gather recency for all candidate files (hot = 14 days).
    let recency = gather_recency(200, 14.0);

    // Per-file ownership cache: discover_symbol_ownership spawns git blame + git log.
    // With up to 10 candidates, many may share files — computing once per file
    // avoids up to N*2 redundant git subprocess spawns.
    let mut ownership_cache: std::collections::HashMap<String, OwnershipSignal> =
        std::collections::HashMap::new();

    let mut entry_points: Vec<Value> = Vec::new();
    for (score, qname) in &candidates {
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
        let ledger_entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();
        let has_ledger = !ledger_entries.is_empty();
        let match_reasons = explain_match(&sym, &tokens, &ledger_entries, hot);
        // Compute ownership once per unique file; reuse for all symbols in the same file.
        let ownership_hint = ownership_cache.entry(sym.file.clone()).or_insert_with(|| {
            discover_symbol_ownership(&sym.file, sym.start.line, sym.end.line, sym.doc.as_deref())
        });
        let ctx = assemble_symbol_context(
            &engine,
            &index_store,
            &effect_store,
            &ledger_store,
            &sym,
            &id_map,
            args.include_body,
            engine.fts.as_ref(),
            Some(ownership_hint),
        )?;
        let bucket = result_bucket(&sym.file, &match_reasons, has_ledger, hot);
        let mut ep = json!({
            "score": score,
            "layer": layer,
            "summary": summary,
            "last_touched_days": last_touched_days,
            "hot": hot,
            "match_reasons": match_reasons,
            "bucket": bucket,
        });
        if let (Some(obj), Some(ctx_obj)) = (ep.as_object_mut(), ctx.as_object()) {
            for (k, v) in ctx_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        entry_points.push(ep);
    }

    // Aggregate invariants and hazards across all entry points into a single
    // top-level section — the anti-footgun guard an agent should read first.
    let mut all_invariants: Vec<Value> = Vec::new();
    let mut all_hazards: Vec<Value> = Vec::new();
    let mut seen_invariants: std::collections::HashSet<String> = std::collections::HashSet::new();

    for ep in &entry_points {
        let qname = ep
            .get("symbol")
            .and_then(|s| s.get("qname"))
            .and_then(Value::as_str)
            .unwrap_or("");

        if let Some(invs) = ep.get("invariants").and_then(Value::as_array) {
            for inv in invs {
                let key = inv
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !key.is_empty() && seen_invariants.insert(key) {
                    let mut v = inv.clone();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("source_qname".to_string(), Value::String(qname.to_string()));
                    }
                    all_invariants.push(v);
                }
            }
        }
        if let Some(hzs) = ep.get("hazards").and_then(Value::as_array) {
            for hz in hzs {
                let mut v = hz.clone();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("source_qname".to_string(), Value::String(qname.to_string()));
                }
                all_hazards.push(v);
            }
        }
    }

    // Build by_layer grouped view (layer → [entry_points]).
    // Layer order is intent-aware; intent="" falls back to default order.
    let layer_order = intent_layer_order(intent);
    let mut by_layer: serde_json::Map<String, Value> = serde_json::Map::new();
    for layer_key in layer_order {
        let mut members: Vec<Value> = entry_points
            .iter()
            .filter(|ep| ep.get("layer").and_then(Value::as_str) == Some(*layer_key))
            .cloned()
            .collect();
        if let Some(max) = args.max_per_layer {
            members.truncate(max);
        }
        if !members.is_empty() {
            by_layer.insert(layer_key.to_string(), Value::Array(members));
        }
    }

    let focus = intent_focus(intent);

    // --- Uncertainty model -----------------------------------------------
    let raw_scores: Vec<f64> = candidates.iter().map(|(s, _)| *s).collect();
    let confidences = confidence_scores(&raw_scores);
    // Attach confidence to each entry_point in order.
    for (ep, conf) in entry_points.iter_mut().zip(confidences.iter()) {
        if let Some(obj) = ep.as_object_mut() {
            obj.insert("confidence".to_string(), json!(conf));
        }
    }
    // --- Staleness warnings -----------------------------------------------
    let dirty = git_dirty_files();
    let stale_symbols: Vec<String> = entry_points
        .iter()
        .filter_map(|ep| {
            ep.get("symbol")
                .and_then(|s| s.get("file"))
                .and_then(Value::as_str)
        })
        .filter(|f| dirty.contains(*f))
        .map(|s| s.to_string())
        .collect();

    let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
    let layers_present: std::collections::HashSet<&str> = entry_points
        .iter()
        .filter_map(|ep| ep.get("layer").and_then(Value::as_str))
        .collect();
    // t-001: suppress possible-miss warnings when scope is intentionally narrowed.
    let scope_narrowed = !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
    let possible_misses = if scope_narrowed {
        vec![]
    } else {
        detect_possible_misses(&query, &layers_present, entry_points.len())
    };

    // Scoped suggestions (investigate doesn't compute these — pass empty slice).
    let dq_state = compute_trust_score(&cfg.db_path).data_quality.state;
    let uncertainty = compute_uncertainty(
        &tokens,
        &ambiguous_terms,
        &possible_misses,
        entry_points.len(),
        &[],
        engine.fts.as_ref(),
        Some(dq_state.as_str()),
    );

    // Default: grouped by_layer output (compact, deduped by layer).
    // --flat restores the legacy flat entry_points array.
    // Invariants/hazards surfaced first so agents see constraints before call graphs.

    let feedback_state =
        build_feedback_state_from_entries(&all_fb, &query, feedback_metrics.entries_applied);
    let coverage = if !feedback_state.available {
        "none"
    } else if feedback_state.query_matches == 0 {
        "none"
    } else if feedback_metrics.entries_applied > 0 {
        "applied"
    } else {
        "partial"
    };
    let feedback_summary = json!({
        "entries_applied": feedback_metrics.entries_applied,
        "suppressed": feedback_metrics.suppressed,
        "preserved_useful_siblings": feedback_metrics.preserved_useful_siblings,
        "boosted": feedback_metrics.boosted,
        "recurring_fp_suppressed": feedback_metrics.recurring_fp_suppressed,
        "rules_applied": feedback_metrics.rules_applied,
        "entries_total": feedback_state.entries_total,
        "query_matches": feedback_state.query_matches,
        "coverage": coverage,
    });
    let out = if args.flat {
        json!({
            "query": query,
            "intent": if intent.is_empty() { Value::Null } else { Value::String(intent.to_string()) },
            "focus": if focus.is_empty() { Value::Null } else { Value::String(focus.to_string()) },
            "tokens": tokens,
            "uncertainty": uncertainty.to_json(),
            "ambiguous_terms": ambiguous_terms,
            "possible_misses": possible_misses,
            "scope_narrowed": scope_narrowed,
            "stale_symbols": stale_symbols,
            "feedback_state": feedback_state.to_json(),
            "feedback_summary": feedback_summary,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "entry_points": entry_points,
        })
    } else {
        json!({
            "query": query,
            "intent": if intent.is_empty() { Value::Null } else { Value::String(intent.to_string()) },
            "focus": if focus.is_empty() { Value::Null } else { Value::String(focus.to_string()) },
            "tokens": tokens,
            "uncertainty": uncertainty.to_json(),
            "ambiguous_terms": ambiguous_terms,
            "possible_misses": possible_misses,
            "scope_narrowed": scope_narrowed,
            "stale_symbols": stale_symbols,
            "feedback_state": feedback_state.to_json(),
            "feedback_summary": feedback_summary,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "by_layer": by_layer,
        })
    };
    // Token economy (1.0.78/79): agent mode emits compact JSON, drops
    // input-echo + top-level empty/null fields.
    let (out, compact_for_agent) = if args.agent {
        let max_list = (args.agent_budget / 500).max(3).min(20);
        let mut trimmed = trim_for_agent(&out, max_list);
        if let Some(obj) = trimmed.as_object_mut() {
            obj.remove("query");
        }
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
