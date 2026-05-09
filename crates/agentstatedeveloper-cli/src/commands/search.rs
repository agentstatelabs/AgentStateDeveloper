//! `asd search <query>` — ranked concept search over indexed symbols.
//!
//! Primary path: BM25 via FTS5 table populated at `asd index` time.
//! Hybrid reranking: FTS BM25 score + ledger-text token boost.
//! Fallback: in-memory O(N) scoring when FTS table is empty or absent.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{
    AGENT_DEFAULT_BUDGET, AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore,
    EffectStore, Engine, FeedbackStore, FeedbackVerdict, FtsFilters, IndexStore, LedgerKind,
    LedgerStore, SearchDocsDb, SearchFtsDb,
    apply_feedback_adjustments, apply_file_scope_feedback, classify_layer_sym, confidence_reason,
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
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };

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
            .and_then(|fts| fts.search(&args.query, &filters, args.limit * 4).ok())
    };

    if let Some(hits) = fts_result {
        let tokens = tokens_from_query.clone();

        // Hybrid reranking: BM25 + path/name boost + ledger boost + ownership anchor boost.
        let mut scored: Vec<(f64, _)> = hits
            .into_iter()
            .map(|hit| {
                let boost = hybrid_boost(&hit, &tokens);
                let (ledger_boost, ownership_boost) = {
                    let entries = ledger_store
                        .list_entries(&engine.ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let text = entries
                        .iter()
                        .map(|e| e.summary.to_lowercase())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let text_boost = if text.is_empty() {
                        0.0
                    } else {
                        tokens.iter().filter(|t| text.contains(t.as_str())).count() as f64
                    };
                    // Source-of-truth boost: scaled by query-term overlap so SOT symbols
                    // that directly match the query float well above generic matches.
                    // Invariant-bearing symbols with name overlap get an additional bump.
                    let haystack = format!("{} {}", hit.qname.to_lowercase(), hit.file.to_lowercase());
                    let name_overlap = tokens.iter().filter(|t| haystack.contains(t.as_str())).count();
                    let has_ownership = entries.iter().any(|e| e.kind == LedgerKind::Ownership);
                    let has_invariant = entries.iter().any(|e| e.kind == LedgerKind::Invariant);
                    let sot_boost = if has_ownership && name_overlap >= 2 {
                        5.0  // strong: SOT symbol whose name directly matches the query
                    } else if has_ownership && name_overlap >= 1 {
                        3.5  // moderate: SOT symbol with partial name overlap
                    } else if has_ownership {
                        2.0  // baseline: SOT symbol, no name overlap
                    } else if has_invariant && name_overlap >= 1 {
                        1.5  // invariant-bearing symbol that matches the query
                    } else {
                        0.0
                    };
                    (text_boost, sot_boost)
                };
                (hit.bm25_score + boost + ledger_boost + ownership_boost, hit)
            })
            .collect();

        // Apply durable feedback: suppress noisy/wrong-layer, boost useful.
        let mut feedback_suppressed: usize = 0;
        {
            let fb_store = AsgFeedbackStore { repo: &engine.repo };
            let idx = AsgIndexStore { repo: &engine.repo };
            if let Ok(fb) = fb_store.list_all(&engine.ref_name) {
                if !fb.is_empty() {
                    let fb_tuples: Vec<_> = fb.iter()
                        .filter(|e| e.file_scope.is_none())
                        .map(|e| (e.symbol_id.clone(), e.query.clone(), e.verdict))
                        .collect();
                    let fs_tuples: Vec<_> = fb.iter()
                        .filter_map(|e| e.file_scope.as_ref().map(|g| (g.clone(), e.verdict, e.query.clone())))
                        .collect();
                    let mut adj: Vec<(f64, String)> = scored.iter()
                        .map(|(s, h)| (*s, h.qname.clone()))
                        .collect();
                    apply_feedback_adjustments(&engine, &idx, &args.query, &mut adj, &fb_tuples);
                    apply_file_scope_feedback(&engine, &idx, &args.query, &mut adj, &fs_tuples);
                    let adj_map: std::collections::HashMap<String, f64> =
                        adj.into_iter().map(|(s, q)| (q, s)).collect();
                    let before = scored.len();
                    scored.retain(|(_, h)| adj_map.contains_key(&h.qname));
                    feedback_suppressed = before - scored.len();
                    for (score, h) in scored.iter_mut() {
                        if let Some(&new_s) = adj_map.get(&h.qname) {
                            *score = new_s;
                        }
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

        let index_store = AsgIndexStore { repo: &engine.repo };
        // Uncertainty: compute confidence scores across the result set.
        let raw_scores: Vec<f64> = scored.iter().map(|(s, _)| *s).collect();
        let confidences = confidence_scores(&raw_scores);
        // Uncertainty: detect ambiguous query tokens.
        let ambiguous_terms = detect_ambiguous_tokens(&tokens, &cfg.db_path, &filters);
        if args.agent {
            let mut layers_present: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let results: Vec<serde_json::Value> = scored.iter().zip(confidences.iter()).map(|((score, hit), conf)| {
                let rec = recency.get(&hit.file);
                let is_hot = rec.map(|r| r.hot).unwrap_or(false);
                let tier = symbol_tier(&hit.file);
                let layer = classify_layer_sym(&hit.file, &hit.qname, tier, &layer_overrides);
                layers_present.insert(Box::leak(layer.to_string().into_boxed_str()));
                let ledger_entries = ledger_store
                    .list_entries(&engine.ref_name, &hit.symbol_id)
                    .unwrap_or_default();
                let has_ledger = !ledger_entries.is_empty();
                let match_reasons = if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, &hit.qname) {
                    explain_match(&sym, &tokens, &ledger_entries, is_hot)
                } else {
                    vec![]
                };
                let bucket = result_bucket(&hit.file, &match_reasons, has_ledger, is_hot);
                let conf_reason = confidence_reason(&match_reasons, has_ledger, is_hot);
                // Check for an active useful feedback verdict (these survive filtering).
                let fb_store2 = AsgFeedbackStore { repo: &engine.repo };
                let fb_status = if let Ok(fb) = fb_store2.list_all(&engine.ref_name) {
                    let q = args.query.to_lowercase();
                    fb.iter().find(|e| {
                        e.symbol_id == hit.symbol_id
                            && (e.query.is_empty()
                                || q.contains(e.query.as_str())
                                || e.query.contains(q.as_str()))
                    }).map(|e| e.verdict.as_str().to_string())
                } else {
                    None
                };
                // feedback_rule: which specific verdict+query affected this result.
                let feedback_rule: Option<serde_json::Value> = if let Ok(fb) = fb_store2.list_all(&engine.ref_name) {
                    let impacts = explain_feedback_impacts(
                        &engine, &AsgIndexStore { repo: &engine.repo },
                        &args.query, &[hit.qname.clone()], &fb,
                    );
                    impacts.get(&hit.qname).map(|imp| serde_json::json!({
                        "verdict": imp.verdict,
                        "matched_query": imp.matched_query,
                        "author": imp.author,
                    }))
                } else {
                    None
                };
                // effect_detail: one-line reason for effect verification state.
                let effect_detail = {
                    let decl = effect_store.get_effects(&engine.ref_name, &hit.symbol_id)
                        .ok()
                        .flatten();
                    effect_detail_reason(decl.as_ref())
                };
                serde_json::json!({
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
                })
            }).collect();
            let scope_narrowed = !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
            let possible_misses = if scope_narrowed {
                vec![]
            } else {
                detect_possible_misses(&args.query, &layers_present, results.len())
            };
            // t-003: typed confidence warnings (ambiguous vs sparse).
            let confidence_warnings = detect_confidence_warnings(
                &tokens, results.len(), &ambiguous_terms, &cfg.db_path,
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
            let raw = serde_json::json!({
                "query": args.query,
                "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
                "ambiguous_terms": ambiguous_terms,
                "possible_misses": possible_misses,
                "confidence_warnings": confidence_warnings,
                "query_suggestions": query_suggestions,
                "scoped_suggestions": scoped_suggestions,
                "scope_narrowed": scope_narrowed,
                "feedback_suppressed": feedback_suppressed,
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

            // Load feedback for display badges (already applied to ranking above).
            let fb_for_display = {
                let fb_store = AsgFeedbackStore { repo: &engine.repo };
                fb_store.list_all(&engine.ref_name).unwrap_or_default()
            };
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
                let conf = confidences.get(idx).copied().unwrap_or(0.5);
                let (bucket, conf_reason_str, match_reasons_disp) = if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, &hit.qname) {
                    let entries = ledger_store.list_entries(&engine.ref_name, &sym.symbol_id).unwrap_or_default();
                    let has_ledger = !entries.is_empty();
                    let reasons = explain_match(&sym, &tokens, &entries, is_hot);
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
            }
            if feedback_suppressed > 0 {
                eprintln!("asd: {} result(s) suppressed by feedback (use `asd feedback list` to review)", feedback_suppressed);
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

    let index = AsgIndexStore { repo: &engine.repo };
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
    {
        let fb_store = AsgFeedbackStore { repo: &engine.repo };
        let idx = AsgIndexStore { repo: &engine.repo };
        if let Ok(fb) = fb_store.list_all(&engine.ref_name) {
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
                apply_feedback_adjustments(&engine, &idx, &args.query, &mut adj, &fb_tuples);
                apply_file_scope_feedback(&engine, &idx, &args.query, &mut adj, &fs_tuples);
                let surviving: std::collections::HashSet<String> =
                    adj.into_iter().map(|(_, q)| q).collect();
                scored.retain(|(_, sym)| surviving.contains(&sym.qname));
            }
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

