//! State Trust Score — machine-readable rollup of ASD semantic state quality.
//!
//! Answers: "Can the agent rely on ASD's output for the current task?"
//!
//! # Signals
//!
//! | Signal            | Source                        | Max deduction |
//! |-------------------|-------------------------------|---------------|
//! | Index freshness   | FTS `last_indexed_at`         | −0.30         |
//! | Symbol count      | FTS `symbol_count`            | blocking      |
//! | Sidecar state     | `sidecar_lifecycle_state`     | −0.10         |
//! | Dirty files       | `git status --short`          | −0.05         |
//! | Ledger density    | Engine ledger tree            | −0.15         |
//! | Concept gaps      | Ownership without Concept     | −0.05         |
//!
//! # Levels
//!
//! - `high`    — score ≥ 0.85
//! - `medium`  — score ≥ 0.65
//! - `low`     — score ≥ 0.40
//! - `blocked` — score < 0.40 or any blocking signal fires

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Engine, AsgIndexStore, AsgLedgerStore, IndexStore, LedgerStore,
    SearchFtsDb, SidecarState, ASD_SCHEMA_VERSION,
    sidecar_lifecycle_state,
    schema::{LedgerKind, Symbol},
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Raw signal values gathered before scoring. Exposed for debugging and for
/// truth-drift history snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustSignals {
    /// Index age in fractional hours (−1.0 if unknown).
    pub age_hours: f64,
    /// Total indexed symbols.
    pub symbol_count: u64,
    /// Sidecar lifecycle state key: missing / present / hydrated / fresh-reset.
    pub sidecar_state: String,
    /// Number of source files with uncommitted changes.
    pub dirty_file_count: usize,
    /// Symbols that have an Ownership entry but no Concept entry.
    pub concept_gap_count: usize,
    /// Ledger entries per symbol (raw float).
    pub ledger_density: f64,
    /// ASD schema version string embedded in the binary.
    pub schema_version: String,
}

/// Distinguishes intentional clean-room state from unexpected data loss.
///
/// | `state`            | density | prior activity? | meaning                              |
/// |--------------------|---------|-----------------|--------------------------------------|
/// | `clean_room`       | 0.0     | no              | fresh init/clone — expected          |
/// | `unannotated`      | 0.0     | yes             | indexed but no annotations yet       |
/// | `degraded`         | >0 <5%  | yes             | regression — possible state loss     |
/// | `sparse_but_active`| 5–50%   | yes             | annotation in progress               |
/// | `populated`        | ≥50%    | —               | healthy, well-annotated              |
/// | `empty`            | —       | —               | no symbols — run `asd index`         |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataQuality {
    /// One of: "clean_room" | "sparse_but_active" | "populated" | "degraded" | "empty".
    pub state: String,
    /// Human-readable explanation of why this state was inferred.
    pub reason: String,
    /// True when sparse/low state is expected (e.g. after `asd init` or `git clone`).
    /// False when sparse state is anomalous given prior activity.
    pub expected_after_reset: bool,
}

/// Rolled-up state trust score for the current ASD workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustScore {
    /// Normalised trust value in [0.0, 1.0], rounded to 2 d.p.
    pub score: f64,
    /// Human label: "high" | "medium" | "low" | "blocked".
    pub level: String,
    /// Sorted list of short reason tokens (additive, both positive & negative).
    pub reasons: Vec<String>,
    /// True when one or more blocking conditions prevent reliable use.
    pub blocking: bool,
    /// Raw signal values (useful for history snapshots and debugging).
    pub signals: TrustSignals,
    /// Distinguishes intentional clean-room DB from unexpected state degradation.
    pub data_quality: DataQuality,
    /// Task categories this DB state is reliable enough to support.
    ///
    /// Values: `"search"` | `"impact"` | `"change-planning"` | `"audit"` | `"compliance"`
    pub safe_to_use_for: Vec<String>,
    /// Task categories where this DB state should NOT be relied upon.
    ///
    /// Values: same set as `safe_to_use_for`.
    pub avoid_for: Vec<String>,
}

