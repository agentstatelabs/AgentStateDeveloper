//! Plan E t-006: shared helpers for the prepare-change pipeline.
//!
//! Background: the full prepare-change orchestration is duplicated across
//! `agentstatedeveloper-cli/src/commands/prepare_change.rs` and the
//! `prepare_change` handler in `agentstatedeveloper-mcp/src/mcp_server.rs`.
//! Plan A t-009 added file_score_floor + top_symbol/why rationale and had
//! to edit BOTH sites with parallel destructure-tuple updates.
//!
//! This module single-sources the t-009 logic so future edits to file
//! scoring land once. The remaining orchestration (by_layer assembly,
//! recipe_inspect/edit/run lists) stays per-surface — Plan F captures the
//! full extract as a follow-up when that duplication becomes painful.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

use crate::candidates::{confidence_scores, explain_match, result_bucket};
use crate::effects::{AsgEffectStore, EffectStore};
use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::{ASD_PATH_PREFIX, LedgerEntry, LedgerKind, Symbol};
use crate::search_fts::{
    FileRecency, SearchFtsDb, classify_file_role, classify_layer_sym, extract_summary,
    git_dirty_files, symbol_tier,
};

/// Plan A t-009 / Plan E t-006: per-file score entry built during the
/// prepare-change candidate walk. Carries everything the JSON output
/// needs without forcing the caller to re-derive layer/recency/why.
#[derive(Debug, Clone, Serialize)]
pub struct FileScoreEntry {
    pub score: f64,
    pub file: String,
    pub layer: String,
    pub last_touched_days: Option<f64>,
    pub hot: bool,
    pub top_symbol: String,
    pub why: String,
}

/// Relative score floor for file inclusion in `likely_edit_files`.
///
/// ExampleFlow field-eval (2026-06-04, refinement #2): the prior 0.25
/// floor (25% of top score) was too permissive on targeted queries.
/// ExampleProj observation: a query like "ProjectManager save logic" with
/// top BM25 ~10 would admit files scoring 2.5+, sweeping in
/// project/session/UI files that share no query tokens but had hotness
/// boost from recent edits. Raising to 0.40 cuts that noise while
/// keeping legitimate secondary results on broader queries (the top
/// candidate has to be substantially better than the cutoff for the
/// cutoff to matter — broad queries with several near-equal candidates
/// are unaffected because the floor scales with the top score).
pub const FILE_SCORE_FLOOR_RATIO: f64 = 0.40;

/// Cliff detection threshold (1.0.85, ExampleFlow refinement #2 deep
/// half): when a consecutive score ratio drops below this, the lower
/// score is treated as the start of a new "also-ran" cohort and the
/// floor is raised to exclude it.
///
/// ExampleFlow case: query "Drift Pad scheduler sync" produced
/// scores 42 / 31 / 19 / 18. Ratios: 31/42=0.74, 19/31=0.61, 18/19=0.95.
/// The 19/31 ratio (0.61 < 0.70) marks the cliff between cohort 1
/// (genuine matches at 42 and 31) and cohort 2 (path-name false
/// matches at 19 and 18). Floor set to 31 → 2 files survive instead
/// of 4.
///
/// 0.70 picked empirically: queries with smoothly-decaying scores
/// (e.g. 100 / 90 / 80 / 70) have all consecutive ratios > 0.7 and
/// are NOT cliff-cut. Only sharp drops trigger.
pub const CLIFF_RATIO_THRESHOLD: f64 = 0.70;

