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

        // File-stem injection: a file is "covered" only when the claiming
        // symbol in the top-`depth` results itself has ledger entries.  If a
        // non-ledger symbol holds the slot, the file stays open so a
        // ledger-bearing sibling can be injected via stem.  Files ranked
        // depth+1..depth*8 are always eligible for re-injection.
        let covered_files: HashSet<String> = scored
            .iter()
            .take(depth)
            .filter_map(|(_, qname)| {
                let sym = index_store.get_symbol_by_qname(&engine.ref_name, qname)
                    .ok().flatten()?;
                let has_ledger = !ledger_store
                    .list_entries(&engine.ref_name, &sym.symbol_id)
                    .unwrap_or_default()
                    .is_empty();
                if has_ledger { Some(sym.file) } else { None }
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
        reasons.push(format!("ledger:{} invariant{}", inv_count, if inv_count == 1 { "" } else { "s" }));
    }
    if haz_count > 0 {
        reasons.push(format!("ledger:{} hazard{}", haz_count, if haz_count == 1 { "" } else { "s" }));
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
/// - `core`      — has ledger entries AND high-signal match (name token or hot file)
/// - `supporting`— has ledger, name match, or recent edit
/// - `noisy`     — only file/sig/doc lexical match; no ledger, not hot
/// - `test-only` — symbol lives in a test file
pub fn result_bucket(
    file: &str,
    match_reasons: &[String],
    has_ledger: bool,
    is_hot: bool,
) -> &'static str {
    use crate::search_fts::symbol_tier;
    if symbol_tier(file) == 2 { return "test-only"; }
    let has_name = match_reasons.iter().any(|r| r.starts_with("name:"));
    if has_ledger && (has_name || is_hot) { return "core"; }
    if has_ledger || has_name || is_hot { return "supporting"; }
    "noisy"
}

/// Detect query tokens that match too many unrelated files (broad/ambiguous terms).
///
/// Returns token strings whose FTS hit count across distinct files exceeds the
/// threshold, indicating they will add noise rather than precision.
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
    tokens.iter()
        .filter(|t| !is_stopword(t))
        .filter(|token| {
            fts.search(token, filters, THRESHOLD + 10)
                .map(|hits| {
                    hits.iter()
                        .map(|h| h.file.as_str())
                        .collect::<HashSet<_>>()
                        .len() > THRESHOLD
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Heuristic possible-miss warnings for a result set.
///
/// Checks whether the result set covers the layers implied by the query.
/// Returns human-readable warning strings for use in `possible_misses` output.
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

/// Apply feedback verdicts as score adjustments after candidate selection.
///
/// `Useful` entries add a boost; `Noisy` / `WrongLayer` entries demote the
/// symbol to negative infinity (effectively removed). Called at CLI/MCP
/// call sites after `find_candidates`.
pub fn apply_feedback_adjustments(
    engine: &Engine,
    index_store: &AsgIndexStore,
    query: &str,
    scored: &mut Vec<(f64, String)>,
    feedback_entries: &[(String, String, crate::schema::FeedbackVerdict)],
) {
    // feedback_entries: (symbol_id, query_pattern, verdict)
    if feedback_entries.is_empty() { return; }
    let query_norm = query.to_lowercase();

    for (score, qname) in scored.iter_mut() {
        let sym_id = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s.symbol_id,
            _ => continue,
        };
        for (fb_symbol_id, fb_query, verdict) in feedback_entries {
            if fb_symbol_id != &sym_id { continue; }
            let q_matches = fb_query.is_empty()
                || query_norm.contains(fb_query.as_str())
                || fb_query.contains(query_norm.as_str());
            if !q_matches { continue; }
            match verdict {
                crate::schema::FeedbackVerdict::Useful => *score += 1.5,
                crate::schema::FeedbackVerdict::Noisy
                | crate::schema::FeedbackVerdict::WrongLayer => {
                    *score = f64::NEG_INFINITY;
                }
                crate::schema::FeedbackVerdict::Missing => {}
            }
        }
    }
    scored.retain(|(s, _)| s.is_finite());
}
