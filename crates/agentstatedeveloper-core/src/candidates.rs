//! Shared candidate-selection logic used by CLI commands and the MCP server.
//!
//! `find_candidates` is the single entry point: it runs FTS5 hybrid search,
//! injects file-stem matches, applies ledger-aware file deduplication (symbols
//! with ledger entries win their file slot), falls back to an in-memory
//! O(N) scorer when the FTS index is not populated, and finishes with a
//! ledger-anchor pass that unconditionally injects invariant/hazard-bearing
//! symbols whose summaries match any query token.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::{LedgerEntry, LedgerKind, Symbol, SymbolKind};
use crate::search_fts::{FtsFilters, SearchFtsDb, hybrid_boost, is_stopword};

// ---------------------------------------------------------------------------
// Query tokenisation
// ---------------------------------------------------------------------------

/// Tokenise a free-form query into lowercase, stopword-filtered terms.
pub fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2 && !is_stopword(t))
        .collect()
}

// ---------------------------------------------------------------------------
// Kind helpers
// ---------------------------------------------------------------------------

pub fn kind_str(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "module",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Variable => "variable",
    }
}

// ---------------------------------------------------------------------------
// In-memory fallback scorer
// ---------------------------------------------------------------------------

/// Score a symbol against query tokens using simple substring matching.
/// Used when the FTS index is not populated.
pub fn in_memory_score(
    sym: &crate::Symbol,
    tokens: &[String],
    ledger_store: &AsgLedgerStore,
    engine: &Engine,
) -> u32 {
    let qname_lower = sym.qname.to_lowercase();
    let sig_lower = sym.signature.as_deref().unwrap_or("").to_lowercase();
    let doc_lower = sym.doc.as_deref().unwrap_or("").to_lowercase();
    let file_lower = sym.file.to_lowercase();

    let ledger_text: String = ledger_store
        .list_entries(&engine.ref_name, &sym.symbol_id)
        .unwrap_or_default()
        .iter()
        .map(|e| e.summary.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");

    let mut score: u32 = 0;
    for token in tokens {
        if qname_lower.contains(token.as_str()) { score += 4; }
        if !sig_lower.is_empty() && sig_lower.contains(token.as_str()) { score += 3; }
        if !doc_lower.is_empty() && doc_lower.contains(token.as_str()) { score += 3; }
        if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) { score += 2; }
        if file_lower.contains(token.as_str()) { score += 1; }
    }
    score
}

// ---------------------------------------------------------------------------
// Ledger-anchor pass
// ---------------------------------------------------------------------------

/// Score assigned to anchored symbols — above zero so they appear in results,
/// but below any genuine FTS hit so they don't displace real matches.
const ANCHOR_SCORE: f64 = 0.5;

/// Maximum number of extra symbols that can be injected by the anchor pass.
const MAX_ANCHORS: usize = 5;

/// After candidate selection, scan every ledger entry whose summary contains
/// a query token and inject the bearing symbol if it is not already present.
///
/// Only Invariant and Hazard entries trigger anchoring — Decisions and Notes
/// are informational and don't imply the symbol must surface for the query.
///
/// Cost: one `get_tree` call over the ledger tree (small dataset) + one
/// `get_tree` call over the qname index to build the id→qname reverse map.
/// Both are cached Git-object reads.
fn ledger_anchor_pass(
    engine: &Engine,
    tokens: &[String],
    candidates: &mut Vec<(f64, String)>,
) {
    if tokens.is_empty() { return; }

    // Walk the entire ledger tree looking for token-matching entries.
    let ledger_prefix = format!("{}/ledger", crate::paths::ASD_ROOT);
    let ledger_tree = match engine.repo.get_tree(&engine.ref_name, &ledger_prefix) {
        Ok(v) => v,
        Err(_) => return,
    };

    let mut matching_sym_ids: Vec<String> = Vec::new();
    if let serde_json::Value::Object(by_symbol) = ledger_tree {
        for (sym_id, per_symbol) in by_symbol {
            if let serde_json::Value::Object(entries_map) = per_symbol {
                for (_entry_id, v) in entries_map {
                    if let Ok(entry) = serde_json::from_value::<LedgerEntry>(v) {
                        if !matches!(entry.kind, LedgerKind::Invariant | LedgerKind::Hazard) {
                            continue;
                        }
                        let summary_lower = entry.summary.to_lowercase();
                        if tokens.iter().any(|t| summary_lower.contains(t.as_str())) {
                            matching_sym_ids.push(sym_id.clone());
                            break; // one match per symbol is enough
                        }
                    }
                }
            }
        }
    }

    if matching_sym_ids.is_empty() { return; }

    // Build symbol_id → Symbol reverse map from the qname index.
    let qname_prefix = format!("{}/index/by-qname", crate::paths::ASD_ROOT);
    let id_map: HashMap<String, Symbol> =
        match engine.repo.get_tree(&engine.ref_name, &qname_prefix) {
            Ok(serde_json::Value::Object(map)) => map
                .values()
                .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                .map(|s| (s.symbol_id.clone(), s))
                .collect(),
            _ => HashMap::new(),
        };

    let existing_qnames: HashSet<String> = candidates.iter().map(|(_, q)| q.clone()).collect();
    let mut anchors = 0usize;

    for sym_id in matching_sym_ids {
        if anchors >= MAX_ANCHORS { break; }
        if let Some(sym) = id_map.get(&sym_id) {
            if !existing_qnames.contains(&sym.qname) {
                candidates.push((ANCHOR_SCORE, sym.qname.clone()));
                anchors += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Candidate selection
// ---------------------------------------------------------------------------

const ASD_PATH_PREFIX: &str = "/asd/v1";

/// Return the top-`depth` `(score, qname)` pairs for a query.
///
/// Strategy:
/// 1. FTS5 hybrid search with BM25 + ledger boost.
/// 2. File-stem injection: for each query token, find files whose stem
///    contains the token and inject a representative symbol for any file
///    not already in the top-`depth` FTS results.
/// 3. Ledger-aware file dedup: per-file, prefer the symbol that has ledger
///    entries (invariant/hazard/decision) over a higher-scoring sibling
///    that does not.  Invariant-bearing symbols must not be silently dropped.
/// 4. Falls back to in-memory O(N) scoring when FTS is unavailable.
pub fn find_candidates(
    engine: &Engine,
    db_path: &Path,
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

        // File-stem injection: only protect files already in the top `depth`
        // slots — files ranked depth+1..depth*8 in FTS are still eligible for
        // re-injection so a strong stem boost can displace a weak FTS match.
        let covered_files: HashSet<String> = scored
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

        // Ledger-aware file dedup: promote symbols with ledger entries above
        // same-file competitors that only have a score advantage, so an
        // invariant-bearing method is never silently dropped.
        let has_ledger: HashSet<String> = scored
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
        let mut seen_files: HashSet<String> = HashSet::new();
        scored.retain(|(_, qname)| {
            match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
                Ok(Some(sym)) => seen_files.insert(sym.file),
                _ => true,
            }
        });
        // Restore score order for the final result.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(depth);

        // Ledger-anchor pass: inject invariant/hazard-bearing symbols that
        // matched query tokens but were dropped by dedup or FTS ranking.
        ledger_anchor_pass(engine, tokens, &mut scored);

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
    ledger_anchor_pass(engine, tokens, &mut scored);
    scored
}
