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
use std::path::PathBuf;

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::{LedgerEntry, LedgerKind, Symbol, SymbolKind};
use crate::search_fts::{FtsFilters, SearchFtsDb, classify_layer_sym, hybrid_boost, is_stopword};

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

/// Parse a query string into `(positive_tokens, exclusion_terms)`.
///
/// Words prefixed with `-` (e.g. `-sample`) are treated as exclusions.
/// The rest are tokenised with the same rules as `query_tokens`.
///
/// Example: `"drift playhead -sample -waveform"` →
///   tokens: `["drift", "playhead"]`, exclusions: `["sample", "waveform"]`
pub fn parse_query(query: &str) -> (Vec<String>, Vec<String>) {
    let mut positive_words: Vec<&str> = Vec::new();
    let mut exclusions: Vec<String> = Vec::new();

    for word in query.split_whitespace() {
        if let Some(excl) = word.strip_prefix('-') {
            let term = excl.to_lowercase();
            if term.len() >= 2 {
                exclusions.push(term);
            }
        } else {
            positive_words.push(word);
        }
    }

    let tokens = positive_words
        .join(" ")
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2 && !is_stopword(t))
        .collect();

    (tokens, exclusions)
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
// Glob path matching
// ---------------------------------------------------------------------------

/// Match a file path against a glob pattern.
///
/// Supports:
///   `**`  — matches zero or more path segments
///   `*`   — matches any characters within a single segment (no `/`)
///   all other characters match literally (case-sensitive)
pub fn glob_match(pattern: &str, path: &str) -> bool {
    glob_match_parts(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

fn glob_match_parts(pat: &[&str], path: &[&str]) -> bool {
    match (pat.first(), path.first()) {
        (None, None) => true,
        (None, _) | (_, None) => {
            // Pattern exhausted with path remaining, or path exhausted but
            // pattern has non-** remaining — check if remaining pattern is all **
            pat.is_empty() && path.is_empty()
                || pat.iter().all(|p| *p == "**") && path.is_empty()
                || pat.is_empty() && path.is_empty()
        }
        (Some(&"**"), _) => {
            // ** matches zero segments (skip it) or one segment (consume path head)
            glob_match_parts(&pat[1..], path)
                || glob_match_parts(pat, &path[1..])
        }
        (Some(p), Some(s)) => {
            segment_match(p, s) && glob_match_parts(&pat[1..], &path[1..])
        }
    }
}

/// Match a single path segment against a pattern segment (supports `*`).
fn segment_match(pat: &str, seg: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == seg;
    }
    let mut remaining = seg;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !remaining.starts_with(part) { return false; }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            if !remaining.ends_with(part) { return false; }
        } else {
            match remaining.find(part) {
                Some(pos) => remaining = &remaining[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// Return true if `file` matches any of the given glob patterns.
pub fn matches_any_path_glob(globs: &[String], file: &str) -> bool {
    globs.iter().any(|g| glob_match(g.as_str(), file))
}

// ---------------------------------------------------------------------------
// Scope alias resolution
// ---------------------------------------------------------------------------

/// Load named scope aliases from `.asd/scopes.toml` in the current directory.
///
/// Format:
/// ```toml
/// drift-pad = ["App/**/DriftPad*", "Packages/SequencerCore/**"]
/// sequencer-core = ["Packages/SequencerCore/**"]
/// ```
///
/// Returns an empty map if the file is absent or unparseable.
pub fn load_scope_aliases(db_path: &Path) -> HashMap<String, Vec<String>> {
    let scopes_path = db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("scopes.toml");
    let contents = match std::fs::read_to_string(&scopes_path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    let table: toml::Table = match toml::from_str(&contents) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    table.into_iter().filter_map(|(k, v)| {
        let globs = match v {
            toml::Value::Array(arr) => arr.into_iter().filter_map(|v| {
                if let toml::Value::String(s) = v { Some(s) } else { None }
            }).collect(),
            toml::Value::String(s) => vec![s],
            _ => return None,
        };
        Some((k, globs))
    }).collect()
}

/// Resolve a `--scope` name to path globs.
/// Falls back to treating the scope name itself as a path glob if not in the map.
pub fn resolve_scope(scope: &str, db_path: &Path) -> Vec<String> {
    let aliases = load_scope_aliases(db_path);
    aliases.get(scope).cloned().unwrap_or_else(|| vec![scope.to_string()])
}

// ---------------------------------------------------------------------------
// Path filter
// ---------------------------------------------------------------------------

/// Retain only candidates whose file matches at least one of the path globs.
/// No-op when `paths_filter` is empty.
fn apply_paths_filter(
    engine: &Engine,
    index_store: &AsgIndexStore,
    paths_filter: &[String],
    scored: &mut Vec<(f64, String)>,
) {
    if paths_filter.is_empty() { return; }
    scored.retain(|(_, qname)| {
        match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(sym)) => matches_any_path_glob(paths_filter, &sym.file),
            _ => true,
        }
    });
}

// ---------------------------------------------------------------------------
// Exclusion filter
// ---------------------------------------------------------------------------

/// Remove candidates that match any exclusion term (case-insensitive substring)
/// in their qname, file path, doc comment, or signature.
fn apply_exclusions(
    engine: &Engine,
    index_store: &AsgIndexStore,
    exclude_terms: &[String],
    scored: &mut Vec<(f64, String)>,
) {
    if exclude_terms.is_empty() { return; }
    scored.retain(|(_, qname)| {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => return true,
        };
        let qname_lower = qname.to_lowercase();
        let file_lower = sym.file.to_lowercase();
        let doc_lower = sym.doc.as_deref().unwrap_or("").to_lowercase();
        let sig_lower = sym.signature.as_deref().unwrap_or("").to_lowercase();
        !exclude_terms.iter().any(|excl| {
            qname_lower.contains(excl.as_str())
                || file_lower.contains(excl.as_str())
                || doc_lower.contains(excl.as_str())
                || sig_lower.contains(excl.as_str())
        })
    });
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
    // Fetch depth*2 candidates (was depth*8). The overfetch factor is needed so
    // ledger/SOT boosts can reorder candidates after BM25 ranking; *2 keeps
    // accuracy well enough for the golden probe suite while cutting ledger-read
    // count to 25% of the original. Bump to *3/*4/*8 if ranking regressions appear.
    let fts_result = SearchFtsDb::open(db_path)
        .ok()
        .filter(|fts| fts.has_data())
        .and_then(|fts| fts.search(query, filters, depth * 2).ok());

    if let Some(hits) = fts_result {
        // Ledger-entry cache: read once per symbol during scoring, reuse in
        // covered_files check below. Eliminates 2× list_entries per top-N hit.
        let ledger_cache: HashMap<String, Vec<crate::schema::LedgerEntry>> =
            HashMap::with_capacity(hits.len());
        // Also keep qname → (symbol_id, file) to avoid get_symbol_by_qname in covered_files.
        let mut qname_to_sym: HashMap<String, (String, String)> =
            HashMap::with_capacity(hits.len());

        // M59: track which symbol_ids have ledger entries using the denormalized
        // FTS fields — no list_entries call needed in the scoring loop.
        let mut has_ledger_ids: HashSet<String> = HashSet::new();

        let mut scored: Vec<(f64, String)> = {
            let mut tmp = Vec::with_capacity(hits.len());
            for hit in hits {
                let boost = hybrid_boost(&hit, tokens);
                // M59: use denormalized ledger_text/flags from FTS row — no list_entries call.
                // ledger_text = all summaries concatenated (lowercase).
                // ledger_flags = comma-separated kinds: "ownership,invariant,hazard,decision".
                let (ledger_boost, ownership_struct_boost) = {
                    let sot = if hit.has_ownership() { 2.0_f64 } else { 0.0 };
                    // Count token hits in the concatenated ledger text and weight by kind.
                    let text_boost = if hit.ledger_text.is_empty() {
                        0.0
                    } else {
                        let matches = tokens.iter()
                            .filter(|t| hit.ledger_text.contains(t.as_str()))
                            .count() as f64;
                        if matches == 0.0 {
                            0.0
                        } else {
                            // Weight by highest-priority kind flagged for this symbol.
                            let weight = if hit.has_ownership()  { 3.0 }
                                    else if hit.has_invariant() { 1.5 }
                                    else if hit.has_hazard()    { 1.0 }
                                    else                        { 0.5 };
                            matches * weight
                        }
                    };
                    if hit.has_ledger() {
                        has_ledger_ids.insert(hit.symbol_id.clone());
                    }
                    (text_boost, sot)
                };
                qname_to_sym.insert(hit.qname.clone(), (hit.symbol_id.clone(), hit.file.clone()));
                tmp.push((hit.bm25_score + boost + ledger_boost + ownership_struct_boost, hit.qname));
            }
            tmp
        };
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // File-stem injection: a file is "covered" only when the claiming
        // symbol in the top-`depth` results itself has ledger entries.  If a
        // non-ledger symbol holds the slot, the file stays open so a
        // ledger-bearing sibling can be injected via stem.  Files ranked
        // depth+1..depth*4 are always eligible for re-injection.
        // Uses cached ledger data and pre-built qname→sym map — no extra git reads.
        let covered_files: HashSet<String> = scored
            .iter()
            .take(depth)
            .filter_map(|(_, qname)| {
                let (sym_id, file) = qname_to_sym.get(qname)?;
                // M59: check has_ledger_ids (populated from FTS denormalized fields) first;
                // fall back to ledger_cache for stem-injected hits added below.
                let has_led = has_ledger_ids.contains(sym_id)
                    || ledger_cache.get(sym_id).map_or(false, |e| !e.is_empty());
                if has_led { Some(file.clone()) } else { None }
            })
            .collect();

        const VIEW_STEM_HINTS: &[&str] = &[
            "view", "ui", "render", "display", "screen", "layout", "widget", "cell", "button", "pad",
        ];
        let query_lower = query.to_lowercase();
        let is_view_query = VIEW_STEM_HINTS.iter().any(|h| query_lower.contains(h));
        if let Ok(fts) = SearchFtsDb::open(db_path) {
            for token in tokens {
                if let Ok(stem_hits) = fts.file_stem_candidates(token, filters, depth * 2) {
                    for hit in stem_hits {
                        if !covered_files.contains(&hit.file) {
                            let boost = hybrid_boost(&hit, tokens);
                            let tier = hit.tier;
                            let layer = classify_layer_sym(&hit.file, &hit.qname, tier, &[]);
                            let view_boost = if is_view_query
                                && (layer == "ui" || layer == "viewmodel")
                            { 2.0 } else { 0.0 };
                            // Extend maps so the has_ledger + file-dedup passes below
                            // can use cache lookups instead of get_symbol_by_qname reads.
                            qname_to_sym.entry(hit.qname.clone())
                                .or_insert_with(|| (hit.symbol_id.clone(), hit.file.clone()));
                            // M59: propagate ledger presence from stem hit into has_ledger_ids.
                            if hit.has_ledger() {
                                has_ledger_ids.insert(hit.symbol_id.clone());
                            }
                            scored.push((1.0 + boost + view_boost, hit.qname));
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
        //
        // Uses the ledger_cache and qname_to_sym maps built during the scoring
        // and stem-injection passes — no extra git/index reads needed here.
        let has_ledger: HashSet<String> = scored
            .iter()
            .filter_map(|(_, qname)| {
                let (sym_id, _) = qname_to_sym.get(qname.as_str())?;
                // M59: check has_ledger_ids first (no git read), then ledger_cache fallback.
                let has = has_ledger_ids.contains(sym_id)
                    || ledger_cache.get(sym_id).map_or(false, |e| !e.is_empty());
                if has { Some(qname.clone()) } else { None }
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
            match qname_to_sym.get(qname.as_str()) {
                Some((_, file)) => seen_files.insert(file.clone()),
                None => true, // unknown qname (should not happen) — keep it
            }
        });
        // Restore score order for the final result.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(depth);

        apply_paths_filter(engine, index_store, &filters.paths_filter, &mut scored);
        apply_exclusions(engine, index_store, &filters.exclude_terms, &mut scored);

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
    apply_paths_filter(engine, index_store, &filters.paths_filter, &mut scored);
    apply_exclusions(engine, index_store, &filters.exclude_terms, &mut scored);
    ledger_anchor_pass(engine, tokens, &mut scored);
    scored
}

/// Explain why a symbol was returned for a query.
///
/// Returns short reason strings like `"name:playhead"`, `"doc:transport"`,
/// `"ledger:2 invariants"`, `"recent-edit"`. Useful as `match_reasons` in
/// agent output so the caller can understand which signal drove ranking.
pub fn explain_match(
    sym: &Symbol,
    tokens: &[String],
    ledger_entries: &[LedgerEntry],
    is_hot: bool,
) -> Vec<String> {
    let qname_lower = sym.qname.to_lowercase();
    let file_lower = sym.file.to_lowercase();
    let sig_lower = sym.signature.as_deref().unwrap_or("").to_lowercase();
    let doc_lower = sym.doc.as_deref().unwrap_or("").to_lowercase();

    let mut reasons: Vec<String> = Vec::new();
    for token in tokens {
        let t = token.as_str();
        if qname_lower.contains(t) {
            reasons.push(format!("name:{}", token));
        } else if file_lower.contains(t) {
            reasons.push(format!("file:{}", token));
        } else if sig_lower.contains(t) {
            reasons.push(format!("sig:{}", token));
        } else if doc_lower.contains(t) {
            reasons.push(format!("doc:{}", token));
        }
    }

    let inv_count = ledger_entries.iter().filter(|e| matches!(e.kind, LedgerKind::Invariant)).count();
    let haz_count = ledger_entries.iter().filter(|e| matches!(e.kind, LedgerKind::Hazard)).count();
    if inv_count > 0 {
        reasons.push(format!("invariant-attached:{}", inv_count));
    }
    if haz_count > 0 {
        reasons.push(format!("ledger:{} hazard{}", haz_count, if haz_count == 1 { "" } else { "s" }));
    }
    // Ownership-boundary: this symbol is explicitly declared as the source-of-truth
    // for a domain concept that overlaps the query.
    let owns: Vec<&str> = ledger_entries.iter()
        .filter(|e| e.kind == LedgerKind::Ownership)
        .map(|e| e.summary.as_str())
        .filter(|s| tokens.iter().any(|t| s.to_lowercase().contains(t.as_str())))
        .collect();
    if !owns.is_empty() {
        reasons.push(format!("ownership:{}", owns[0].split_whitespace().take(4).collect::<Vec<_>>().join("-")));
    }
    if is_hot {
        reasons.push("recent-edit".to_string());
    }

    reasons
}

// ---------------------------------------------------------------------------
// Uncertainty model helpers
// ---------------------------------------------------------------------------

/// Normalize raw scores to [0.1, 1.0] confidence within the result set.
/// The highest-scoring result gets 1.0; the lowest gets 0.1.
pub fn confidence_scores(scores: &[f64]) -> Vec<f64> {
    if scores.is_empty() { return vec![]; }
    if scores.len() == 1 { return vec![1.0]; }
    let max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let range = max - min;
    if range < 1e-9 { return vec![1.0; scores.len()]; }
    scores.iter().map(|&s| 0.1 + 0.9 * (s - min) / range).collect()
}

/// Classify a result into a semantic bucket.
///
/// - `core`       — has ledger entries AND high-signal name match or hot file
/// - `relevant`   — has ledger OR name match (previously "supporting", signal-bearing)
/// - `peripheral` — only doc or file-path match; no ledger, name, or recent edit
/// - `noisy`      — no match signal at all
/// - `test-only`  — symbol lives in a test file
pub fn result_bucket(
    file: &str,
    match_reasons: &[String],
    has_ledger: bool,
    is_hot: bool,
) -> &'static str {
    use crate::search_fts::symbol_tier;
    if symbol_tier(file) == 2 { return "test-only"; }
    let has_name = match_reasons.iter().any(|r| r.starts_with("name:"));
    let has_doc_or_file = match_reasons.iter().any(|r| r.starts_with("doc:") || r.starts_with("file:"));
    if has_ledger && (has_name || is_hot) { return "core"; }
    if has_ledger || has_name { return "relevant"; }
    if is_hot || has_doc_or_file { return "peripheral"; }
    "noisy"
}

/// One-line explanation of why a result has high or low confidence.
///
/// Used in `--explain` output and agent JSON to replace the bare bucket label.
pub fn confidence_reason(match_reasons: &[String], has_ledger: bool, is_hot: bool) -> String {
    let has_name = match_reasons.iter().any(|r| r.starts_with("name:"));
    let has_inv  = match_reasons.iter().any(|r| r.starts_with("invariant-attached:"));
    let has_own  = match_reasons.iter().any(|r| r.starts_with("ownership:"));
    let has_haz  = match_reasons.iter().any(|r| r.starts_with("ledger:"));
    let has_sig  = match_reasons.iter().any(|r| r.starts_with("sig:"));
    let has_doc  = match_reasons.iter().any(|r| r.starts_with("doc:"));
    let has_file = match_reasons.iter().any(|r| r.starts_with("file:"));

    if has_name && has_inv {
        "strong: name match + invariant-attached".to_string()
    } else if has_name && has_own {
        "strong: name match + ownership declaration".to_string()
    } else if has_name && has_haz {
        "strong: name match + hazard ledger entry".to_string()
    } else if has_name && has_ledger {
        "strong: name match + ledger entry".to_string()
    } else if has_name && is_hot {
        "moderate: name match + recently edited".to_string()
    } else if has_name {
        "moderate: name match only".to_string()
    } else if has_ledger && has_sig {
        "moderate: signature match + ledger entry".to_string()
    } else if has_ledger {
        "moderate: ledger entry (indirect match)".to_string()
    } else if has_sig {
        "weak: signature match only".to_string()
    } else if has_doc {
        "weak: doc-only match".to_string()
    } else if has_file {
        "weak: file path match only".to_string()
    } else {
        "weak: low-signal indirect match".to_string()
    }
}

/// Detect query tokens that match too many unrelated files (broad/ambiguous terms).
///
/// Returns token strings whose FTS hit count across distinct files exceeds the
/// threshold, indicating they will add noise rather than precision.
///
/// Uses a single SQL UNION ALL query to check all tokens in one round-trip
/// (previously N separate fts.search() calls, each fetching and iterating rows).
pub fn detect_ambiguous_tokens(
    tokens: &[String],
    db_path: &Path,
    filters: &FtsFilters,
) -> Vec<String> {
    const THRESHOLD: usize = 25;
    let fts = match SearchFtsDb::open(db_path) {
        Ok(f) if f.has_data() => f,
        _ => return vec![],
    };
    let candidates: Vec<&str> = tokens.iter()
        .filter(|t| !is_stopword(t))
        .map(|t| t.as_str())
        .collect();
    if candidates.is_empty() { return vec![]; }

    let counts = fts.count_distinct_files_per_token(&candidates, filters.include_tests)
        .unwrap_or_default();

    counts.into_iter()
        .filter(|(_, cnt)| *cnt > THRESHOLD)
        .map(|(tok, _)| tok)
        .collect()
}

/// Heuristic possible-miss warnings for a result set.
///
/// Checks whether the result set covers the layers implied by the query.
/// Returns human-readable warning strings for use in `possible_misses` output.
/// Pass `scope_narrowed = true` to suppress all warnings when the user
/// intentionally restricted results via --scope / --paths / --exclude (t-001).
pub fn detect_possible_misses(
    query: &str,
    layers_present: &HashSet<&str>,
    result_count: usize,
) -> Vec<String> {
    let ql = query.to_lowercase();
    let mut warnings = Vec::new();

    // UI/view query with no view-layer results.
    const VIEW_HINTS: &[&str] = &[
        "view", " ui", "render", "display", "screen", "layout", "widget", "cell", "button", "pad",
    ];
    const VIEW_LAYERS: &[&str] = &["view", "ui", "presentation"];
    if VIEW_HINTS.iter().any(|h| ql.contains(h))
        && !VIEW_LAYERS.iter().any(|l| layers_present.contains(l))
        && result_count > 0
    {
        warnings.push(
            "no view-layer symbols found — query suggests UI involvement; \
             check --scope or path coverage"
                .to_string(),
        );
    }

    // t-004: service/domain query with results only in UI layer.
    const SERVICE_HINTS: &[&str] = &["service", "manager", "coordinator", "handler", "processor", "store", "repository", "repo"];
    const SERVICE_LAYERS: &[&str] = &["service", "domain", "core", "infrastructure"];
    if SERVICE_HINTS.iter().any(|h| ql.contains(h))
        && result_count > 0
        && SERVICE_LAYERS.iter().all(|l| !layers_present.contains(l))
        && (layers_present.contains("view") || layers_present.contains("ui"))
    {
        warnings.push(
            "results are all UI-layer — query suggests service/domain involvement; \
             service layer symbols may not be indexed"
                .to_string(),
        );
    }

    // t-004: data/model query missing persistence layer.
    const DATA_HINTS: &[&str] = &["model", "entity", "persist", "database", "migration", "schema", "table"];
    const DATA_LAYERS: &[&str] = &["persistence", "data", "infrastructure", "domain"];
    if DATA_HINTS.iter().any(|h| ql.contains(h))
        && result_count > 0
        && DATA_LAYERS.iter().all(|l| !layers_present.contains(l))
    {
        warnings.push(
            "no persistence/data-layer symbols found — query suggests data model involvement"
                .to_string(),
        );
    }

    // t-005: Named-layer warnings for scheduler, engine, and network.
    const SCHEDULER_HINTS: &[&str] = &["scheduler", "schedule", "clock", "timer", "tick", "dispatch", "queue", "async"];
    if SCHEDULER_HINTS.iter().any(|h| ql.contains(h))
        && result_count > 0
        && !layers_present.contains("scheduler")
    {
        warnings.push(
            "scheduler layer absent from results — query implies timing/dispatch involvement; \
             check that scheduler symbols are indexed"
                .to_string(),
        );
    }

    const ENGINE_HINTS: &[&str] = &["engine", "audio", "video", "render", "pipeline", "runtime", "processor"];
    if ENGINE_HINTS.iter().any(|h| ql.contains(h))
        && result_count > 0
        && !layers_present.contains("engine")
        && !layers_present.contains("core_model")
    {
        warnings.push(
            "engine/core layer absent from results — query implies runtime/processing involvement; \
             consider broadening scope or adding the engine layer to the index"
                .to_string(),
        );
    }

    const NETWORK_HINTS: &[&str] = &["network", "api", "http", "request", "response", "endpoint", "fetch", "upload", "download"];
    if NETWORK_HINTS.iter().any(|h| ql.contains(h))
        && result_count > 0
        && !layers_present.contains("network")
        && !layers_present.contains("infrastructure")
    {
        warnings.push(
            "network layer absent from results — query implies API/network involvement; \
             check that network client symbols are indexed"
                .to_string(),
        );
    }

    // Very few results for a precise multi-word query.
    if result_count > 0 && result_count < 3 && ql.split_whitespace().count() >= 3 {
        warnings.push(format!(
            "only {} result{} for a multi-term query — symbols may not be indexed yet",
            result_count,
            if result_count == 1 { "" } else { "s" }
        ));
    }

    warnings
}

/// Suggest refined, scoped queries when the current query contains ambiguous terms.
///
/// Extracts domain-specific co-occurring tokens from the top result qnames and
/// combines them with the non-ambiguous query tokens to produce 2–3 focused suggestions.
pub fn suggest_scoped_queries(
    original_tokens: &[String],
    ambiguous_terms: &[String],
    top_qnames: &[String],
) -> Vec<String> {
    if ambiguous_terms.is_empty() || top_qnames.is_empty() { return vec![]; }

    let amb_set: HashSet<&str> = ambiguous_terms.iter().map(|s| s.as_str()).collect();
    let specific_tokens: Vec<&str> = original_tokens.iter()
        .filter(|t| !amb_set.contains(t.as_str()))
        .map(|s| s.as_str())
        .collect();

    // Extract candidate narrowing terms from top result qnames (last segment words).
    let mut cooccur: HashMap<String, usize> = HashMap::new();
    for qname in top_qnames {
        let last = qname.rsplit(|c: char| c == '.' || c == ':' || c == '/').next().unwrap_or(qname);
        // Split CamelCase into words.
        let mut cur = String::new();
        let mut words: Vec<String> = Vec::new();
        for ch in last.chars() {
            if ch.is_uppercase() && !cur.is_empty() {
                words.push(cur.clone()); cur.clear();
            }
            cur.push(ch.to_lowercase().next().unwrap_or(ch));
        }
        if !cur.is_empty() { words.push(cur); }
        // Also split snake_case.
        let words: Vec<String> = words.iter()
            .flat_map(|w| w.split('_').map(|s| s.to_string()).collect::<Vec<_>>())
            .filter(|w| w.len() > 2 && !is_stopword(w) && !amb_set.contains(w.as_str()))
            .collect();
        for w in words {
            if !original_tokens.iter().any(|t| t == &w) {
                *cooccur.entry(w).or_default() += 1;
            }
        }
    }

    // Pick top 3 most-common co-occurring terms not already in the query.
    let mut ranked: Vec<(usize, String)> = cooccur.into_iter().map(|(w, c)| (c, w)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0));

    let mut suggestions = Vec::new();
    for (_, narrowing_term) in ranked.iter().take(3) {
        let mut parts: Vec<&str> = specific_tokens.clone();
        parts.push(narrowing_term.as_str());
        let query = parts.join(" ");
        if !suggestions.contains(&query) {
            suggestions.push(query);
        }
    }
    suggestions
}

/// Distinguish whether low result confidence comes from ambiguous terms or a
/// sparse index. Returns a vec of typed warning objects with kind + message.
///
/// kinds: "ambiguous_term" | "sparse_index"
pub fn detect_confidence_warnings(
    tokens: &[String],
    result_count: usize,
    ambiguous_terms: &[String],
    db_path: &Path,
) -> Vec<serde_json::Value> {
    use serde_json::json;
    let mut warnings = Vec::new();

    // Ambiguous terms already detected upstream — annotate them with advice.
    for term in ambiguous_terms {
        warnings.push(json!({
            "kind": "ambiguous_term",
            "term": term,
            "message": format!(
                "\"{}\" matches too many unrelated files — add more specific terms to narrow results",
                term
            ),
        }));
    }

    // Sparse index: very few results but no ambiguous terms → index may be thin.
    if result_count < 3 && ambiguous_terms.is_empty() && !tokens.is_empty() {
        // Check total indexed symbol count as a proxy for index completeness.
        let total = SearchFtsDb::open(db_path)
            .ok()
            .filter(|f| f.has_data())
            .and_then(|f| f.search("", &FtsFilters::default(), 1).ok())
            .is_some();
        if total {
            warnings.push(json!({
                "kind": "sparse_index",
                "message": "very few results — the index may not cover this area yet; \
                            try `asd index` or broaden the query",
            }));
        }
    }

    warnings
}

/// Suggest more specific queries when the current query is too broad.
///
/// Returns a vec of suggestion strings. Empty when the query is already focused.
pub fn suggest_better_queries(tokens: &[String], query: &str) -> Vec<String> {
    if tokens.is_empty() { return vec![]; }
    let mut suggestions = Vec::new();

    // Single-token broad queries.
    let stopword_count = tokens.iter().filter(|t| is_stopword(t)).count();
    let meaningful = tokens.len() - stopword_count;

    if meaningful <= 1 {
        let tok = tokens.iter().find(|t| !is_stopword(t)).map(|s| s.as_str()).unwrap_or(query);
        suggestions.push(format!("try a more specific phrase, e.g. \"{} <subsystem>\" or \"{} <action>\"", tok, tok));
    }

    // Stopword-heavy query (>50% stopwords with 3+ tokens).
    if tokens.len() >= 3 && stopword_count * 2 > tokens.len() {
        suggestions.push(
            "query contains many common words — focus on domain nouns and verbs for better precision"
                .to_string(),
        );
    }

    // Very common single terms likely to be ambiguous.
    const BROAD_TERMS: &[&str] = &["update", "get", "set", "handle", "process", "run", "execute", "init", "start", "stop", "load", "save", "create", "delete", "state", "data", "model", "manager", "service", "util", "helper", "config"];
    for tok in tokens {
        if BROAD_TERMS.contains(&tok.as_str()) && tokens.len() == 1 {
            suggestions.push(format!(
                "\"{}\" is very broad — combine with a domain term, e.g. \"{} playhead\" or \"{} session\"",
                tok, tok, tok
            ));
        }
    }

    suggestions.dedup();
    suggestions
}

/// Tokenise a feedback query string into a set of non-stopword terms.
fn fb_query_tokens(q: &str) -> std::collections::HashSet<String> {
    q.split(|c: char| !c.is_alphabetic())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_lowercase())
        .filter(|t| !is_stopword(t))
        .collect()
}

/// Extract name tokens from a fully-qualified symbol name for sibling suppression.
/// Takes the last `::` or `.`-delimited segment, then splits snake_case by `_`
/// and CamelCase at uppercase boundaries.
fn name_tokens_from_qname(qname: &str) -> HashSet<String> {
    let last = qname.rsplit(|c| c == ':' || c == '.').next().unwrap_or(qname);
    // Split CamelCase at uppercase boundaries.
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in last.chars() {
        if ch.is_uppercase() && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        cur.push(ch.to_lowercase().next().unwrap_or(ch));
    }
    if !cur.is_empty() { tokens.push(cur); }
    // Also split by `_`.
    tokens.iter()
        .flat_map(|t| t.split('_'))
        .filter(|t| t.len() > 2 && !is_stopword(t))
        .map(|t| t.to_string())
        .collect()
}

/// t-003: Query-family matching — a stored verdict applies to the current query
/// when their token sets overlap by at least one token (not just substring match).
/// This makes "drift playhead" verdicts apply to "playhead drift" and
/// "drift pad playhead position".
fn query_family_matches(current_query_tokens: &std::collections::HashSet<String>, fb_query: &str) -> bool {
    if fb_query.is_empty() { return true; }
    let fb_tokens = fb_query_tokens(fb_query);
    if fb_tokens.is_empty() { return true; }
    // Any shared non-stopword token means the queries are in the same family.
    fb_tokens.iter().any(|t| current_query_tokens.contains(t))
}

/// Apply feedback verdicts as score adjustments after candidate selection.
///
/// `Useful` entries add a boost; `Noisy` / `WrongLayer` entries demote the
/// symbol to negative infinity (effectively removed). Also:
///
/// - t-002: Sibling propagation — when a symbol in a file is marked Noisy or
///   WrongLayer, all other symbols from the same file are also suppressed for
///   query families that overlap the noisy verdict.
/// - t-003: Query-family matching — a stored verdict applies to the current
///   query when they share at least one non-stopword token, not just exact
///   substring match.
/// - Recurring false positives — symbols with ≥2 noisy verdicts sharing a
///   query token are suppressed for any query that overlaps those tokens.
///
/// Called at CLI/MCP call sites after `find_candidates`.
pub fn apply_feedback_adjustments(
    engine: &Engine,
    index_store: &AsgIndexStore,
    query: &str,
    scored: &mut Vec<(f64, String)>,
    feedback_entries: &[(String, String, crate::schema::FeedbackVerdict)],
) {
    if feedback_entries.is_empty() { return; }
    let query_norm = query.to_lowercase();
    let current_tokens: std::collections::HashSet<String> = fb_query_tokens(&query_norm);
    let current_tokens_vec: Vec<&str> = current_tokens.iter().map(|s| s.as_str()).collect();

    // --- Recurring false positive map (same as before) ---
    let mut noisy_queries_by_sym: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (sym_id, fb_query, verdict) in feedback_entries {
        if matches!(verdict, crate::schema::FeedbackVerdict::Noisy | crate::schema::FeedbackVerdict::WrongLayer) {
            noisy_queries_by_sym.entry(sym_id.as_str()).or_default().push(fb_query.as_str());
        }
    }
    let recurring_fp: std::collections::HashMap<&str, Vec<String>> = noisy_queries_by_sym
        .into_iter()
        .filter(|(_, queries)| queries.len() >= 2)
        .map(|(sym_id, queries)| {
            let mut token_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for q in &queries {
                let tokens = fb_query_tokens(q);
                for t in tokens { *token_counts.entry(t).or_default() += 1; }
            }
            let suppression_tokens: Vec<String> = token_counts
                .into_iter()
                .filter(|(_, count)| *count >= 2)
                .map(|(t, _)| t)
                .collect();
            (sym_id, suppression_tokens)
        })
        .collect();

    // --- t-001: Build noisy-file map: file → [(sym_id, kind, name_tokens)].
    // We single-pass the index tree so we only pay one repo.get_tree call.
    // Only symbols whose query family overlaps the current query are tracked.
    struct NoisySymInfo { sym_id: String, name_tokens: HashSet<String> }
    let mut noisy_file_syms: HashMap<String, Vec<NoisySymInfo>> = HashMap::new();
    {
        let noisy_ids: HashSet<&str> = feedback_entries.iter()
            .filter(|(_, fb_query, verdict)| {
                matches!(verdict, crate::schema::FeedbackVerdict::Noisy | crate::schema::FeedbackVerdict::WrongLayer)
                    && query_family_matches(&current_tokens, fb_query)
            })
            .map(|(sym_id, _, _)| sym_id.as_str())
            .collect();
        if !noisy_ids.is_empty() {
            let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
            if let Ok(serde_json::Value::Object(map)) = engine.repo.get_tree(&engine.ref_name, &prefix) {
                for sym_val in map.values() {
                    if let Ok(sym) = serde_json::from_value::<Symbol>(sym_val.clone()) {
                        if noisy_ids.contains(sym.symbol_id.as_str()) {
                            let name_tokens = name_tokens_from_qname(&sym.qname);
                            noisy_file_syms.entry(sym.file.clone()).or_default().push(NoisySymInfo {
                                sym_id: sym.symbol_id,
                                name_tokens,
                            });
                        }
                    }
                }
            }
        }
    }

    // --- Score adjustments ---
    for (score, qname) in scored.iter_mut() {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let sym_id = &sym.symbol_id;

        // Sibling file suppression: any symbol sharing a file with a noisy symbol
        // is suppressed unless it has an explicit Useful verdict or is a Class/Module
        // container (which may host unrelated symbols in the same file).
        if let Some(noisy_syms) = noisy_file_syms.get(&sym.file) {
            let is_container = matches!(sym.kind, SymbolKind::Class | SymbolKind::Module);
            let is_the_noisy_sym = noisy_syms.iter().any(|ns| ns.sym_id == *sym_id);
            if !is_container && !is_the_noisy_sym {
                let has_useful = feedback_entries.iter().any(|(fid, fq, v)| {
                    fid == sym_id
                        && matches!(v, crate::schema::FeedbackVerdict::Useful)
                        && query_family_matches(&current_tokens, fq)
                });
                if !has_useful {
                    *score = f64::NEG_INFINITY;
                    continue;
                }
            }
        }

        // Per-symbol verdict matching (t-003: query-family aware).
        for (fb_symbol_id, fb_query, verdict) in feedback_entries {
            if fb_symbol_id != sym_id { continue; }
            // t-003: use token-family matching instead of substring containment.
            if !query_family_matches(&current_tokens, fb_query) { continue; }
            match verdict {
                crate::schema::FeedbackVerdict::Useful => *score += 1.5,
                crate::schema::FeedbackVerdict::Noisy
                | crate::schema::FeedbackVerdict::WrongLayer => {
                    *score = f64::NEG_INFINITY;
                }
                crate::schema::FeedbackVerdict::Missing => {}
            }
        }

        // Recurring false positive suppression.
        if score.is_finite() {
            if let Some(sup_tokens) = recurring_fp.get(sym_id.as_str()) {
                if current_tokens_vec.iter().any(|qt| sup_tokens.iter().any(|st| st == *qt)) {
                    *score = f64::NEG_INFINITY;
                }
            }
        }
    }
    scored.retain(|(s, _)| s.is_finite());
}

/// Per-result explanation of which feedback verdict affected a search hit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedbackImpact {
    /// Verdict that applied: "useful", "noisy", "wrong_layer", or "sibling_suppressed".
    pub verdict: String,
    /// The original feedback query that triggered this impact.
    pub matched_query: String,
    /// The author who recorded the feedback.
    pub author: String,
}

/// For each qname in `qnames`, determine which feedback verdict (if any)
/// affected it and return a map of qname → FeedbackImpact.
///
/// Used by search commands to annotate results with why they were boosted,
/// demoted, or suppressed.
pub fn explain_feedback_impacts(
    engine: &Engine,
    index_store: &AsgIndexStore,
    query: &str,
    qnames: &[String],
    feedback_entries: &[crate::schema::FeedbackEntry],
) -> HashMap<String, FeedbackImpact> {
    let mut impacts: HashMap<String, FeedbackImpact> = HashMap::new();
    if feedback_entries.is_empty() || qnames.is_empty() { return impacts; }
    let query_norm = query.to_lowercase();
    let current_tokens = fb_query_tokens(&query_norm);

    // Build noisy-file → name_tokens map.
    struct NoisyFileEntry { sym_id: String, name_tokens: HashSet<String> }
    let mut noisy_file_syms: HashMap<String, Vec<NoisyFileEntry>> = HashMap::new();
    {
        let noisy_ids: HashSet<&str> = feedback_entries.iter()
            .filter(|e| {
                matches!(e.verdict, crate::schema::FeedbackVerdict::Noisy | crate::schema::FeedbackVerdict::WrongLayer)
                    && query_family_matches(&current_tokens, &e.query)
            })
            .map(|e| e.symbol_id.as_str())
            .collect();
        if !noisy_ids.is_empty() {
            let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
            if let Ok(serde_json::Value::Object(map)) = engine.repo.get_tree(&engine.ref_name, &prefix) {
                for sym_val in map.values() {
                    if let Ok(sym) = serde_json::from_value::<Symbol>(sym_val.clone()) {
                        if noisy_ids.contains(sym.symbol_id.as_str()) {
                            noisy_file_syms.entry(sym.file.clone()).or_default().push(NoisyFileEntry {
                                sym_id: sym.symbol_id,
                                name_tokens: name_tokens_from_qname(&sym.qname),
                            });
                        }
                    }
                }
            }
        }
    }

    for qname in qnames {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let sym_id = &sym.symbol_id;

        // Check per-symbol verdicts first (direct match takes priority).
        let mut found = false;
        for entry in feedback_entries {
            if entry.symbol_id != *sym_id { continue; }
            if !query_family_matches(&current_tokens, &entry.query) { continue; }
            let v_str = entry.verdict.as_str();
            impacts.insert(qname.clone(), FeedbackImpact {
                verdict: v_str.to_string(),
                matched_query: entry.query.clone(),
                author: entry.author.clone(),
            });
            found = true;
            break;
        }
        if found { continue; }

        // Check sibling suppression.
        if let Some(noisy_syms) = noisy_file_syms.get(&sym.file) {
            if !matches!(sym.kind, SymbolKind::Class | SymbolKind::Module) {
                let candidate_tokens = name_tokens_from_qname(qname);
                let overlaps = noisy_syms.iter().any(|ns| {
                    ns.sym_id != *sym_id
                        && candidate_tokens.iter().any(|t| ns.name_tokens.contains(t))
                });
                if overlaps {
                    if let Some(entry) = feedback_entries.iter().find(|e| {
                        noisy_syms.iter().any(|ns| ns.sym_id == e.symbol_id)
                            && matches!(e.verdict, crate::schema::FeedbackVerdict::Noisy | crate::schema::FeedbackVerdict::WrongLayer)
                            && query_family_matches(&current_tokens, &e.query)
                    }) {
                        impacts.insert(qname.clone(), FeedbackImpact {
                            verdict: "sibling_suppressed".to_string(),
                            matched_query: entry.query.clone(),
                            author: entry.author.clone(),
                        });
                    }
                }
            }
        }
    }
    impacts
}

// ---------------------------------------------------------------------------
// File-scope feedback (t-003)
// ---------------------------------------------------------------------------

/// Apply file-scoped feedback verdicts to `scored`.
///
/// Entries with `file_scope` set demote or boost all symbols from files
/// matching the glob pattern, subject to query-family matching.
pub fn apply_file_scope_feedback(
    engine: &Engine,
    index_store: &AsgIndexStore,
    query: &str,
    scored: &mut Vec<(f64, String)>,
    file_scope_entries: &[(String, crate::schema::FeedbackVerdict, String)],
) {
    if file_scope_entries.is_empty() { return; }
    let query_norm = query.to_lowercase();
    let current_tokens = fb_query_tokens(&query_norm);

    for (score, qname) in scored.iter_mut() {
        if !score.is_finite() { continue; }
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        for (file_glob, verdict, entry_query) in file_scope_entries {
            if !query_family_matches(&current_tokens, entry_query) { continue; }
            if !glob_match(file_glob, &sym.file) { continue; }
            match verdict {
                crate::schema::FeedbackVerdict::Useful => *score += 1.5,
                crate::schema::FeedbackVerdict::Noisy | crate::schema::FeedbackVerdict::WrongLayer => {
                    *score = f64::NEG_INFINITY;
                    break;
                }
                crate::schema::FeedbackVerdict::Missing => {}
            }
        }
    }
    scored.retain(|(s, _)| s.is_finite());
}
