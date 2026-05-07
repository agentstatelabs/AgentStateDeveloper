//! `asd search <query>` — ranked concept search over indexed symbols.
//!
//! Primary path: BM25 via FTS5 table populated at `asd index` time.
//! Hybrid reranking: FTS BM25 score + ledger-text token boost.
//! Fallback: in-memory O(N) scoring when FTS table is empty or absent.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{
    AGENT_DEFAULT_BUDGET, AsgIndexStore, AsgLedgerStore, Engine, FtsFilters, IndexStore,
    LedgerStore, SearchFtsDb, SymbolKind, classify_layer_sym, estimate_tokens, explain_match,
    extract_summary, gather_recency, hybrid_boost, in_memory_score, intent_focus, kind_str,
    load_layer_overrides, parse_intent, parse_query, resolve_scope, stale_warning, symbol_tier,
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

    // --- FTS path ---
    let fts_result = SearchFtsDb::open(&cfg.db_path)
        .ok()
        .filter(|fts| fts.has_data())
        .and_then(|fts| fts.search(&args.query, &filters, args.limit * 4).ok());

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
        if args.agent {
            let results: Vec<serde_json::Value> = scored.iter().map(|(score, hit)| {
                let rec = recency.get(&hit.file);
                let tier = symbol_tier(&hit.file);
                let layer = classify_layer_sym(&hit.file, &hit.qname, tier, &layer_overrides);
                let ledger_entries = ledger_store
                    .list_entries(&engine.ref_name, &hit.symbol_id)
                    .unwrap_or_default();
                let match_reasons = if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, &hit.qname) {
                    explain_match(&sym, &tokens, &ledger_entries)
                } else {
                    vec![]
                };
                serde_json::json!({
                    "score": score, "qname": hit.qname, "kind": hit.kind,
                    "file": hit.file, "line": hit.line, "layer": layer,
                    "summary": extract_summary(hit.doc.as_deref(), hit.signature.as_deref()),
                    "last_touched_days": rec.and_then(|r| r.last_touched_days),
                    "hot": rec.map(|r| r.hot).unwrap_or(false),
                    "match_reasons": match_reasons,
                })
            }).collect();
            let raw = serde_json::json!({
                "query": args.query,
                "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
                "results": results,
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
            for (score, hit) in &scored {
                let rec = recency.get(&hit.file);
                let hot_tag = if rec.map(|r| r.hot).unwrap_or(false) { " [hot]" } else { "" };
                let age_tag = rec
                    .and_then(|r| r.last_touched_days)
                    .map(|d| format!(" ~{:.0}d ago", d))
                    .unwrap_or_default();
                println!(
                    "[{:.1}] {} {}{}{} ({}:{})",
                    score, hit.kind, hit.qname, hot_tag, age_tag, hit.file, hit.line
                );
                if let Some(sig) = &hit.signature {
                    if !sig.is_empty() { println!("       sig: {}", sig); }
                }
                let summary = extract_summary(hit.doc.as_deref(), hit.signature.as_deref());
                if !summary.is_empty() { println!("       {}", summary); }
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
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