/// Compute the precision floor for file scoring. Two-pass:
///   1. Relative floor: 40% of the top candidate's score.
///   2. Cliff-aware floor: walk consecutive ranks looking for a sharp
///      drop (ratio < 0.70). When found, raise the floor to the last
///      pre-cliff score so the also-ran cohort is excluded.
///
/// Files scoring below the final floor are dropped from
/// `likely_edit_files`. Returns 0.0 when `candidates` is empty.
///
/// The cliff pass is conservative: it triggers only on sharp drops,
/// leaving smooth-decay queries (where every rank is a legitimate
/// secondary hit) unaffected. Field-eval rationale in
/// `CLIFF_RATIO_THRESHOLD` docs.
/// Find the cliff cutoff index in a descending-sorted score list.
/// Returns the count of entries that survive (the index right
/// after the last pre-cliff score), or `scores.len()` when there's
/// no cliff in the list.
///
/// ExampleFlow 1.0.86 regression (2026-06-04): the candidate-level
/// cliff in `file_score_floor` missed cliffs that appear only after
/// file aggregation. Symbol candidates had a smooth gradient
/// (42/31/29/27/25/19/18) — no consecutive pair triggered 0.70.
/// But file-level top scores were 42/31/19/18 — a clear 0.61 cliff
/// at rank 3. This helper operates on the aggregated file scores
/// so cliff detection happens at the right granularity.
///
/// Use:
///   let cut = cliff_cutoff_index(file_scores.iter().map(|f| f.0));
///   file_scores.truncate(cut);
pub fn cliff_cutoff_index<I: IntoIterator<Item = f64>>(scores: I) -> usize {
    let scores: Vec<f64> = scores.into_iter().collect();
    if scores.len() < 2 {
        return scores.len();
    }
    for i in 0..scores.len() - 1 {
        let prev = scores[i];
        let next = scores[i + 1];
        if prev <= 0.0 {
            return i + 1;
        }
        if next / prev < CLIFF_RATIO_THRESHOLD {
            return i + 1; // keep through index i, cut starts at i+1
        }
    }
    scores.len()
}

pub fn file_score_floor(candidates: &[(f64, String)]) -> f64 {
    let Some((top, _)) = candidates.first() else {
        return 0.0;
    };
    let relative_floor = top * FILE_SCORE_FLOOR_RATIO;

    // Cliff pass: walk pairs of consecutive scores. The first pair
    // where the LOWER score is < 0.70 of the higher score marks the
    // cohort boundary. Floor becomes the HIGHER score (inclusive
    // cutoff — pre-cliff cohort survives).
    let mut cliff_floor = relative_floor;
    for window in candidates.windows(2) {
        let prev = window[0].0;
        let next = window[1].0;
        if prev <= 0.0 {
            break;
        }
        if next / prev < CLIFF_RATIO_THRESHOLD {
            cliff_floor = prev;
            break;
        }
    }
    relative_floor.max(cliff_floor)
}

/// Append a file-score entry if `file` is not yet seen AND `score` is at or
/// above `floor`. Returns true when an entry was pushed. The caller owns the
/// `seen_files` set + the destination Vec — keeps this helper allocation-free
/// and reusable across CLI / MCP orchestration loops that interleave file
/// scoring with other ledger walks.
pub fn push_file_score(
    out: &mut Vec<FileScoreEntry>,
    seen_files: &mut std::collections::HashSet<String>,
    sym: &Symbol,
    entries: &[LedgerEntry],
    tokens: &[String],
    score: f64,
    layer: &str,
    last_touched_days: Option<f64>,
    hot: bool,
    floor: f64,
) -> bool {
    if score < floor {
        return false;
    }
    if !seen_files.insert(sym.file.clone()) {
        return false;
    }
    let reasons = explain_match(sym, tokens, entries, hot);
    let why = reasons
        .first()
        .cloned()
        .unwrap_or_else(|| format!("contains symbol {}", sym.qname));
    out.push(FileScoreEntry {
        score,
        file: sym.file.clone(),
        layer: layer.to_string(),
        last_touched_days,
        hot,
        top_symbol: sym.qname.clone(),
        why,
    });
    true
}

// ---------------------------------------------------------------------------
// Plan M t-004 (1.0.98): full scoring walk lifted from CLI prepare_change
// + MCP prepare_change handler. Closes the Plan F TODO. Both call sites
// now share one canonical pipeline:
//   aggregate_candidate_data → propagate_caller_invariants → finalize_file_scores
// ---------------------------------------------------------------------------

/// Per-row file score tuple shape used by the prepare-change pipeline.
/// (score, file, layer, last_touched_days, hot, top_symbol, why)
///
/// Kept as a tuple (not a struct) because the t-009/t-003 sites already
/// destructure it positionally in many places. The longer-term refactor
/// can promote this to `FileScoreEntry` (which exists above) once all
/// call sites use field access.
pub type FileScoreTuple = (f64, String, String, Option<f64>, bool, String, String);

/// Holds the six parallel collections built up by the per-candidate
/// scan plus the top symbol id (driver for downstream BFS) and the
/// `seen_inv` dedup set (threaded into the subsequent caller-invariant
/// propagation pass so both invariant sources share one namespace).
#[derive(Debug, Default)]
pub struct CandidateAggregates {
    pub by_layer: serde_json::Map<String, Value>,
    pub design_invariants: Vec<Value>,
    pub known_hazards: Vec<Value>,
    pub validation_scenarios_ledger: Vec<Value>,
    pub effects_summary: Vec<Value>,
    pub file_scores: Vec<FileScoreTuple>,
    pub top_sym_id: Option<String>,
    pub seen_inv: HashSet<String>,
}

