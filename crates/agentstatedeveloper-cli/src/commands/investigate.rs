//! `asd investigate <query>` — broad feature archaeology in one pass.
//!
//! 1. FTS5 hybrid search to find entry points (falls back to in-memory).
//! 2. For each top result: callers, callees, effects, invariants, hazards, notes.
//! 3. Prints a structured JSON report.

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, Engine, FtsFilters, IndexStore, LedgerStore,
    SearchFtsDb, classify_layer_sym, estimate_tokens, extract_summary, gather_recency, git_dirty_files,
    hybrid_boost,
    intent_focus, intent_layer_order, load_layer_overrides, parse_intent, stale_warning,
    symbol_tier, trim_for_agent,
};

use crate::commands::{
    context_for::assemble_symbol_context,
    graph::build_id_map,
    search::{in_memory_score, kind_str, query_tokens},
};
use crate::config::Config;

const ASD_PATH_PREFIX: &str = "/asd/v1";

#[derive(Debug, Args)]
pub struct InvestigateArgs {
    /// Natural-language or keyword query. Scored across symbol name,
    /// signature, doc comment, file path, and ledger entries.
    pub query: String,

    /// Number of top entry-point symbols to fully expand (default: 10).
    #[arg(long, default_value = "10")]
    pub depth: usize,

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
}

