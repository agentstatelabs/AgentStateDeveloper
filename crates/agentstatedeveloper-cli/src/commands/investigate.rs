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
    SearchFtsDb,
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

    /// Number of top entry-point symbols to fully expand (default: 5).
    #[arg(long, default_value = "5")]
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
}

pub fn run(cfg: &Config, args: InvestigateArgs) -> Result<()> {
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

    let mut entry_points: Vec<Value> = Vec::new();
    for (score, qname) in &candidates {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let ctx = assemble_symbol_context(
            &engine,
            &index_store,
            &effect_store,
            &ledger_store,
            &sym,
            &id_map,
            args.include_body,
        )?;
        let mut ep = json!({ "score": score });
        if let (Some(obj), Some(ctx_obj)) = (ep.as_object_mut(), ctx.as_object()) {
            for (k, v) in ctx_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        entry_points.push(ep);
    }

    let out = json!({
        "query": args.query,
        "tokens": tokens,
        "entry_points": entry_points,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Returns top-`depth` (score, symbol_id) pairs using FTS when available,
/// falling back to in-memory scoring.
fn find_candidates(
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
        .and_then(|fts| fts.search(query, filters, depth * 4).ok());

    if let Some(hits) = fts_result {
        let mut scored: Vec<(f64, String)> = hits
            .into_iter()
            .map(|hit| {
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
                (hit.bm25_score + ledger_boost, hit.qname)
            })
            .collect();
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