/// Main candidate-scan pass. Iterates the ranked candidate list once,
/// populating all six parallel accumulators:
///   - `by_layer` — entry-point JSON grouped by classified layer
///   - `design_invariants` — Invariant ledger entries, deduped on summary
///   - `known_hazards` — Hazard ledger entries
///   - `validation_scenarios_ledger` — ValidationScenario entries, deduped
///   - `effects_summary` — declared effects (low-signal suppressed unless sole)
///   - `file_scores` — per-file score row used by likely_edit_files
///
/// Plan M t-004 (1.0.98): lifted from CLI commands/prepare_change.rs and
/// the MCP prepare_change handler. Both surfaces now call this directly,
/// closing the Plan F TODO that flagged the duplication.
pub fn aggregate_candidate_data(
    engine: &Engine,
    index_store: &AsgIndexStore<'_>,
    ledger_store: &AsgLedgerStore<'_>,
    effect_store: &AsgEffectStore<'_>,
    candidates: &[(f64, String)],
    tokens: &[String],
    recency: &HashMap<String, FileRecency>,
    layer_overrides: &[(String, String)],
) -> CandidateAggregates {
    let raw_scores: Vec<f64> = candidates.iter().map(|(s, _)| *s).collect();
    let confidences = confidence_scores(&raw_scores);
    let mut by_layer: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut design_invariants: Vec<Value> = Vec::new();
    let mut known_hazards: Vec<Value> = Vec::new();
    let mut validation_scenarios_ledger: Vec<Value> = Vec::new();
    let mut effects_summary: Vec<Value> = Vec::new();
    let mut seen_inv: HashSet<String> = HashSet::new();
    let mut seen_vs: HashSet<String> = HashSet::new();
    let mut seen_effect: HashSet<String> = HashSet::new();
    // Only include effects from symbols scoring ≥25% of the top score to reduce noise.
    let effect_score_floor = candidates.first().map(|(s, _)| s * 0.25).unwrap_or(0.0);

    // ExampleFlow refinement #2 (1.0.83): file_score_floor was bumped
    // 0.25 → 0.40 to cut targeted-query noise.
    let mut file_scores: Vec<FileScoreTuple> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();
    let file_score_floor = file_score_floor(candidates);

    let mut top_sym_id: Option<String> = None;

    for (idx, (score, qname)) in candidates.iter().enumerate() {
        let conf = confidences.get(idx).copied().unwrap_or(0.5);
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let tier = symbol_tier(&sym.file);
        let layer = classify_layer_sym(&sym.file, &sym.qname, tier, layer_overrides);
        let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
        let rec = recency.get(&sym.file);
        let last_touched_days = rec.and_then(|r| r.last_touched_days);
        let hot = rec.map(|r| r.hot).unwrap_or(false);

        if top_sym_id.is_none() {
            top_sym_id = Some(sym.symbol_id.clone());
        }

        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();

        // File tracking — capture the contributing symbol + its top match
        // reason so prepare-change can answer "why this file?".
        if seen_files.insert(sym.file.clone()) && *score >= file_score_floor {
            let reasons = explain_match(&sym, tokens, &entries, hot);
            let why = reasons
                .first()
                .cloned()
                .unwrap_or_else(|| format!("contains symbol {}", sym.qname));
            file_scores.push((
                *score,
                sym.file.clone(),
                layer.to_string(),
                last_touched_days,
                hot,
                sym.qname.clone(),
                why,
            ));
        }
        for entry in &entries {
            let key = entry.summary.clone();
            match entry.kind {
                LedgerKind::Invariant => {
                    if seen_inv.insert(key) {
                        // 1.0.86: include entry_id so downstream "duplicate"
                        // sections (preserve, suggested_test_coverage,
                        // scenario_tests) can reference by id.
                        design_invariants.push(json!({
                            "entry_id": entry.entry_id,
                            "summary": entry.summary,
                            "source": sym.qname,
                        }));
                    }
                }
                LedgerKind::Hazard => {
                    known_hazards.push(json!({
                        "summary": entry.summary,
                        "source": sym.qname,
                    }));
                }
                LedgerKind::ValidationScenario => {
                    if seen_vs.insert(key) {
                        validation_scenarios_ledger.push(json!({
                            "scenario": entry.summary,
                            "source": sym.qname,
                        }));
                    }
                }
                _ => {}
            }
        }

        // Effects — only from sufficiently-scoring candidates to reduce noise.
        // Low-signal effects (throw, random, log, pure, time.read, time.sleep)
        // are suppressed unless they are the only effects declared on the symbol.
        if *score >= effect_score_floor {
            if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, &sym.symbol_id) {
                let has_high_signal = decl.declared.iter().any(|e| !e.effect.is_low_signal());
                for eff in &decl.declared {
                    if has_high_signal && eff.effect.is_low_signal() {
                        continue;
                    }
                    let cat = eff.effect.as_str().to_string();
                    let key = format!("{}:{}", cat, sym.qname);
                    if seen_effect.insert(key) {
                        effects_summary.push(json!({
                            "category": cat,
                            "source": sym.qname,
                        }));
                    }
                }
            }
        }

        // Add to by_layer with full enrichment (confidence/bucket/match_reasons).
        let has_ledger = !entries.is_empty();
        let match_reasons = explain_match(&sym, tokens, &entries, hot);
        let bucket = result_bucket(&sym.file, &match_reasons, has_ledger, hot);
        let ep_val = json!({
            "score": score,
            "confidence": conf,
            "bucket": bucket,
            "match_reasons": match_reasons,
            "qname": sym.qname,
            "file": sym.file,
            "line": sym.start.line,
            "layer": layer,
            "summary": summary,
            "last_touched_days": last_touched_days,
            "hot": hot,
        });
        by_layer
            .entry(layer.to_string())
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .unwrap()
            .push(ep_val);
    }

    CandidateAggregates {
        by_layer,
        design_invariants,
        known_hazards,
        validation_scenarios_ledger,
        effects_summary,
        file_scores,
        top_sym_id,
        seen_inv,
    }
}

