//! `asd search <query>` — ranked concept search over indexed symbols.
//!
//! Primary path: BM25 via FTS5 table populated at `asd index` time.
//! Hybrid reranking: FTS BM25 score + ledger-text token boost.
//! Fallback: in-memory O(N) scoring when FTS table is empty or absent.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore,
    EffectStore, Engine, FeedbackStore, FeedbackVerdict, FtsFilters, IndexStore,
    LedgerStore, SearchDocsDb, SearchFtsDb,
    apply_feedback_adjustments, apply_file_scope_feedback, build_feedback_state_from_entries,
    classify_layer_sym,
    compute_trust_score, confidence_reason, compute_uncertainty, FeedbackMetrics, FeedbackState,
    confidence_scores, detect_ambiguous_tokens, detect_confidence_warnings, detect_possible_misses,
    effect_detail_reason, estimate_tokens, explain_match, explain_feedback_impacts, extract_summary,
    gather_recency, hybrid_boost, in_memory_score,
    intent_focus, kind_str, load_layer_overrides, parse_intent, parse_query, resolve_scope,
    result_bucket, stale_warning, suggest_better_queries, suggest_scoped_queries, symbol_tier,
    trim_for_agent,
};

use crate::config::Config;

const ASD_PATH_PREFIX: &str = "/asd/v1";

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Concept or keyword(s) to search for. Scored across symbol name,
    /// signature, doc comment, file path, and ledger entry summaries.
    pub query: String,

    /// Filter by symbol kind: module, function, method, class, variable.
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by language (e.g. "swift", "python", "typescript", "rust").
    #[arg(long)]
    pub language: Option<String>,

    /// Maximum results to show (default: 20).
    #[arg(long, default_value = "20")]
    pub limit: usize,

    /// Include symbols from test files in results. By default tests are
    /// excluded so production entry points rank first.
    #[arg(long)]
    pub include_tests: bool,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Adjust guidance context for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    #[arg(long)]
    pub intent: Option<String>,

    /// Emit token-budgeted JSON for LLM consumption. Trims bodies and
    /// collapses low-signal fields; adds token_estimate.
    #[arg(long)]
    pub agent: bool,

    /// Token budget when --agent is set (default: 8000).
    #[arg(long, default_value = "8000")]
    pub agent_budget: usize,

    /// Comma-separated terms to exclude. Candidates whose qname, file, doc,
    /// or signature contain any term are dropped. Also supports inline
    /// minus-prefix syntax in the query, e.g. "drift playhead -sample".
    #[arg(long)]
    pub exclude: Option<String>,

    /// Comma-separated glob patterns to restrict results to specific paths,
    /// e.g. --paths "App/**/DriftPad*,Packages/SequencerCore/**".
    #[arg(long)]
    pub paths: Option<String>,

    /// Named scope alias from .asd/scopes.toml, e.g. --scope drift-pad.
    /// Expanded to the path globs defined in the scopes file.
    #[arg(long)]
    pub scope: Option<String>,

    /// Print match reasons for each result (which token matched which field,
    /// ledger involvement). Implied by --agent; this shows it in terminal output.
    #[arg(long)]
    pub explain: bool,

    /// Restrict results to semantic symbols only (skip document index).
    #[arg(long)]
    pub symbols_only: bool,

    /// Restrict results to document/resource hits only (skip symbol index).
    #[arg(long)]
    pub docs_only: bool,

    /// Print per-result score breakdown: BM25, hybrid boost, ledger boost,
    /// SOT boost, state-holder penalty, and domain-overlap count.
    #[arg(long)]
    pub debug_boosts: bool,
}

