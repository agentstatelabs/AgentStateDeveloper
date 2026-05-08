//! `asd search <query>` — ranked concept search over indexed symbols.
//!
//! Primary path: BM25 via FTS5 table populated at `asd index` time.
//! Hybrid reranking: FTS BM25 score + ledger-text token boost.
//! Fallback: in-memory O(N) scoring when FTS table is empty or absent.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{
    AGENT_DEFAULT_BUDGET, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, Engine, FeedbackStore,
    FeedbackVerdict, FtsFilters, IndexStore, LedgerStore, SearchDocsDb, SearchFtsDb,
    apply_feedback_adjustments, classify_layer_sym, confidence_scores, detect_ambiguous_tokens,
    detect_possible_misses, estimate_tokens, explain_match, extract_summary, gather_recency,
    hybrid_boost, in_memory_score, intent_focus, kind_str, load_layer_overrides, parse_intent,
    parse_query, resolve_scope, result_bucket, stale_warning, symbol_tier, trim_for_agent,
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

        // Hybrid reranking: BM25 + path/name boost + ledger boost.
        let mut scored: Vec<(f64, _)> = hits
            .into_iter()
            .map(|hit| {
                let boost = hybrid_boost(&hit, &tokens);
                let ledger_boost = {
                    let entries = ledger_store
                        .list_entries(&engine.ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let text = entries
                        .iter()
                        .map(|e| e.summary.to_lowercase())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if text.is_empty() {
                        0.0
                    } else {
                        tokens.iter().filter(|t| text.contains(t.as_str())).count() as f64
                    }
                };
                (hit.bm25_score + boost + ledger_boost, hit)
            })
            .collect();

        // Apply durable feedback: suppress noisy/wrong-layer, boost useful.
        {
            let fb_store = AsgFeedbackStore { repo: &engine.repo };
            let idx = AsgIndexStore { repo: &engine.repo };
            if let Ok(fb) = fb_store.list_all(&engine.ref_name) {
                if !fb.is_empty() {
                    let fb_tuples: Vec<_> = fb.iter()
                        .map(|e| (e.symbol_id.clone(), e.query.clone(), e.verdict))
                        .collect();
                    let mut adj: Vec<(f64, String)> = scored.iter()
                        .map(|(s, h)| (*s, h.qname.clone()))
                        .collect();
                    apply_feedback_adjustments(&engine, &idx, &args.query, &mut adj, &fb_tuples);
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
                serde_json::json!({
                    "score": score, "confidence": conf, "bucket": bucket,
                    "qname": hit.qname, "kind": hit.kind,
                    "file": hit.file, "line": hit.line, "layer": layer,
                    "summary": extract_summary(hit.doc.as_deref(), hit.signature.as_deref()),
                    "last_touched_days": rec.and_then(|r| r.last_touched_days),
                    "hot": is_hot,
                    "match_reasons": match_reasons,
                    "feedback_status": fb_status,
                })
            }).collect();
            let scope_narrowed = !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
            let possible_misses = if scope_narrowed {
                vec![]
            } else {
                detect_possible_misses(&args.query, &layers_present, results.len())
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
                "scope_narrowed": scope_narrowed,
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
                println!(
                    "[{:.1}] {} {}{}{}{} ({}:{})",
                    score, hit.kind, hit.qname, hot_tag, fb_tag, age_tag, hit.file, hit.line
                );
                if let Some(sig) = &hit.signature {
                    if !sig.is_empty() { println!("       sig: {}", sig); }
                }
                let summary = extract_summary(hit.doc.as_deref(), hit.signature.as_deref());
                if !summary.is_empty() { println!("       {}", summary); }
                if args.explain {
                    let entries = ledger_store
                        .list_entries(&engine.ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let has_ledger = !entries.is_empty();
                    let reasons = index_store
                        .get_symbol_by_qname(&engine.ref_name, &hit.qname)
                        .ok().flatten()
                        .map(|sym| explain_match(&sym, &tokens, &entries, is_hot))
                        .unwrap_or_default();
                    let conf = confidences.get(idx).copied().unwrap_or(0.0);
                    let bucket = result_bucket(&hit.file, &reasons, has_ledger, is_hot);
                    println!(
                        "       confidence: {:.0}%  bucket: {}",
                        conf * 100.0,
                        bucket
                    );
                    if !reasons.is_empty() {
                        println!("       why: {}", reasons.join(", "));
                    }
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
                    .map(|e| (e.symbol_id.clone(), e.query.clone(), e.verdict))
                    .collect();
                let mut adj: Vec<(f64, String)> = scored.iter()
                    .map(|(s, sym)| (*s as f64, sym.qname.clone()))
                    .collect();
                apply_feedback_adjustments(&engine, &idx, &args.query, &mut adj, &fb_tuples);
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

