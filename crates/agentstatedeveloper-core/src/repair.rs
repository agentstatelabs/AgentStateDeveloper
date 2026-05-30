//! ASG state repair — scan for corruption and apply safe auto-corrections.
//!
//! Two entry points:
//!   - [`scan_asg`]: read-only; returns every detected issue.
//!   - [`repair_asg`]: calls `scan_asg`, then optionally applies auto-fixable
//!     corrections and re-scans to show what remains.
//!
//! ## Detected issue kinds
//!
//! | kind | severity | auto-fixable |
//! |---|---|---|
//! | `orphaned_effect` | warn | yes — deleted |
//! | `effect_id_mismatch` | error | no — ambiguous, needs manual resolution |
//! | `malformed_effect` | error | no — unknown cause |
//! | `orphaned_callee_ref` | warn | yes — ref removed from callees list |
//! | `orphaned_caller_ref` | warn | yes — ref removed from callers list |
//! | `orphaned_ledger` | warn | no — preserve audit trail |

use std::collections::HashSet;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::Result;
use crate::paths;
use crate::schema::{EffectDecl, ScratchEntry, ScratchStatus};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Severity level for a [`RepairIssue`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// Degraded quality but not blocking (e.g., stale ref).
    Warn,
    /// Data inconsistency that may cause incorrect query results (e.g., mismatched IDs).
    Error,
}

/// A single integrity issue detected in the ASG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairIssue {
    /// Machine-readable issue type (see module docs).
    pub kind: String,
    pub severity: IssueSeverity,
    /// ASG path that carries the corrupt/orphaned data.
    pub path: String,
    /// Human-readable description.
    pub detail: String,
    /// Whether `repair_asg(dry_run: false)` will correct this automatically.
    pub auto_fixable: bool,
}

/// Result returned by [`repair_asg`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairReport {
    /// Total issues detected before any fixes.
    pub issues_found: usize,
    /// Number of individual corrections applied (0 when `dry_run` is true).
    pub fixes_applied: usize,
    /// True when the caller passed `dry_run: true` — no writes were made.
    pub dry_run: bool,
    /// Issues remaining *after* fixes (same as initial list when `dry_run`).
    pub issues: Vec<RepairIssue>,
}

// ---------------------------------------------------------------------------
// Core scan
// ---------------------------------------------------------------------------

/// Read-only integrity scan.  Returns every detected [`RepairIssue`] without
/// modifying any ASG state.
pub fn scan_asg(repo: &Repository, ref_name: &str) -> Result<Vec<RepairIssue>> {
    let mut issues = Vec::new();

    let live_symbol_ids = build_live_symbol_ids(repo, ref_name);

    check_effects(repo, ref_name, &live_symbol_ids, &mut issues);
    check_callee_refs(repo, ref_name, &live_symbol_ids, &mut issues);
    check_caller_refs(repo, ref_name, &live_symbol_ids, &mut issues);
    check_ledger(repo, ref_name, &live_symbol_ids, &mut issues);
    check_scratch(repo, ref_name, &live_symbol_ids, &mut issues);

    Ok(issues)
}

// ---------------------------------------------------------------------------
// Repair (scan + optionally fix)
// ---------------------------------------------------------------------------