/// Plan J t-001 caller-invariant propagation.
///
/// Walks each candidate's direct callers (depth=1) and returns any
/// Invariant ledger entries that aren't already in `seen_inv`. Each
/// returned value is tagged `from_caller: true` so the agent can tell
/// which upstream contract is at stake.
///
/// `seen_inv` is mutated — new invariant summaries are inserted as the
/// helper accumulates them, preserving the original loop's dedup
/// semantics with the main-candidate invariants gathered earlier.
///
/// Plan M t-004 (1.0.98): lifted to core so CLI + MCP share one impl.
pub fn propagate_caller_invariants(
    engine: &Engine,
    index_store: &AsgIndexStore<'_>,
    ledger_store: &AsgLedgerStore<'_>,
    db_path: &std::path::Path,
    candidates: &[(f64, String)],
    seen_inv: &mut HashSet<String>,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut candidate_sym_ids: Vec<(String, String)> = Vec::new();
    for (_, qname) in candidates.iter() {
        if let Ok(Some(s)) = index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            candidate_sym_ids.push((s.symbol_id, s.qname));
        }
    }
    let mut caller_ids_seen: HashSet<String> = HashSet::new();
    let mut caller_visit_order: Vec<(String, String)> = Vec::new();
    for (cand_sym_id, _) in &candidate_sym_ids {
        let direct_callers = index_store
            .get_callers(&engine.ref_name, cand_sym_id)
            .unwrap_or_default();
        for caller_id in direct_callers {
            if candidate_sym_ids.iter().any(|(sid, _)| sid == &caller_id) {
                continue;
            }
            if caller_ids_seen.insert(caller_id.clone()) {
                caller_visit_order.push((cand_sym_id.clone(), caller_id));
            }
        }
    }
    let caller_id_strs: Vec<&str> = caller_visit_order
        .iter()
        .map(|(_, cid)| cid.as_str())
        .collect();
    let caller_resolved = SearchFtsDb::open(db_path)
        .ok()
        .map(|fts| fts.resolve_symbol_ids_bulk(&caller_id_strs))
        .unwrap_or_default();
    let need_fallback = caller_visit_order
        .iter()
        .any(|(_, cid)| !caller_resolved.contains_key(cid.as_str()));
    let fallback_id_to_qname: HashMap<String, String> = if need_fallback {
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        match engine.repo.get_tree(&engine.ref_name, &prefix) {
            Ok(Value::Object(m)) => m
                .into_iter()
                .filter_map(|(qn, v)| {
                    v.get("symbol_id")
                        .and_then(|v| v.as_str())
                        .map(|sid| (sid.to_string(), qn))
                })
                .collect(),
            _ => HashMap::new(),
        }
    } else {
        HashMap::new()
    };
    for (_cand_sym_id, caller_id) in &caller_visit_order {
        let caller_qname = caller_resolved
            .get(caller_id.as_str())
            .map(|r| r.qname.as_str())
            .or_else(|| fallback_id_to_qname.get(caller_id).map(String::as_str));
        let Some(caller_qname) = caller_qname else {
            continue;
        };
        let caller_entries = ledger_store
            .list_entries(&engine.ref_name, caller_id)
            .unwrap_or_default();
        for entry in caller_entries {
            if !matches!(entry.kind, LedgerKind::Invariant) {
                continue;
            }
            let key = entry.summary.clone();
            if seen_inv.insert(key) {
                out.push(json!({
                    "entry_id": entry.entry_id,
                    "summary": entry.summary,
                    "source": caller_qname,
                    "from_caller": true,
                }));
            }
        }
    }
    out
}