impl TrustScore {
    /// Serialize to a compact `serde_json::Value` for embedding in CLI output.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

// ---------------------------------------------------------------------------
// Core computation
// ---------------------------------------------------------------------------

/// Compute a [`TrustScore`] for the ASD workspace at `db_path`.
///
/// Opens the FTS database and (if available) the Engine/ASG. All errors are
/// treated as degraded signals — this function never fails.
pub fn compute_trust_score(db_path: &Path) -> TrustScore {
    let project_root = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // -----------------------------------------------------------------------
    // 1. FTS: age + symbol count
    // -----------------------------------------------------------------------
    let (age_hours, symbol_count) = match SearchFtsDb::open(db_path) {
        Ok(fts) => {
            let sym = fts.symbol_count() as u64;
            let age = match fts.last_indexed_at() {
                Some(ts) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let secs = (now - ts).max(0) as f64;
                    secs / 3600.0
                }
                None => -1.0,
            };
            (age, sym)
        }
        Err(_) => (-1.0, 0),
    };

    // -----------------------------------------------------------------------
    // 2. Sidecar lifecycle
    // -----------------------------------------------------------------------
    let sidecar_state = sidecar_lifecycle_state(&project_root);
    let sidecar_key = match &sidecar_state {
        SidecarState::Missing    => "missing",
        SidecarState::Present    => "present",
        SidecarState::Hydrated   => "hydrated",
        SidecarState::FreshReset => "fresh-reset",
    };

    // -----------------------------------------------------------------------
    // 3. Dirty files — run from project root to match status.rs behaviour
    // -----------------------------------------------------------------------
    let dirty_file_count = count_dirty_source_files(&project_root);

    // -----------------------------------------------------------------------
    // 4. Ledger density + concept gaps (via Engine/ASG)
    // -----------------------------------------------------------------------
    let (ledger_density, concept_gap_count) = ledger_signals(db_path, symbol_count);

    // -----------------------------------------------------------------------
    // 5. Schema version
    // -----------------------------------------------------------------------
    let schema_version = ASD_SCHEMA_VERSION.to_string();

    let signals = TrustSignals {
        age_hours: (age_hours * 100.0).round() / 100.0,
        symbol_count,
        sidecar_state: sidecar_key.to_string(),
        dirty_file_count,
        concept_gap_count,
        ledger_density: (ledger_density * 1000.0).round() / 1000.0,
        schema_version,
    };

    // -----------------------------------------------------------------------
    // 6. Data quality: clean-room vs degraded vs healthy
    // -----------------------------------------------------------------------
    let data_quality = classify_data_quality(&signals, &project_root);

    score_signals(&signals, sidecar_key, &data_quality)
}

// ---------------------------------------------------------------------------
// Scoring algorithm
// ---------------------------------------------------------------------------

fn score_signals(sig: &TrustSignals, sidecar_key: &str, dq: &DataQuality) -> TrustScore {
    let mut score: f64 = 1.0;
    let mut reasons: Vec<String> = Vec::new();
    let mut blocking = false;

    // --- Blocking: empty index --------------------------------------------
    if sig.symbol_count == 0 {
        score = 0.0;
        blocking = true;
        reasons.push("empty_index".to_string());
        return finish(score, reasons, blocking, sig, dq);
    }

    // --- Index freshness --------------------------------------------------
    if sig.age_hours < 0.0 {
        // Unknown — treat as mildly stale (no timestamp in DB)
        score -= 0.10;
        reasons.push("index_age_unknown".to_string());
    } else if sig.age_hours < 1.0 {
        reasons.push("fresh_index".to_string());
    } else if sig.age_hours < 24.0 {
        score -= 0.15;
        reasons.push("stale_index".to_string());
    } else {
        score -= 0.30;
        reasons.push("very_stale_index".to_string());
    }

    // --- Sidecar ----------------------------------------------------------
    match sidecar_key {
        "hydrated" => {
            reasons.push("sidecar_hydrated".to_string());
        }
        "present" => {
            // Sidecar exists but not loaded into ASG — minor deduction.
            score -= 0.05;
            reasons.push("sidecar_present".to_string());
        }
        "fresh-reset" => {
            score -= 0.10;
            reasons.push("sidecar_fresh_reset".to_string());
        }
        _ => {
            // Missing
            score -= 0.10;
            reasons.push("sidecar_missing".to_string());
        }
    }

    // --- Dirty files (informational) --------------------------------------
    if sig.dirty_file_count > 0 {
        score -= 0.05;
        reasons.push("dirty_files_present".to_string());
    }

    // --- Ledger density ---------------------------------------------------
    // Clean-room DBs (fresh init / clone) are expected to be sparse.
    // Don't penalise them for it — the data_quality block already explains the state.
    let is_clean_room = dq.state == "clean_room";
    if sig.ledger_density < 0.05 {
        if is_clean_room {
            reasons.push("clean_room_sparse_ledger".to_string());
        } else {
            score -= 0.15;
            reasons.push("sparse_ledger".to_string());
        }
    } else if sig.ledger_density < 0.50 {
        score -= 0.05;
        reasons.push("sparse_ledger".to_string());
    } else {
        reasons.push("ledger_annotated".to_string());
    }

    // --- Concept gaps (as rate relative to symbol count) ------------------
    let gap_rate = sig.concept_gap_count as f64 / sig.symbol_count.max(1) as f64;
    if gap_rate > 0.10 {
        score -= 0.05;
        reasons.push("concept_gaps_present".to_string());
    }

    finish(score, reasons, blocking, sig, dq)
}

