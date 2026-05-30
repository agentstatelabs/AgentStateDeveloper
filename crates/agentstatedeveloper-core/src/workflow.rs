//! Workflow Recipe Detection + Task Close Evidence Quality.
//!
//! Answers three questions at task-close time:
//!
//! 1. **What workflow actually happened?** — `WorkflowSummary::workflow_type`
//!    (full / annotate-and-close / test-and-close / close-only)
//!
//! 2. **Which recommended steps were skipped?** — `WorkflowSummary::missing_recommended_steps`
//!
//! 3. **How strong is the closure evidence?** — `EvidenceQuality::evidence_quality_score`
//!
//! # Workflow types
//!
//! | Type                | annotate_commit | tested |
//! |---------------------|-----------------|--------|
//! | `full`              | ✓               | ✓      |
//! | `annotate-and-close`| ✓               | —      |
//! | `test-and-close`    | —               | ✓      |
//! | `close-only`        | —               | —      |
//!
//! # Detection signals
//!
//! Steps are detected from ledger entries on the touched symbols **before** the
//! new proof entry is written, so task-close itself does not inflate the score.
//!
//! - `annotate_commit`   — any existing entry has a `commit:*` tag
//! - `invariant_checked` — any existing Invariant entry on a touched symbol
//! - `tested`            — `--validated` flag OR any existing ValidationScenario
//! - `task_closed`       — always true (we are running task-close right now)

use std::io::Write as IoWrite;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::schema::{LedgerEntry, LedgerKind};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-field evidence quality breakdown for the task closure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceQuality {
    /// At least one existing ledger entry carries a `commit:` tag (from annotate-commit).
    pub proof_has_commit: bool,
    /// `--validated` was passed OR evidence text mentions "test" / "spec" / "spec".
    pub proof_has_tests: bool,
    /// Changed files were resolved (touched_symbols_count > 0).
    pub proof_has_files: bool,
    /// `--validated` flag was explicitly set.
    pub proof_has_manual_validation: bool,
    /// `--proof` was supplied with non-default text.
    pub closure_summary_present: bool,
    /// At least one symbol was annotated (entries written > 0).
    pub touched_symbols_annotated: bool,
    /// Fraction of the six boolean fields that are true, rounded to 2 d.p.
    pub evidence_quality_score: f64,
}

impl EvidenceQuality {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Rolled-up workflow summary emitted in task-close JSON and persisted to
/// `.asd/workflow-sessions.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSummary {
    /// One of: "full" | "annotate-and-close" | "test-and-close" | "close-only".
    pub workflow_type: String,
    /// Steps that were detected from ledger evidence.
    pub steps_detected: Vec<String>,
    /// Recommended steps that were not detected.
    pub missing_recommended_steps: Vec<String>,
    /// Per-field evidence quality breakdown.
    pub evidence_quality: EvidenceQuality,
    /// CTX task id (empty string when absent).
    pub task_id: String,
    /// CTX plan id (empty string when absent).
    pub plan_id: String,
    /// RFC-3339 timestamp of this task-close invocation.
    pub closed_at: String,
    /// Number of symbols that received ledger entries.
    pub symbols_annotated: usize,
    /// Total ledger entries written in this task-close call.
    pub ledger_entries_written: usize,
    /// Data-quality state of the workspace at task-close time.
    ///
    /// Mirrors `DataQuality::state` from the trust score:
    /// `"clean_room"` | `"unannotated"` | `"sparse_but_active"` | `"populated"` | `"degraded"` | `"empty"`.
    ///
    /// Helps agents understand why `evidence_quality_score` is low on a fresh
    /// workspace: a score of 0.17 on an `unannotated` DB is expected, not a bug.
    #[serde(default)]
    pub db_state: String,
    /// Human-readable clarification when `db_state` may need explanation.
    ///
    /// Set for `clean_room` and `unannotated` (low evidence expected),
    /// and for `degraded` (possible state loss). Empty for healthy states.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub db_state_note: String,
}