/// Scan the ASG for integrity issues and, when `dry_run` is false, apply all
/// auto-fixable corrections.
///
/// When `dry_run` is true the function behaves identically to [`scan_asg`]
/// (no writes, `fixes_applied` is 0).
pub fn repair_asg(
    repo: &Repository,
    ref_name: &str,
    agent_id: &str,
    dry_run: bool,
) -> Result<RepairReport> {
    let issues = scan_asg(repo, ref_name)?;
    let issues_found = issues.len();

    if dry_run {
        return Ok(RepairReport {
            issues_found,
            fixes_applied: 0,
            dry_run: true,
            issues,
        });
    }

    let mut fixes_applied = 0usize;

    // -----------------------------------------------------------------------
    // Fix 1: orphaned effect records → delete.
    // -----------------------------------------------------------------------
    for issue in issues.iter().filter(|i| i.kind == "orphaned_effect") {
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("repair: drop orphaned effect {}", issue.path),
        );
        match repo.delete(ref_name, &issue.path, opts) {
            Ok(_) => fixes_applied += 1,
            Err(e) => eprintln!("asd repair: delete {} failed: {}", issue.path, e),
        }
    }

    // -----------------------------------------------------------------------
    // Fix 2: orphaned callee refs — rebuild each callees list without orphans.
    // -----------------------------------------------------------------------
    let live_ids = build_live_symbol_ids(repo, ref_name);
    fixes_applied += rewrite_edge_lists(
        repo,
        ref_name,
        agent_id,
        &live_ids,
        "callees",
        &format!("{}/index/callees", paths::ASD_ROOT),
        |id| paths::callees_path(id),
    );

    // -----------------------------------------------------------------------
    // Fix 3: orphaned caller refs — same approach for the callers index.
    // -----------------------------------------------------------------------
    fixes_applied += rewrite_edge_lists(
        repo,
        ref_name,
        agent_id,
        &live_ids,
        "callers",
        &format!("{}/index/callers", paths::ASD_ROOT),
        |id| paths::callers_path(id),
    );

    // Re-scan: show what remains after fixes.
    let remaining = scan_asg(repo, ref_name)?;

    Ok(RepairReport {
        issues_found,
        fixes_applied,
        dry_run: false,
        issues: remaining,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_live_symbol_ids(repo: &Repository, ref_name: &str) -> HashSet<String> {
    let prefix = format!("{}/index/by-qname", paths::ASD_ROOT);
    match repo.get_tree(ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map
            .values()
            .filter_map(|v| v.get("symbol_id")?.as_str().map(|s| s.to_string()))
            .collect(),
        _ => HashSet::new(),
    }
}

fn check_effects(
    repo: &Repository,
    ref_name: &str,
    live: &HashSet<String>,
    issues: &mut Vec<RepairIssue>,
) {
    let prefix = format!("{}/effects", paths::ASD_ROOT);
    let map = match repo.get_tree(ref_name, &prefix) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return,
    };
    for (key, value) in &map {
        match serde_json::from_value::<EffectDecl>(value.clone()) {
            Err(e) => {
                issues.push(RepairIssue {
                    kind: "malformed_effect".to_string(),
                    severity: IssueSeverity::Error,
                    path: paths::effects_path(key),
                    detail: format!("blob for '{}' fails to deserialize: {}", key, e),
                    auto_fixable: false,
                });
            }
            Ok(decl) => {
                if decl.symbol_id != *key {
                    issues.push(RepairIssue {
                        kind: "effect_id_mismatch".to_string(),
                        severity: IssueSeverity::Error,
                        path: paths::effects_path(key),
                        detail: format!(
                            "key='{}' but internal symbol_id='{}' — likely bad merge conflict resolution",
                            key, decl.symbol_id
                        ),
                        auto_fixable: false,
                    });
                }
                if !live.contains(key) {
                    issues.push(RepairIssue {
                        kind: "orphaned_effect".to_string(),
                        severity: IssueSeverity::Warn,
                        path: paths::effects_path(key),
                        detail: format!(
                            "symbol '{}' not found in index; effect record is orphaned",
                            key
                        ),
                        auto_fixable: true,
                    });
                }
            }
        }
    }
}

fn check_callee_refs(
    repo: &Repository,
    ref_name: &str,
    live: &HashSet<String>,
    issues: &mut Vec<RepairIssue>,
) {
    let prefix = format!("{}/index/callees", paths::ASD_ROOT);
    let map = match repo.get_tree(ref_name, &prefix) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return,
    };
    for (caller_id, value) in &map {
        let callees = extract_str_array(value, "callees");
        for callee_id in callees {
            if !live.contains(&callee_id) {
                issues.push(RepairIssue {
                    kind: "orphaned_callee_ref".to_string(),
                    severity: IssueSeverity::Warn,
                    path: paths::callees_path(caller_id),
                    detail: format!(
                        "callee '{}' referenced by '{}' no longer exists in the index",
                        callee_id, caller_id
                    ),
                    auto_fixable: true,
                });
            }
        }
    }
}

fn check_caller_refs(
    repo: &Repository,
    ref_name: &str,
    live: &HashSet<String>,
    issues: &mut Vec<RepairIssue>,
) {
    let prefix = format!("{}/index/callers", paths::ASD_ROOT);
    let map = match repo.get_tree(ref_name, &prefix) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return,
    };
    for (callee_id, value) in &map {
        let callers = extract_str_array(value, "callers");
        for caller_id in callers {
            if !live.contains(&caller_id) {
                issues.push(RepairIssue {
                    kind: "orphaned_caller_ref".to_string(),
                    severity: IssueSeverity::Warn,
                    path: paths::callers_path(callee_id),
                    detail: format!(
                        "caller '{}' in call list for '{}' no longer exists in the index",
                        caller_id, callee_id
                    ),
                    auto_fixable: true,
                });
            }
        }
    }
}

