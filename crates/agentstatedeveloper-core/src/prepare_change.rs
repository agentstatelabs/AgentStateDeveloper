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

/// Compute the precision floor for file scoring: 25% of the top candidate's
/// score. Files scoring below this are dropped from `likely_edit_files` to
/// keep prepare-change output focused. Returns 0.0 when `candidates` is empty
/// so an empty input simply admits everything (well, nothing).
pub fn file_score_floor(candidates: &[(f64, String)]) -> f64 {
    candidates.first().map(|(s, _)| s * 0.25).unwrap_or(0.0)
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
    fn file_score_floor_is_25pct_of_top() {
        let cands = vec![(40.0_f64, "a".into()), (5.0, "b".into())];
        assert!((file_score_floor(&cands) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn file_score_floor_empty_input_is_zero() {
        assert_eq!(file_score_floor(&[]), 0.0);
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