impl WorkflowSummary {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

// ---------------------------------------------------------------------------
// Evidence quality scoring
// ---------------------------------------------------------------------------

/// Score task-closure evidence quality from available signals.
///
/// # Parameters
/// - `pre_existing_entries` — all ledger entries for touched symbols **before**
///   this task-close call (used to detect annotate_commit / tested).
/// - `was_validated` — `--validated` CLI flag.
/// - `evidence_text` — optional `--evidence` value.
/// - `proof_text` — the resolved proof text (may be default "task completed").
/// - `proof_was_explicit` — true when the caller supplied `--proof` explicitly.
/// - `touched_count` — number of resolved symbols.
/// - `entries_written` — number of entries written in this call.
pub fn score_evidence_quality(
    pre_existing_entries: &[LedgerEntry],
    was_validated: bool,
    evidence_text: Option<&str>,
    proof_was_explicit: bool,
    touched_count: usize,
    entries_written: usize,
) -> EvidenceQuality {
    let proof_has_commit = pre_existing_entries
        .iter()
        .any(|e| e.tags.iter().any(|t| t.starts_with("commit:")));

    let evidence_mentions_test = evidence_text
        .map(|ev| {
            let low = ev.to_lowercase();
            low.contains("test") || low.contains("spec") || low.contains("assert")
        })
        .unwrap_or(false);

    let has_existing_validation = pre_existing_entries
        .iter()
        .any(|e| e.kind == LedgerKind::ValidationScenario);

    let proof_has_tests = was_validated || evidence_mentions_test || has_existing_validation;
    let proof_has_files = touched_count > 0;
    let proof_has_manual_validation = was_validated;
    let closure_summary_present = proof_was_explicit;
    let touched_symbols_annotated = entries_written > 0;

    let bool_count = [
        proof_has_commit,
        proof_has_tests,
        proof_has_files,
        proof_has_manual_validation,
        closure_summary_present,
        touched_symbols_annotated,
    ]
    .iter()
    .filter(|&&b| b)
    .count();

    let evidence_quality_score = (bool_count as f64 / 6.0 * 100.0).round() / 100.0;

    EvidenceQuality {
        proof_has_commit,
        proof_has_tests,
        proof_has_files,
        proof_has_manual_validation,
        closure_summary_present,
        touched_symbols_annotated,
        evidence_quality_score,
    }
}

// ---------------------------------------------------------------------------
// Workflow recipe detection
// ---------------------------------------------------------------------------

/// Detect which workflow recipe was followed and which steps were skipped.
///
/// Returns `(workflow_type, steps_detected, missing_recommended_steps)`.
pub fn detect_workflow(
    pre_existing_entries: &[LedgerEntry],
    eq: &EvidenceQuality,
    has_invariants: bool,
) -> (String, Vec<String>, Vec<String>) {
    let mut steps: Vec<String> = Vec::new();

    // annotate_commit: any existing entry has a commit: tag.
    let did_annotate = eq.proof_has_commit;
    if did_annotate {
        steps.push("annotate_commit".to_string());
    }

    // invariant_checked: any existing Invariant entry on a touched symbol.
    let did_invariant = pre_existing_entries
        .iter()
        .any(|e| e.kind == LedgerKind::Invariant);
    if did_invariant {
        steps.push("invariant_checked".to_string());
    }

    // tested: validated flag OR existing ValidationScenario.
    let did_test = eq.proof_has_tests
        && (eq.proof_has_manual_validation
            || pre_existing_entries
                .iter()
                .any(|e| e.kind == LedgerKind::ValidationScenario));
    if did_test {
        steps.push("tested".to_string());
    }

    // task_closed is always true (we're here).
    steps.push("task_closed".to_string());

    // Classify workflow type.
    let workflow_type = match (did_annotate, did_test) {
        (true, true) => "full",
        (true, false) => "annotate-and-close",
        (false, true) => "test-and-close",
        (false, false) => "close-only",
    };

    // Missing recommended steps.
    let mut missing: Vec<String> = Vec::new();
    if !did_annotate {
        missing.push("annotate_commit".to_string());
    }
    if !did_test {
        missing.push("test_or_validate".to_string());
    }
    if has_invariants && !did_invariant {
        missing.push("check_invariants".to_string());
    }

    (workflow_type.to_string(), steps, missing)
}

// ---------------------------------------------------------------------------
// Workflow session persistence
// ---------------------------------------------------------------------------

/// Append a compact workflow session record to `.asd/workflow-sessions.jsonl`.
/// Silently skips on any I/O error so task-close never fails due to logging.
/// Caps the file at 500 lines (drops the oldest when over limit).
pub fn append_workflow_session(db_path: &Path, summary: &WorkflowSummary) {
    let dot_asd = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(".asd"),
        _ => return,
    };
    let _ = std::fs::create_dir_all(&dot_asd);
    let sessions_path = dot_asd.join("workflow-sessions.jsonl");

    let line = serde_json::to_string(&json!({
        "closed_at":   summary.closed_at,
        "task_id":     if summary.task_id.is_empty() { Value::Null } else { json!(summary.task_id) },
        "plan_id":     if summary.plan_id.is_empty() { Value::Null } else { json!(summary.plan_id) },
        "workflow_type":   summary.workflow_type,
        "steps_detected":  summary.steps_detected,
        "missing_steps":   summary.missing_recommended_steps,
        "evidence_score":  summary.evidence_quality.evidence_quality_score,
        "symbols_annotated": summary.symbols_annotated,
        "entries_written": summary.ledger_entries_written,
    }));
    let line = match line {
        Ok(l) => format!("{l}\n"),
        Err(_) => return,
    };

    // Read existing lines, append new one, cap at 500.
    let existing: Vec<String> = std::fs::read_to_string(&sessions_path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    let mut combined: Vec<String> = existing;
    combined.push(line.trim_end_matches('\n').to_string());
    if combined.len() > 500 {
        let excess = combined.len() - 500;
        combined.drain(0..excess);
    }

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&sessions_path)
    {
        let _ = f.write_all(combined.join("\n").as_bytes());
        let _ = f.write_all(b"\n");
    }
}
