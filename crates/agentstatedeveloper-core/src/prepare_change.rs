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

use serde::Serialize;

use crate::candidates::explain_match;
use crate::schema::{LedgerEntry, Symbol};

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