pub fn run(cfg: &Config, args: InvestigateArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref()
        .and_then(parse_intent)
        .unwrap_or("");
    if args.intent.is_some() && intent.is_empty() {
        eprintln!("asd: unknown intent {:?} — valid values: bugfix, feature, refactor, test, architecture, ui",
            args.intent.as_deref().unwrap_or(""));
    }
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let id_map = build_id_map(&engine);

    let tokens = query_tokens(&args.query);
    if tokens.is_empty() {
        println!("{}", json!({ "query": args.query, "entry_points": [] }));
        return Ok(());
    }

    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
    };

    // Each entry_point candidate: (combined_score, symbol_id, qname)
    // We resolve full Symbol via index_store for context assembly.
    // Returns (score, qname) pairs.
    let candidates: Vec<(f64, String)> = find_candidates(
        &engine,
        &cfg.db_path,
        &args.query,
        &tokens,
        &filters,
        &ledger_store,
        &index_store,
        args.depth,
    );

    // One git pass to gather recency for all candidate files (hot = 14 days).
    let recency = gather_recency(200, 14.0);

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
        let ctx = assemble_symbol_context(
            &engine,
            &index_store,
            &effect_store,
            &ledger_store,
            &sym,
            &id_map,
            args.include_body,
        )?;
        let mut ep = json!({
            "score": score,
            "layer": layer,
            "summary": summary,
            "last_touched_days": last_touched_days,
            "hot": hot,
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
        let qname = ep.get("symbol")
            .and_then(|s| s.get("qname"))
            .and_then(Value::as_str)
            .unwrap_or("");

        if let Some(invs) = ep.get("invariants").and_then(Value::as_array) {
            for inv in invs {
                let key = inv.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
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

    // --- Staleness warnings -----------------------------------------------
    let dirty = git_dirty_files();
    let stale_symbols: Vec<&str> = entry_points.iter()
        .filter_map(|ep| ep.get("symbol").and_then(|s| s.get("file")).and_then(Value::as_str))
        .filter(|f| dirty.contains(*f))
        .collect();

    // Default: grouped by_layer output (compact, deduped by layer).
    // --flat restores the legacy flat entry_points array.
    // Invariants/hazards surfaced first so agents see constraints before call graphs.
    let out = if args.flat {
        json!({
            "query": args.query,
            "intent": if intent.is_empty() { Value::Null } else { Value::String(intent.to_string()) },
            "focus": if focus.is_empty() { Value::Null } else { Value::String(focus.to_string()) },
            "tokens": tokens,
            "stale_symbols": stale_symbols,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "entry_points": entry_points,
        })
    } else {
        json!({
            "query": args.query,
            "intent": if intent.is_empty() { Value::Null } else { Value::String(intent.to_string()) },
            "focus": if focus.is_empty() { Value::Null } else { Value::String(focus.to_string()) },
            "tokens": tokens,
            "stale_symbols": stale_symbols,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "by_layer": by_layer,
        })
    };
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

/// Returns top-`depth` (score, qname) pairs using FTS when available,
/// falling back to in-memory scoring.
pub(crate) fn find_candidates(
    engine: &Engine,
    db_path: &std::path::Path,
    query: &str,
    tokens: &[String],
    filters: &FtsFilters,
    ledger_store: &AsgLedgerStore,
    index_store: &AsgIndexStore,
    depth: usize,
) -> Vec<(f64, String)> {
    // --- FTS path ---
    let fts_result = SearchFtsDb::open(db_path)
        .ok()
        .filter(|fts| fts.has_data())
        .and_then(|fts| fts.search(query, filters, depth * 8).ok());

    if let Some(hits) = fts_result {
        let mut scored: Vec<(f64, String)> = hits
            .into_iter()
            .map(|hit| {
                let boost = hybrid_boost(&hit, tokens);
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
                (hit.bm25_score + boost + ledger_boost, hit.qname)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // File-stem injection: for each query token, find files whose stem
        // contains that token and ensure they appear in the final top-depth
        // results.  covered_files only protects the files already in the TOP
        // `depth` slots — files ranked depth+1..depth*8 in FTS are still
        // eligible for re-injection so a strong stem boost can displace a
        // weak FTS match.
        let covered_files: std::collections::HashSet<String> = scored
            .iter()
            .take(depth)
            .filter_map(|(_, qname)| {
                index_store.get_symbol_by_qname(&engine.ref_name, qname)
                    .ok()
                    .flatten()
                    .map(|s| s.file)
            })
            .collect();

        if let Ok(fts) = SearchFtsDb::open(db_path) {
            for token in tokens {
                if let Ok(stem_hits) = fts.file_stem_candidates(token, filters, depth * 2) {
                    for hit in stem_hits {
                        if !covered_files.contains(&hit.file) {
                            let boost = hybrid_boost(&hit, tokens);
                            scored.push((1.0 + boost, hit.qname));
                        }
                    }
                }
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.dedup_by(|a, b| a.1 == b.1);
        // File-level dedup: one slot per file.  Symbols with ledger entries
        // (invariants, hazards, decisions) are promoted above same-file
        // competitors that only have a score advantage, so an invariant-bearing
        // method isn't silently dropped in favour of a higher-scoring sibling.
        let has_ledger: std::collections::HashSet<String> = scored
            .iter()
            .filter_map(|(_, qname)| {
                let sym = index_store.get_symbol_by_qname(&engine.ref_name, qname)
                    .ok().flatten()?;
                let entries = ledger_store
                    .list_entries(&engine.ref_name, &sym.symbol_id)
                    .unwrap_or_default();
                if entries.is_empty() { None } else { Some(qname.clone()) }
            })
            .collect();
        scored.sort_by(|a, b| {
            let a_ledger = has_ledger.contains(&a.1);
            let b_ledger = has_ledger.contains(&b.1);
            b_ledger.cmp(&a_ledger)
                .then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        scored.retain(|(_, qname)| {
            match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
                Ok(Some(sym)) => seen_files.insert(sym.file),
                _ => true,
            }
        });
        // Restore score order for the final result.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(depth);
        return scored;
    }

    // --- Fallback: in-memory O(N) scoring ---
    eprintln!("asd: FTS index not populated — falling back to in-memory search");

    let kind_filter = filters.kind.as_deref().map(|k| k.to_lowercase());
    let lang_filter = filters.language.as_deref();

    let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
    let qnames: Vec<String> = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => vec![],
    };

    let mut scored: Vec<(f64, String)> = Vec::new();
    for qname in &qnames {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        if let Some(ref k) = kind_filter {
            if kind_str(&sym.kind) != k.as_str() { continue; }
        }
        if let Some(lang) = lang_filter {
            if sym.language != lang { continue; }
        }
        let s = in_memory_score(&sym, tokens, ledger_store, engine);
        if s > 0 {
            scored.push((s as f64, sym.qname));
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(depth);
    scored
}