/// Detect conflict risk for a single file. Returns Some(reason) if the
/// file has unresolved merge markers or git status modifications;
/// None when the file is clean.
///
/// Plan M t-004 (1.0.98): lifted from CLI prepare_change.rs so the MCP
/// handler's `likely_edit_files` can carry conflict_detail too (was a
/// CLI-only field; now both surfaces emit it).
pub fn explain_conflict_risk(file: &str) -> Option<String> {
    if let Ok(content) = std::fs::read_to_string(file) {
        if content.contains("<<<<<<<") {
            return Some("file contains unresolved merge conflict markers".to_string());
        }
    }
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--", file])
        .output()
        .ok()?;
    let status_str = String::from_utf8_lossy(&status.stdout);
    let code = status_str.chars().take(2).collect::<String>();
    let reason = match code.trim() {
        "M" | "MM" => "has unstaged modifications",
        "A" => "is newly staged",
        "D" => "is staged for deletion",
        "R" => "has been renamed (staged)",
        "UU" => "has unmerged changes",
        s if s.contains('M') => "has staged and/or unstaged modifications",
        _ => "has uncommitted changes",
    };
    Some(reason.to_string())
}

/// Sort + cliff-cut + build the `likely_edit_files` JSON array.
///
/// Mutates `file_scores` in place: sorts hot-first then score-desc, then
/// applies the file-level cliff cut. Returns the JSON value array used
/// directly in the prepare_change output.
///
/// `dirty_files` is taken by ref so the caller's hoisted git_dirty_files()
/// result is reusable for downstream stale_symbols.
///
/// Plan M t-004 (1.0.98): lifted to core. Whether the helper computes
/// `conflict_detail` is controlled by `include_conflict_detail` so MCP
/// can gain the richer output without forcing it on legacy consumers
/// (set true on both call sites today; flag exists for forward control).
pub fn finalize_file_scores(
    file_scores: &mut Vec<FileScoreTuple>,
    dirty_files: &HashSet<String>,
    include_conflict_detail: bool,
) -> Vec<Value> {
    file_scores.sort_by(|a, b| {
        b.4.cmp(&a.4)
            .then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut score_only_sorted: Vec<f64> = file_scores.iter().map(|f| f.0).collect();
    score_only_sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let cliff_cut = cliff_cutoff_index(score_only_sorted.iter().copied());
    if cliff_cut < file_scores.len() {
        let cutoff_score = score_only_sorted[cliff_cut - 1];
        file_scores.retain(|f| f.0 >= cutoff_score);
    }
    file_scores
        .iter()
        .map(|(score, file, layer, days, hot, top_symbol, why)| {
            let file_role = classify_file_role(file);
            let conflict_risk = dirty_files.contains(file.as_str());
            let conflict_detail = if include_conflict_detail && conflict_risk {
                explain_conflict_risk(file)
            } else {
                None
            };
            json!({
                "file": file,
                "layer": layer,
                "score": score,
                "last_touched_days": days,
                "hot": hot,
                "file_role": file_role,
                "conflict_risk": conflict_risk,
                "conflict_detail": conflict_detail,
                "top_symbol": top_symbol,
                "why": why,
            })
        })
        .collect()
}

/// Convenience: hoist git_dirty_files() for callers that want to share
/// the same set between finalize_file_scores and a downstream
/// stale_symbols scan. Just re-exports the underlying helper for API
/// discoverability.
pub fn dirty_files_for_change() -> HashSet<String> {
    git_dirty_files()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        Author, AuthorKind, LedgerEntry, LedgerKind, Position, Symbol, SymbolKind,
    };
    use std::collections::HashSet;

    fn sym(qname: &str, file: &str) -> Symbol {
        Symbol {
            symbol_id: format!("sym_{qname}"),
            symbol_fp: "fp".into(),
            qname: qname.into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: file.into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 5, col: 0 },
            signature: Some(format!("def {qname}()")),
            doc: None,
        }
    }

    #[test]
    fn file_score_floor_cliff_at_huge_drop() {
        // 40 → 5 is a 0.125 ratio — obvious cliff. Floor locks to
        // rank 1 (40); only rank 1 survives. (Reading 1.0.85 design:
        // cliff IS the dominant signal even with just 2 candidates.)
        let cands = vec![(40.0_f64, "a".into()), (5.0, "b".into())];
        let floor = file_score_floor(&cands);
        assert!((floor - 40.0).abs() < 1e-9, "cliff at rank 1; got {floor}");
    }

    #[test]
    fn file_score_floor_empty_input_is_zero() {
        assert_eq!(file_score_floor(&[]), 0.0);
    }

    #[test]
    fn file_score_floor_ratio_is_40pct() {
        assert!((FILE_SCORE_FLOOR_RATIO - 0.40).abs() < 1e-9);
    }

    #[test]
    fn file_score_floor_cliff_ratio_is_70pct() {
        // Pin the cliff threshold so future tunings are visible.
        assert!((CLIFF_RATIO_THRESHOLD - 0.70).abs() < 1e-9);
    }

    #[test]
    fn file_score_floor_cliff_cuts_at_cohort_boundary() {
        // ExampleFlow refinement #2 (1.0.85, deep half): the
        // literal field-eval case. Query "Drift Pad scheduler sync"
        // produced 42 / 31 / 19 / 18. Ratios:
        //   31/42 = 0.74 (above threshold, no cut)
        //   19/31 = 0.61 (below 0.70 → CLIFF, cut at 31)
        // Floor = 31. Files at 42 and 31 survive; 19 and 18 cut.
        let cands = vec![
            (42.0_f64, "drift_view_model".into()),
            (31.0, "drift_app".into()),
            (19.0, "project_view_model".into()),
            (18.0, "project_controller".into()),
        ];
        let floor = file_score_floor(&cands);
        assert!(
            (floor - 31.0).abs() < 1e-9,
            "cliff-aware floor must lock to pre-cliff rank score (31); got {floor}"
        );
    }

    #[test]
    fn file_score_floor_targeted_query_with_clear_winner() {
        // Top=80 dominates rank-2=20 (ratio 0.25 < 0.70). Cliff at
        // rank 2 → floor = 80. Only rank 1 survives.
        let cands = vec![
            (80.0_f64, "winner".into()),
            (20.0, "also_ran".into()),
            (15.0, "noise".into()),
        ];
        let floor = file_score_floor(&cands);
        assert!((floor - 80.0).abs() < 1e-9, "got {floor}");
    }

    #[test]
    fn file_score_floor_no_cliff_falls_back_to_relative() {
        // Truly smooth decay: all consecutive ratios > 0.70. No
        // cliff fires. Floor = 100 * 0.40 = 40. All ranks (100/
        // 90/80/75) above floor → all survive.
        //
        // Note: must end the candidate list before any pair drops
        // below 0.70 — otherwise the cliff pass fires. Real broad
        // queries with this smooth a decay are uncommon but possible
        // (4 near-equal options).
        let cands = vec![
            (100.0_f64, "a".into()),
            (90.0, "b".into()),
            (80.0, "c".into()),
            (75.0, "d".into()),
        ];
        let floor = file_score_floor(&cands);
        assert!((floor - 40.0).abs() < 1e-9, "got {floor}");
    }

    #[test]
    fn file_score_floor_cliff_after_smooth_decay_stops_there() {
        // 100 / 90 / 80 / 70 / 30 — smooth decay then cliff at
        // 70→30. Floor locks to 70 (last pre-cliff score), so
        // 100/90/80/70 survive, 30 cut. Cliff dominates the
        // relative floor (which would have been 40 and admitted 70
        // anyway, but also 60/50 if they existed).
        let cands = vec![
            (100.0_f64, "a".into()),
            (90.0, "b".into()),
            (80.0, "c".into()),
            (70.0, "d".into()),
            (30.0, "noise".into()),
        ];
        let floor = file_score_floor(&cands);
        assert!((floor - 70.0).abs() < 1e-9, "cliff floor=70; got {floor}");
    }

    #[test]
    fn cliff_cutoff_index_handles_short_lists() {
        assert_eq!(cliff_cutoff_index(std::iter::empty::<f64>()), 0);
        assert_eq!(cliff_cutoff_index(vec![42.0]), 1);
    }

    #[test]
    fn cliff_cutoff_index_matches_exampleflow_file_scores() {
        // 1.0.87 regression case: file-level scores 42/31/19/18.
        // 31/42 = 0.74 (no cut), 19/31 = 0.61 (cut at index 2).
        // Keep first 2 entries; cut starting at index 2.
        let cut = cliff_cutoff_index(vec![42.0, 31.0, 19.0, 18.0]);
        assert_eq!(cut, 2, "must keep top 2; got {cut}");
    }

    #[test]
    fn cliff_cutoff_index_smooth_keeps_everything() {
        // All consecutive ratios > 0.70 → no cliff → keep all.
        let cut = cliff_cutoff_index(vec![100.0, 90.0, 80.0, 75.0]);
        assert_eq!(cut, 4);
    }

    #[test]
    fn cliff_cutoff_index_zero_prev_bails_safely() {
        let cut = cliff_cutoff_index(vec![10.0, 0.0, 5.0]);
        assert_eq!(cut, 1, "cliff at zero prev → keep through index 0");
    }

    #[test]
    fn file_score_floor_cliff_at_first_pair_triggers() {
        // Cliff between rank 1 and rank 2 — relative floor would
        // still admit rank 2 (since rank 2 = 60% * top > 40% floor),
        // but cliff rule overrides.
        let cands = vec![
            (100.0_f64, "winner".into()),
            (60.0, "after_cliff".into()), // 0.60 ratio < 0.70 threshold
        ];
        let floor = file_score_floor(&cands);
        // Cliff at pair → floor = rank 1 = 100. Only rank 1 survives.
        assert!((floor - 100.0).abs() < 1e-9, "got {floor}");
    }

    #[test]
    fn push_file_score_admits_above_floor_and_unseen() {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let pushed = push_file_score(
            &mut out,
            &mut seen,
            &sym("a.b", "x.py"),
            &[],
            &["b".into()],
            20.0,
            "app",
            Some(3.0),
            true,
            10.0,
        );
        assert!(pushed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "x.py");
        assert_eq!(out[0].top_symbol, "a.b");
        assert!(out[0].why.starts_with("contains symbol") || !out[0].why.is_empty());
    }

    #[test]
    fn push_file_score_drops_below_floor() {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let pushed = push_file_score(
            &mut out,
            &mut seen,
            &sym("a.b", "x.py"),
            &[],
            &[],
            5.0,
            "app",
            None,
            false,
            10.0,
        );
        assert!(!pushed);
        assert!(out.is_empty());
    }

    #[test]
    fn push_file_score_dedups_by_file() {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        push_file_score(
            &mut out,
            &mut seen,
            &sym("a.b", "x.py"),
            &[],
            &[],
            20.0,
            "app",
            None,
            false,
            10.0,
        );
        let second = push_file_score(
            &mut out,
            &mut seen,
            &sym("a.c", "x.py"),
            &[],
            &[],
            20.0,
            "app",
            None,
            false,
            10.0,
        );
        assert!(!second, "duplicate file must not push a second entry");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn push_file_score_carries_ledger_reason_when_available() {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let entry = LedgerEntry::new(
            "sym_x",
            LedgerKind::Hazard,
            "watch out",
            Author {
                kind: AuthorKind::Agent,
                id: "t".into(),
            },
        );
        push_file_score(
            &mut out,
            &mut seen,
            &sym("a.b", "x.py"),
            std::slice::from_ref(&entry),
            &[],
            20.0,
            "app",
            None,
            false,
            0.0,
        );
        // The reason should be non-empty whether explain_match returns a
        // ledger-derived line or our "contains symbol" fallback.
        assert!(!out[0].why.is_empty());
    }
}