pub fn run(cfg: &Config, args: SearchArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    if !intent.is_empty() {
        eprintln!("intent: {}", intent_focus(intent));
    }
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);
    // Hoist trust/data-quality state — used by uncertainty model and avoid
    // re-opening the DB or sidecar inside the hot search path.
    let dq_state_str: String = {
        let trust = compute_trust_score(&cfg.db_path);
        trust.data_quality.state.clone()
    };
    // Hoist all feedback entries — previously called 2-3× per search (once for
    // score adjustment, once for result annotation, once for display badges).
    let all_feedback: Vec<agentstatedeveloper_core::FeedbackEntry> = {
        let fb_store = AsgFeedbackStore::from_engine(&engine);
        fb_store.list_all(&engine.ref_name).unwrap_or_default()
    };

    let (tokens_from_query, mut inline_exclusions) = parse_query(&args.query);
    if let Some(ref excl) = args.exclude {
        for term in excl.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
            inline_exclusions.push(term);
        }
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
        exclude_terms: inline_exclusions,
        paths_filter,
    };

    // --- Document hits (broad corpus) ---
    let doc_hits = if !args.symbols_only {
        SearchDocsDb::open(&cfg.db_path)
            .ok()
            .filter(|db| !db.is_empty())
            .and_then(|db| db.search(&tokens_from_query, args.limit, None).ok())
            .unwrap_or_default()
    } else {
        vec![]
    };

    // --- FTS path ---
    let fts_result = if args.docs_only { None } else {
        SearchFtsDb::open(&cfg.db_path)
            .ok()
            .filter(|fts| fts.has_data())
            // Fetch limit*2 candidates (was *4). The extra factor is needed for
            // reranking after ledger/SOT boosts; *2 keeps accuracy well enough for
            // the golden probe suite while cutting ledger-read count in half vs *4.
            // Bump back to *3/*4 if a ranking regression is observed.
            .and_then(|fts| fts.search(&args.query, &filters, args.limit * 2).ok())
    };

    if let Some(hits) = fts_result {
        let tokens = tokens_from_query.clone();

        // Hybrid reranking: BM25 + path/name boost + ledger boost + ownership anchor boost.
        // Also track which symbols received a SOT boost (for boosted-but-outranked report).
        let mut sot_boosted_qnames: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Per-result boost breakdown (populated when --debug-boosts is set).
        let mut boost_debug: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        const GENERIC_BOOST_SKIP: &[&str] = &[
            "state", "update", "position", "value", "cursor", "progress",
            "indicator", "status", "mode", "flag", "current", "local",
            "playhead", "tick", "item", "data", "info", "manager",
        ];
        // M59: ledger_cache is now lazily populated at display time only (top ~20 results).
        // Scoring uses denormalized ledger_text/ledger_flags from FTS rows — no list_entries
        // calls during the hot scoring loop (was N=80 calls, now 0).
        let mut ledger_cache: std::collections::HashMap<String, Vec<agentstatedeveloper_core::LedgerEntry>> =
            std::collections::HashMap::new();
        // has_ledger_ids: symbol_ids with any ledger entries — populated from FTS fields.
        let mut has_ledger_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut scored: Vec<(f64, _)> = {
            let mut tmp = Vec::with_capacity(hits.len());
            for hit in hits {
                let hybrid = hybrid_boost(&hit, &tokens);
                // M59: use denormalized fields — no list_entries call.
                let ledger_boost = if hit.ledger_text.is_empty() {
                    0.0
                } else {
                    tokens.iter().filter(|t| hit.ledger_text.contains(t.as_str())).count() as f64
                };
                let haystack = format!("{} {}", hit.qname.to_lowercase(), hit.file.to_lowercase());
                let domain_overlap = tokens.iter()
                    .filter(|t| !GENERIC_BOOST_SKIP.contains(&t.as_str()))
                    .filter(|t| haystack.contains(t.as_str()))
                    .count();
                let has_ownership = hit.has_ownership();
                let has_invariant = hit.has_invariant();
                if hit.has_ledger() {
                    has_ledger_ids.insert(hit.symbol_id.clone());
                }
                let is_state_holder = matches!(hit.kind.as_str(), "class" | "struct" | "type" | "enum")
                    && !has_ownership && !has_invariant
                    && !tokens.iter().any(|t| matches!(t.as_str(),
                        "state" | "model" | "type" | "class" | "struct" | "enum" | "schema"));
                let state_penalty = if is_state_holder { -0.8 } else { 0.0 };
                let sot_boost = if has_ownership && domain_overlap >= 2 {
                    5.0
                } else if has_ownership && domain_overlap >= 1 {
                    3.5
                } else if has_ownership {
                    2.0
                } else if has_invariant && domain_overlap >= 1 {
                    1.5
                } else {
                    0.0
                };
                let total = hit.bm25_score + hybrid + ledger_boost + sot_boost + state_penalty;
                if args.debug_boosts {
                    boost_debug.insert(hit.qname.clone(), serde_json::json!({
                        "bm25": hit.bm25_score,
                        "hybrid_boost": hybrid,
                        "ledger_boost": ledger_boost,
                        "sot_boost": sot_boost,
                        "state_penalty": state_penalty,
                        "domain_overlap": domain_overlap,
                        "total": total,
                    }));
                }
                tmp.push((total, hit));
            }
            tmp
        };

        // Record SOT-boosted symbols before truncation for outranked reporting.
        // Uses has_ledger_ids populated from FTS fields — no extra git reads.
        for (_, hit) in &scored {
            if has_ledger_ids.contains(&hit.symbol_id) && hit.has_ownership() {
                sot_boosted_qnames.insert(hit.qname.clone());
            }
        }

        // Apply durable feedback: suppress noisy/wrong-layer, boost useful.
        // Uses the hoisted all_feedback — no extra list_all() call here.
        let mut feedback_metrics = FeedbackMetrics::default();
        let mut feedback_suppressed_detail: Vec<String> = Vec::new();
        {
            let idx = AsgIndexStore::from_engine(&engine);
            if !all_feedback.is_empty() {
                let fb_tuples: Vec<_> = all_feedback.iter()
                    .filter(|e| e.file_scope.is_none())
                    .map(|e| (e.symbol_id.clone(), e.query.clone(), e.verdict))
                    .collect();
                let fs_tuples: Vec<_> = all_feedback.iter()
                    .filter_map(|e| e.file_scope.as_ref().map(|g| (g.clone(), e.verdict, e.query.clone())))
                    .collect();
                let mut adj: Vec<(f64, String)> = scored.iter()
                    .map(|(s, h)| (*s, h.qname.clone()))
                    .collect();
                feedback_metrics = apply_feedback_adjustments(&engine, &idx, &args.query, &mut adj, &fb_tuples);
                apply_file_scope_feedback(&engine, &idx, &args.query, &mut adj, &fs_tuples);
                // Collect suppressed qnames before consuming adj.
                let surviving: std::collections::HashSet<&str> =
                    adj.iter().map(|(_, q)| q.as_str()).collect();
                feedback_suppressed_detail = scored.iter()
                    .map(|(_, h)| h.qname.clone())
                    .filter(|q| !surviving.contains(q.as_str()))
                    .collect();
                let adj_map: std::collections::HashMap<String, f64> =
                    adj.into_iter().map(|(s, q)| (q, s)).collect();
                scored.retain(|(_, h)| adj_map.contains_key(&h.qname));
                for (score, h) in scored.iter_mut() {
                    if let Some(&new_s) = adj_map.get(&h.qname) {
                        *score = new_s;
                    }
                }
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.qname.cmp(&b.1.qname)));
        scored.truncate(args.limit);

        if scored.is_empty() {
            println!("No results for {:?}", args.query);
            return Ok(());
        }

        // One git pass to annotate with recency (hot = changed in last 14 days).
        let recency = gather_recency(200, 14.0);

        let index_store = AsgIndexStore::from_engine(&engine);
        // Uncertainty: compute confidence scores across the result set.
        let raw_scores: Vec<f64> = scored.iter().map(|(s, _)| *s).collect();
        let confidences = confidence_scores(&raw_scores);
        // Uncertainty: detect ambiguous query tokens.
        let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
        if args.agent {
            // Pre-compute feedback impacts for ALL result qnames at once using the
            // hoisted all_feedback — no extra list_all() call here.
            // explain_feedback_impacts does a full symbol-tree scan when noisy symbols exist;
            // calling it per-result meant 20 full tree scans. One batch call eliminates that.
            let all_result_qnames: Vec<String> = scored.iter().map(|(_, h)| h.qname.clone()).collect();
            let all_feedback_impacts = explain_feedback_impacts(
                &engine, &AsgIndexStore::from_engine(&engine),
                &args.query, &all_result_qnames, &all_feedback,
            );

            let mut layers_present: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let results: Vec<serde_json::Value> = scored.iter().zip(confidences.iter()).map(|((score, hit), conf)| {
                let rec = recency.get(&hit.file);
                let is_hot = rec.map(|r| r.hot).unwrap_or(false);
                let tier = symbol_tier(&hit.file);
                let layer = classify_layer_sym(&hit.file, &hit.qname, tier, &layer_overrides);
                layers_present.insert(Box::leak(layer.to_string().into_boxed_str()));
                // M59: lazy-load ledger entries at display time (top N only).
                // Scoring no longer populates ledger_cache; we do it here on demand.
                if !ledger_cache.contains_key(&hit.symbol_id) {
                    if let Ok(entries) = ledger_store.list_entries(&engine.ref_name, &hit.symbol_id) {
                        ledger_cache.insert(hit.symbol_id.clone(), entries);
                    }
                }
                let ledger_entries = ledger_cache.get(&hit.symbol_id)
                    .cloned()
                    .unwrap_or_default();
                // Use has_ledger_ids (from FTS fields) as authoritative source;
                // fall back to cache for completeness.
                let has_ledger = has_ledger_ids.contains(&hit.symbol_id) || !ledger_entries.is_empty();
                let match_reasons = if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, &hit.qname) {
                    explain_match(&sym, &tokens, &ledger_entries, is_hot)
                } else {
                    vec![]
                };
                let bucket = result_bucket(&hit.file, &match_reasons, has_ledger, is_hot);
                let conf_reason = confidence_reason(&match_reasons, has_ledger, is_hot);
                // Check for an active useful feedback verdict (these survive filtering).
                // Uses the hoisted all_feedback — no extra git read per result.
                let fb_status = {
                    let q = args.query.to_lowercase();
                    all_feedback.iter().find(|e| {
                        e.symbol_id == hit.symbol_id
                            && (e.query.is_empty()
                                || q.contains(e.query.as_str())
                                || e.query.contains(q.as_str()))
                    }).map(|e| e.verdict.as_str().to_string())
                };
                // feedback_rule: look up from the pre-computed impact map (no extra git reads).
                let feedback_rule: Option<serde_json::Value> = all_feedback_impacts.get(&hit.qname)
                    .map(|imp| serde_json::json!({
                        "verdict": imp.verdict,
                        "matched_query": imp.matched_query,
                        "author": imp.author,
                    }));
                // effect_detail: one-line reason for effect verification state.
                let effect_detail = {
                    let decl = effect_store.get_effects(&engine.ref_name, &hit.symbol_id)
                        .ok()
                        .flatten();
                    effect_detail_reason(decl.as_ref())
                };
                let mut result_val = serde_json::json!({
                    "score": score, "confidence": conf, "bucket": bucket,
                    "confidence_reason": conf_reason,
                    "qname": hit.qname, "kind": hit.kind,
                    "file": hit.file, "line": hit.line, "layer": layer,
                    "summary": extract_summary(hit.doc.as_deref(), hit.signature.as_deref()),
                    "last_touched_days": rec.and_then(|r| r.last_touched_days),
                    "hot": is_hot,
                    "match_reasons": match_reasons,
                    "feedback_status": fb_status,
                    "feedback_rule": feedback_rule,
                    "effect_detail": effect_detail,
                });
                if args.debug_boosts {
                    if let Some(dbg) = boost_debug.get(&hit.qname) {
                        result_val["boost_debug"] = dbg.clone();
                    }
                }
                result_val
            }).collect();
            let scope_narrowed = !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
            let possible_misses = if scope_narrowed {
                vec![]
            } else {
                detect_possible_misses(&args.query, &layers_present, results.len())
            };
            // t-003: typed confidence warnings (ambiguous vs sparse).
            let confidence_warnings = detect_confidence_warnings(
                &tokens, results.len(), &ambiguous_terms, engine.fts.as_ref(),
            );
            // t-004/t-005: query improvement suggestions.
            let query_suggestions = if scope_narrowed { vec![] } else {
                suggest_better_queries(&tokens, &args.query)
            };
            // t-004: scoped query suggestions using co-occurring tokens from top results.
            let top_qnames: Vec<String> = results.iter()
                .take(5)
                .filter_map(|r| r["qname"].as_str().map(|s| s.to_string()))
                .collect();
            let scoped_suggestions = if scope_narrowed || ambiguous_terms.is_empty() {
                vec![]
            } else {
                suggest_scoped_queries(&tokens, &ambiguous_terms, &top_qnames)
            };
            let doc_results: Vec<serde_json::Value> = doc_hits.iter().map(|h| {
                serde_json::json!({
                    "source": "document",
                    "score": h.bm25_score,
                    "kind": h.kind,
                    "path": h.path,
                    "line": h.span_start,
                    "title": h.title,
                    "preview": h.preview,
                    "owner_symbol_id": h.owner_symbol_id,
                })
            }).collect();
            // boosted_outranked: SOT symbols that got a boost but ranked below position 5
            // or didn't make results at all. Useful for diagnosing cases where an SOT
            // symbol should have surfaced higher — the probe harness can assert these
            // are reported when a known-good symbol slips past the top cut.
            const OUTRANKED_THRESHOLD: usize = 5;
            let result_positions: std::collections::HashMap<&str, usize> = results.iter()
                .enumerate()
                .filter_map(|(i, r)| r["qname"].as_str().map(|q| (q, i + 1)))
                .collect();
            let boosted_outranked: Vec<&str> = sot_boosted_qnames.iter()
                .map(|s| s.as_str())
                .filter(|q| match result_positions.get(*q) {
                    Some(&rank) => rank > OUTRANKED_THRESHOLD,
                    None => true, // not in results at all
                })
                .collect();

            // Use hoisted dq_state_str — avoids re-opening sidecar/DB for trust data.
            let uncertainty = compute_uncertainty(
                &tokens, &ambiguous_terms, &possible_misses,
                results.len(), &scoped_suggestions, engine.fts.as_ref(),
                Some(dq_state_str.as_str()),
            );
            // Use hoisted all_feedback — avoids a second list_all() call.
            let feedback_state = build_feedback_state_from_entries(
                &all_feedback, &args.query, feedback_metrics.entries_applied,
            );
            let raw = serde_json::json!({
                "query": args.query,
                "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
                "uncertainty": uncertainty.to_json(),
                "ambiguous_terms": ambiguous_terms,
                "possible_misses": possible_misses,
                "confidence_warnings": confidence_warnings,
                "query_suggestions": query_suggestions,
                "scoped_suggestions": scoped_suggestions,
                "scope_narrowed": scope_narrowed,
                "feedback_suppressed": feedback_metrics.suppressed,
                "feedback_suppressed_detail": feedback_suppressed_detail,
                "boosted_outranked": boosted_outranked,
                "feedback_state": feedback_state.to_json(),
                "feedback_summary": {
                    "entries_applied": feedback_metrics.entries_applied,
                    "suppressed": feedback_metrics.suppressed,
                    "preserved_useful_siblings": feedback_metrics.preserved_useful_siblings,
                    "boosted": feedback_metrics.boosted,
                    "recurring_fp_suppressed": feedback_metrics.recurring_fp_suppressed,
                    "boosted_outranked": boosted_outranked.len(),
                    "rules_applied": feedback_metrics.rules_applied,
                    "entries_total": feedback_state.entries_total,
                    "query_matches": feedback_state.query_matches,
                    "coverage": if !feedback_state.available { "none" }
                                else if feedback_state.query_matches == 0 { "none" }
                                else if feedback_metrics.entries_applied > 0 { "applied" }
                                else { "partial" },
                },
                "results": results,
                "document_hits": doc_results,
            });
            let max_list = (args.agent_budget / 500).max(3).min(20);
            let trimmed = trim_for_agent(&raw, max_list);
            let json_str = serde_json::to_string_pretty(&trimmed)?;
            let token_est = estimate_tokens(&json_str);
            let mut out = trimmed.clone();
            if let Some(obj) = out.as_object_mut() {
                obj.insert("token_estimate".into(), serde_json::json!(token_est));
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            // t-005: show query suggestions before results.
            let scope_narrowed_term = !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
            let q_suggestions = if scope_narrowed_term { vec![] } else { suggest_better_queries(&tokens, &args.query) };
            for s in &q_suggestions {
                eprintln!("asd: {}", s);
            }

            // Use hoisted all_feedback for display badges — no extra list_all() call.
            let fb_for_display = &all_feedback;
            let query_norm = args.query.to_lowercase();
            for (idx, (score, hit)) in scored.iter().enumerate() {
                let rec = recency.get(&hit.file);
                let is_hot = rec.map(|r| r.hot).unwrap_or(false);
                let hot_tag = if is_hot { " [hot]" } else { "" };
                let age_tag = rec
                    .and_then(|r| r.last_touched_days)
                    .map(|d| format!(" ~{:.0}d ago", d))
                    .unwrap_or_default();
                // Feedback badge: show [useful] when a matching verdict boosted this result.
                let fb_tag = if !fb_for_display.is_empty() {
                    let q = query_norm.as_str();
                    let has_useful = fb_for_display.iter().any(|e| {
                        e.symbol_id == hit.symbol_id
                            && (e.query.is_empty()
                                || q.contains(e.query.as_str())
                                || e.query.contains(q))
                            && matches!(e.verdict, FeedbackVerdict::Useful)
                    });
                    if has_useful { " [useful]" } else { "" }
                } else {
                    ""
                };
                // t-002: confidence bucket + reason label in terminal output.
                // M59: lazy-load ledger entries for display (only called for top N shown results).
                let conf = confidences.get(idx).copied().unwrap_or(0.5);
                if !ledger_cache.contains_key(&hit.symbol_id) {
                    if let Ok(entries) = ledger_store.list_entries(&engine.ref_name, &hit.symbol_id) {
                        ledger_cache.insert(hit.symbol_id.clone(), entries);
                    }
                }
                let cached_entries = ledger_cache.get(&hit.symbol_id)
                    .cloned()
                    .unwrap_or_default();
                let (bucket, conf_reason_str, match_reasons_disp) = if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, &hit.qname) {
                    let has_ledger = has_ledger_ids.contains(&hit.symbol_id) || !cached_entries.is_empty();
                    let reasons = explain_match(&sym, &tokens, &cached_entries, is_hot);
                    let b = result_bucket(&hit.file, &reasons, has_ledger, is_hot);
                    let cr = confidence_reason(&reasons, has_ledger, is_hot);
                    (b, cr, reasons)
                } else {
                    ("noisy", "weak: low-signal indirect match".to_string(), vec![])
                };
                // Cap confidence display when query is all-generic (avoids "[relevant 100%]"
                // on matches that only hit generic tokens with no domain anchor).
                let all_generic_query = !tokens.is_empty()
                    && !ambiguous_terms.is_empty()
                    && tokens.iter().all(|t| ambiguous_terms.contains(t));
                let (display_conf, display_bucket) = if all_generic_query && conf > 0.5 {
                    (0.5_f64, "peripheral")
                } else {
                    (conf, bucket)
                };
                let conf_tag = format!(" [{} {:.0}%]", display_bucket, display_conf * 100.0);
                println!(
                    "[{:.1}] {} {}{}{}{}{} ({}:{})",
                    score, hit.kind, hit.qname, hot_tag, fb_tag, conf_tag, age_tag, hit.file, hit.line
                );
                if let Some(sig) = &hit.signature {
                    if !sig.is_empty() { println!("       sig: {}", sig); }
                }
                let summary = extract_summary(hit.doc.as_deref(), hit.signature.as_deref());
                if !summary.is_empty() { println!("       {}", summary); }
                if args.explain {
                    println!(
                        "       confidence: {:.0}%  {} ({})",
                        conf * 100.0,
                        conf_reason_str,
                        bucket
                    );
                    if !match_reasons_disp.is_empty() {
                        println!("       signals: {}", match_reasons_disp.join(", "));
                    }
                }
                if args.debug_boosts {
                    if let Some(dbg) = boost_debug.get(&hit.qname) {
                        println!(
                            "       boosts: bm25={:.2} hybrid={:.2} ledger={:.2} sot={:.2} state_pen={:.2} domain_overlap={}  => total={:.2}",
                            dbg["bm25"].as_f64().unwrap_or(0.0),
                            dbg["hybrid_boost"].as_f64().unwrap_or(0.0),
                            dbg["ledger_boost"].as_f64().unwrap_or(0.0),
                            dbg["sot_boost"].as_f64().unwrap_or(0.0),
                            dbg["state_penalty"].as_f64().unwrap_or(0.0),
                            dbg["domain_overlap"].as_u64().unwrap_or(0),
                            dbg["total"].as_f64().unwrap_or(0.0),
                        );
                    }
                }
            }
            if feedback_metrics.suppressed > 0 {
                eprintln!("asd: {} result(s) suppressed by feedback (use `asd feedback list` to review)", feedback_metrics.suppressed);
            }
            // Try-narrowing suggestion: when ambiguous terms dominate, suggest scoped queries.
            if !ambiguous_terms.is_empty() {
                let top_qnames: Vec<String> = scored.iter().take(5)
                    .map(|(_, h)| h.qname.clone()).collect();
                let narrowing = suggest_scoped_queries(&tokens, &ambiguous_terms, &top_qnames);
                if !narrowing.is_empty() {
                    eprintln!("asd: try narrowing with: {}", narrowing.join(", "));
                }
            }
            // Print document hits below symbol hits.
            if !doc_hits.is_empty() {
                println!("\n-- document hits --");
                for h in &doc_hits {
                    let line_tag = h.span_start.map(|l| format!(":{l}")).unwrap_or_default();
                    println!("[{:.1}] {} {}{}", h.bm25_score, h.kind, h.path, line_tag);
                    if !h.title.is_empty() { println!("       {}", h.title); }
                    if !h.preview.is_empty() { println!("       {}", &h.preview.chars().take(120).collect::<String>()); }
                }
            }
        }
        return Ok(());
    }

    // --- Fallback: in-memory O(N) scoring ---
    eprintln!("asd: FTS index not populated — falling back to in-memory search (run `asd index` to enable fast search)");

    let tokens = tokens_from_query;
    if tokens.is_empty() {
        println!("[]");
        return Ok(());
    }

    let kind_filter = args.kind.as_deref().map(|k| k.to_lowercase());
    let lang_filter = args.language.as_deref();

    let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
    let qnames: Vec<String> = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => vec![],
    };

    let index = AsgIndexStore::from_engine(&engine);
    let mut scored: Vec<(u32, agentstatedeveloper_core::Symbol)> = Vec::new();

    for qname in &qnames {
        let sym = match index.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };

        if let Some(ref k) = kind_filter {
            let sym_kind = kind_str(&sym.kind);
            if sym_kind != k.as_str() { continue; }
        }
        if let Some(lang) = lang_filter {
            if sym.language != lang { continue; }
        }

        let score = in_memory_score(&sym, &tokens, &ledger_store, &engine);
        if score > 0 {
            scored.push((score, sym));
        }
    }

    // Apply durable feedback on fallback path too.
    // Reuse hoisted all_feedback — no extra list_all() call.
    {
        let idx = AsgIndexStore::from_engine(&engine);
        let fb = &all_feedback;
        if !fb.is_empty() {
            let fb_tuples: Vec<_> = fb.iter()
                .filter(|e| e.file_scope.is_none())
                .map(|e| (e.symbol_id.clone(), e.query.clone(), e.verdict))
                .collect();
            let fs_tuples: Vec<_> = fb.iter()
                .filter_map(|e| e.file_scope.as_ref().map(|g| (g.clone(), e.verdict, e.query.clone())))
                .collect();
            let mut adj: Vec<(f64, String)> = scored.iter()
                .map(|(s, sym)| (*s as f64, sym.qname.clone()))
                .collect();
            let _ = apply_feedback_adjustments(&engine, &idx, &args.query, &mut adj, &fb_tuples);
            apply_file_scope_feedback(&engine, &idx, &args.query, &mut adj, &fs_tuples);
            let surviving: std::collections::HashSet<String> =
                adj.into_iter().map(|(_, q)| q).collect();
            scored.retain(|(_, sym)| surviving.contains(&sym.qname));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.qname.cmp(&b.1.qname)));
    scored.truncate(args.limit);

    if scored.is_empty() {
        println!("No results for {:?}", args.query);
        return Ok(());
    }

    for (score, sym) in &scored {
        let kind = kind_str(&sym.kind);
        println!("[{:3}] {} {} ({})", score, kind, sym.qname, sym.file);
        if let Some(sig) = sym.signature.as_deref() {
            if !sig.is_empty() { println!("       sig: {}", sig); }
        }
        let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
        if !summary.is_empty() {
            println!("       {}", summary);
        }
        if args.explain {
            let entries = ledger_store
                .list_entries(&engine.ref_name, &sym.symbol_id)
                .unwrap_or_default();
            let reasons = explain_match(sym, &tokens, &entries, false);
            if !reasons.is_empty() {
                println!("       why: {}", reasons.join(", "));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