fn finish(
    raw_score: f64,
    mut reasons: Vec<String>,
    blocking: bool,
    sig: &TrustSignals,
    dq: &DataQuality,
) -> TrustScore {
    let score = (raw_score.clamp(0.0, 1.0) * 100.0).round() / 100.0;
    let is_blocked = blocking || score < 0.40;
    let level = if is_blocked {
        "blocked"
    } else if score >= 0.85 {
        "high"
    } else if score >= 0.65 {
        "medium"
    } else {
        "low"
    };
    reasons.sort();
    let (safe_to_use_for, avoid_for) = derive_use_guidance(level, dq, is_blocked);
    TrustScore {
        score,
        level: level.to_string(),
        reasons,
        blocking: is_blocked,
        signals: sig.clone(),
        data_quality: dq.clone(),
        safe_to_use_for,
        avoid_for,
    }
}

/// Derive actionable use-guidance from trust level and data-quality state.
///
/// Task categories:
/// - `search`           — symbol/code lookup, FTS queries
/// - `impact`           — change-impact analysis (callers/callees, trace)
/// - `change-planning`  — prepare-change, edit-file classification
/// - `audit`            — effect tracking, annotation review, blame
/// - `compliance`       — ledger-based compliance, evidence verification
fn derive_use_guidance(
    level: &str,
    dq: &DataQuality,
    blocking: bool,
) -> (Vec<String>, Vec<String>) {
    // All known categories in a stable order.
    let all: &[&str] = &["search", "impact", "change-planning", "audit", "compliance"];

    if blocking || dq.state == "empty" {
        // Nothing is reliable when the index is empty or blocking.
        return (vec![], all.iter().map(|s| s.to_string()).collect());
    }

    let (safe, avoid): (Vec<&str>, Vec<&str>) = match (dq.state.as_str(), level) {
        // Fresh workspace — index is good, ledger is empty by design.
        // Search / impact / change-planning work; audit/compliance need annotations.
        ("clean_room" | "unannotated", _) => (
            vec!["search", "impact", "change-planning"],
            vec!["audit", "compliance"],
        ),

        // Regression signal: low confidence across the board.
        // Only basic search is reasonable; everything else is risky.
        ("degraded", _) => (
            vec!["search"],
            vec!["impact", "change-planning", "audit", "compliance"],
        ),

        // Partially annotated — most tasks are fine, full compliance isn't yet.
        ("sparse_but_active", "high" | "medium") => (
            vec!["search", "impact", "change-planning", "audit"],
            vec!["compliance"],
        ),
        ("sparse_but_active", _) => (
            vec!["search", "impact", "change-planning"],
            vec!["audit", "compliance"],
        ),

        // Healthy, well-annotated workspace.
        ("populated", "high") => (
            vec!["search", "impact", "change-planning", "audit", "compliance"],
            vec![],
        ),
        ("populated", "medium") => (
            vec!["search", "impact", "change-planning", "audit"],
            vec!["compliance"],
        ),
        ("populated", _) => (
            vec!["search", "impact"],
            vec!["change-planning", "audit", "compliance"],
        ),

        // Unknown state — conservative defaults.
        _ => (
            vec!["search"],
            vec!["impact", "change-planning", "audit", "compliance"],
        ),
    };

    (
        safe.iter().map(|s| s.to_string()).collect(),
        avoid.iter().map(|s| s.to_string()).collect(),
    )
}

// ---------------------------------------------------------------------------
// Data-quality classification
// ---------------------------------------------------------------------------