fn check_ledger(
    repo: &Repository,
    ref_name: &str,
    live: &HashSet<String>,
    issues: &mut Vec<RepairIssue>,
) {
    let prefix = format!("{}/ledger", paths::ASD_ROOT);
    let map = match repo.get_tree(ref_name, &prefix) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return,
    };
    for (symbol_id, _) in &map {
        if !live.contains(symbol_id) {
            issues.push(RepairIssue {
                kind: "orphaned_ledger".to_string(),
                severity: IssueSeverity::Warn,
                path: paths::ledger_symbol_path(symbol_id),
                detail: format!(
                    "ledger entries for '{}' whose symbol is no longer in the index \
                     (preserved for audit trail — run `asd ledger find` to inspect)",
                    symbol_id
                ),
                auto_fixable: false,
            });
        }
    }
}

/// Walk an edge-list subtree (callees or callers), remove IDs that are no
/// longer live, and rewrite the list if anything changed.  Returns the total
/// number of individual ref drops performed.
fn rewrite_edge_lists(
    repo: &Repository,
    ref_name: &str,
    agent_id: &str,
    live: &HashSet<String>,
    field: &str,
    prefix: &str,
    path_fn: impl Fn(&str) -> String,
) -> usize {
    let map = match repo.get_tree(ref_name, prefix) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return 0,
    };
    let mut total_dropped = 0usize;
    for (owner_id, value) in &map {
        let all = extract_str_array(value, field);
        let clean: Vec<&String> = all.iter().filter(|id| live.contains(*id)).collect();
        let dropped = all.len() - clean.len();
        if dropped == 0 {
            continue;
        }
        let new_val = json!({ field: clean });
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!(
                "repair: dropped {} orphaned {} ref(s) from {}",
                dropped, field, owner_id
            ),
        );
        match repo.set_json(ref_name, &path_fn(owner_id), &new_val, opts) {
            Ok(_) => total_dropped += dropped,
            Err(e) => eprintln!(
                "asd repair: rewrite {} for {} failed: {}",
                field, owner_id, e
            ),
        }
    }
    total_dropped
}

/// Drop orphaned callee and caller refs whose target symbol_id no longer
/// exists in the live index.  Returns the total number of individual refs
/// dropped.  Called by [`hydrate_from_dir`] after a hydrate pass to clean
/// up any stale edges the sidecar may have carried.
pub fn drop_orphaned_edge_refs(repo: &Repository, ref_name: &str, agent_id: &str) -> Result<usize> {
    let live = build_live_symbol_ids(repo, ref_name);
    let mut dropped = 0usize;
    dropped += rewrite_edge_lists(
        repo,
        ref_name,
        agent_id,
        &live,
        "callees",
        &format!("{}/index/callees", paths::ASD_ROOT),
        |id| paths::callees_path(id),
    );
    dropped += rewrite_edge_lists(
        repo,
        ref_name,
        agent_id,
        &live,
        "callers",
        &format!("{}/index/callers", paths::ASD_ROOT),
        |id| paths::callers_path(id),
    );
    Ok(dropped)
}

/// Informational check: find draft scratch entries whose `symbol_id` is no
/// longer in the live index. Does not auto-delete; the agent may still need
/// the note. Emits a `Warn`-level, non-auto-fixable issue per orphan.
fn check_scratch(
    repo: &Repository,
    ref_name: &str,
    live: &HashSet<String>,
    issues: &mut Vec<RepairIssue>,
) {
    let prefix = paths::scratch_root();
    let map = match repo.get_tree(ref_name, prefix) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return,
    };
    for (scratch_id, value) in &map {
        let entry = match serde_json::from_value::<ScratchEntry>(value.clone()) {
            Ok(e) => e,
            Err(e) => {
                issues.push(RepairIssue {
                    kind: "malformed_scratch".to_string(),
                    severity: IssueSeverity::Error,
                    path: paths::scratch_entry_path(scratch_id),
                    detail: format!("scratch entry '{scratch_id}' fails to deserialize: {e}"),
                    auto_fixable: false,
                });
                continue;
            }
        };
        // Only warn about Draft entries — Promoted/Discarded may legitimately
        // outlive their symbol after cleanup cycles.
        if entry.status != ScratchStatus::Draft {
            continue;
        }
        if let Some(ref sym_id) = entry.symbol_id {
            if !live.contains(sym_id) {
                issues.push(RepairIssue {
                    kind: "orphaned_scratch".to_string(),
                    severity: IssueSeverity::Warn,
                    path: paths::scratch_entry_path(scratch_id),
                    detail: format!(
                        "scratch entry '{scratch_id}' references symbol '{sym_id}' \
                         which is no longer in the index (session: {}, workflow: {})",
                        entry.session,
                        entry.workflow.as_deref().unwrap_or("—"),
                    ),
                    auto_fixable: false,
                });
            }
        }
    }
}

fn extract_str_array(v: &serde_json::Value, field: &str) -> Vec<String> {
    v.get(field)
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