/// Classify workspace data quality by combining ledger density with presence of
/// prior-activity markers in `.asd/` history files.
///
/// Prior-activity signals checked (in priority order):
/// - `.asd/index.log`               — written by `asd index` (most reliable)
/// - `.asd/workflow-sessions.jsonl` — written by `asd task-close`
/// - `.asd/probe-history.jsonl`     — written by `asd probe run`
/// - `.asd/trust-history.jsonl`     — written by `asd status --json` (≥3 entries)
///
/// Any of these present implies the workspace was actively used, not freshly cloned.
fn classify_data_quality(sig: &TrustSignals, project_root: &std::path::Path) -> DataQuality {
    if sig.symbol_count == 0 {
        return DataQuality {
            state: "empty".to_string(),
            reason: "no symbols indexed — run `asd index` to populate".to_string(),
            expected_after_reset: false,
        };
    }

    let dot_asd = project_root.join(".asd");
    let has_prior_activity = file_has_content(&dot_asd.join("index.log"))
        || file_has_content(&dot_asd.join("workflow-sessions.jsonl"))
        || file_has_content(&dot_asd.join("probe-history.jsonl"))
        || file_line_count(&dot_asd.join("trust-history.jsonl")) >= 3;

    if sig.ledger_density < 0.05 {
        if !has_prior_activity {
            // No index, no history → genuinely fresh.
            DataQuality {
                state: "clean_room".to_string(),
                reason: "fresh index with sparse ledger and no prior activity — \
                         expected after `asd init` or `git clone`".to_string(),
                expected_after_reset: true,
            }
        } else if sig.ledger_density == 0.0 {
            // Index exists, was run before, but no annotations were ever written.
            // This is a normal "unannotated" workspace — not alarming.
            DataQuality {
                state: "unannotated".to_string(),
                reason: "index built but no ledger annotations yet — \
                         run `asd annotate-commit` or `asd task-close` to start".to_string(),
                expected_after_reset: true,
            }
        } else {
            // Had some entries, now suspiciously few — genuine regression signal.
            DataQuality {
                state: "degraded".to_string(),
                reason: "sparse ledger despite prior task and probe activity — \
                         possible state loss or DB reset".to_string(),
                expected_after_reset: false,
            }
        }
    } else if sig.ledger_density < 0.50 {
        DataQuality {
            state: "sparse_but_active".to_string(),
            reason: "partially annotated workspace — annotation in progress".to_string(),
            expected_after_reset: false,
        }
    } else {
        DataQuality {
            state: "populated".to_string(),
            reason: "well-annotated workspace with healthy ledger density".to_string(),
            expected_after_reset: false,
        }
    }
}

/// Returns true if a file exists and contains at least one non-blank line.
fn file_has_content(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.lines().any(|l| !l.trim().is_empty()))
        .unwrap_or(false)
}

/// Returns the count of non-blank lines in a file (0 if absent or unreadable).
fn file_line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Count source files that git reports as modified or staged (excludes untracked).
fn count_dirty_source_files(project_root: &Path) -> usize {
    const SRC_EXTS: &[&str] = &[
        ".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go",
        ".kt", ".java", ".rb", ".cs", ".m", ".mm", ".cpp", ".c",
    ];
    let out = std::process::Command::new("git")
        .args(["status", "--short", "--untracked-files=no"])
        .current_dir(project_root)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.len() > 3 && SRC_EXTS.iter().any(|ext| l.ends_with(ext)))
                .count()
        }
        _ => 0,
    }
}

/// Derive ledger density and concept-gap count from the ASG.
/// Returns (density, gap_count). Falls back to (0.0, 0) if Engine can't open.
fn ledger_signals(db_path: &Path, symbol_count: u64) -> (f64, usize) {
    let engine = match Engine::open_sqlite(db_path) {
        Ok(e) => e,
        Err(_) => return (0.0, 0),
    };
    let index_store = AsgIndexStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);

    // Fast path: if the FTS index shows zero annotated symbols, skip the
    // per-symbol ledger walks entirely (unannotated / clean-room DBs).
    let annotated = engine.fts.as_ref()
        .map(|fts| fts.annotated_symbol_count())
        .unwrap_or(0);
    if annotated == 0 {
        return (0.0, 0);
    }

    // Load all symbols via the SQLite cache (avoids a full git tree walk).
    let syms: Vec<Symbol> = index_store.build_id_map(&engine).into_values().collect();

    if syms.is_empty() {
        return (0.0, 0);
    }

    let mut total_entries = 0usize;
    let mut concept_gaps = 0usize;

    for sym in &syms {
        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();
        total_entries += entries.len();
        let has_ownership = entries.iter().any(|e| e.kind == LedgerKind::Ownership);
        let has_concept   = entries.iter().any(|e| e.kind == LedgerKind::Concept);
        if has_ownership && !has_concept {
            concept_gaps += 1;
        }
    }

    let density = total_entries as f64 / syms.len().max(1) as f64;
    (density, concept_gaps)
}
