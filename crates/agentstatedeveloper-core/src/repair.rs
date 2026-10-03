//! ASG state repair — scan for corruption and apply safe auto-corrections.
//!
//! Three entry points:
//!   - [`scan_asg`]: read-only; returns every detected ASG-side issue.
//!   - [`scan_sidecar`]: read-only; flags files/dirs in `.asd/conclusions/`
//!     that violate the Plan K sidecar inclusion rule (Plan K t-010).
//!   - [`repair_asg`]: calls `scan_asg`, then optionally applies auto-fixable
//!     corrections and re-scans to show what remains.
//!
//! ## Detected issue kinds
//!
//! | kind | severity | auto-fixable |
//! |---|---|---|
//! | `orphaned_effect` | warn | yes — deleted, unless it holds runtime or trace evidence |
//! | `effect_id_mismatch` | error | no — ambiguous, needs manual resolution |
//! | `malformed_effect` | error | no — unknown cause |
//! | `orphaned_callee_ref` | warn | yes — ref removed from callees list |
//! | `orphaned_caller_ref` | warn | yes — ref removed from callers list |
//! | `orphaned_ledger` | warn | no — preserve audit trail |
//! | `sidecar_unknown_file` | warn | no — manual review (might be intentional) |
//! | `sidecar_unknown_dir` | warn | no — manual review |
//! | `sidecar_wrong_extension` | warn | no — manual review |

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

    // Every fix lands in one commit. A commit per dropped effect or rewritten
    // edge list stored a fresh copy of the enclosing map each time — on a
    // store with 54,685 orphaned effects, roughly 100 GB. The
    // subtrees are cleaned in memory on top of the state the speculation
    // forked from and written once each.
    let (spec, fork) = crate::subtree::speculate_at_head(repo, ref_name, "asd-repair")?;
    let staged = (|| -> Result<(usize, usize, usize)> {
        let live = build_live_symbol_ids(repo, &fork);

        // Fix 1: orphaned effect records → delete, keeping evidence.
        let effects_root = format!("{}/effects", paths::ASD_ROOT);
        let mut effects = crate::subtree::read_seed(repo, &fork, &effects_root)?;
        let before = effects.len();
        effects
            .retain(|id, value| live.contains(id) || crate::effects::value_carries_evidence(value));
        let effects_dropped = before - effects.len();
        if effects_dropped > 0 {
            repo.spec_set_json(spec, &effects_root, &serde_json::Value::Object(effects))
                .map_err(|e| crate::error::AsdError::Other(e.to_string()))?;
        }

        // Fixes 2 and 3: orphaned callee and caller refs.
        let callees = stage_edge_lists(repo, spec, &fork, &live, "callees")?;
        let callers = stage_edge_lists(repo, spec, &fork, &live, "callers")?;
        Ok((effects_dropped, callees, callers))
    })();
    let fixes_applied = match staged {
        Ok((0, 0, 0)) => {
            let _ = repo.discard_speculation(spec);
            0
        }
        Ok((effects, callees, callers)) => {
            let opts = CommitOptions::new(
                agent_id,
                IntentCategory::Refine,
                format!(
                    "repair: drop {effects} orphaned effect(s), {callees} orphaned callee ref(s), \
                     {callers} orphaned caller ref(s)"
                ),
            );
            repo.commit_speculation(spec, opts)
                .map_err(|e| crate::error::AsdError::Other(e.to_string()))?;
            effects + callees + callers
        }
        Err(e) => {
            let _ = repo.discard_speculation(spec);
            return Err(e);
        }
    };

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
                    let evidence = crate::effects::carries_evidence(&decl);
                    issues.push(RepairIssue {
                        kind: "orphaned_effect".to_string(),
                        severity: IssueSeverity::Warn,
                        path: paths::effects_path(key),
                        detail: if evidence {
                            format!(
                                "symbol '{key}' not found in index; effect record is orphaned \
                                 but holds runtime or trace evidence, so it is kept"
                            )
                        } else {
                            format!("symbol '{key}' not found in index; effect record is orphaned")
                        },
                        auto_fixable: !evidence,
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

/// Remove ids that are no longer live from every list in the `field` edge
/// subtree (`callees` or `callers`) as the speculation forked it, and write
/// the subtree once if anything changed. Returns the number of refs dropped.
fn stage_edge_lists(
    repo: &Repository,
    spec: agentstategraph::SpecHandle,
    fork: &str,
    live: &HashSet<String>,
    field: &str,
) -> Result<usize> {
    let prefix = format!("{}/index/{field}", paths::ASD_ROOT);
    let mut map = crate::subtree::read_seed(repo, fork, &prefix)?;
    let mut total_dropped = 0usize;
    for value in map.values_mut() {
        let all = extract_str_array(value, field);
        let clean: Vec<&String> = all.iter().filter(|id| live.contains(*id)).collect();
        let dropped = all.len() - clean.len();
        if dropped > 0 {
            *value = json!({ field: clean });
            total_dropped += dropped;
        }
    }
    if total_dropped > 0 {
        repo.spec_set_json(spec, &prefix, &serde_json::Value::Object(map))
            .map_err(|e| crate::error::AsdError::Other(e.to_string()))?;
    }
    Ok(total_dropped)
}

/// Drop orphaned callee and caller refs whose target symbol_id no longer
/// exists in the live index, in one commit.  Returns the total number of
/// individual refs dropped.  Called by [`hydrate_from_dir`] after a hydrate
/// pass to clean up any stale edges the sidecar may have carried.
pub fn drop_orphaned_edge_refs(repo: &Repository, ref_name: &str, agent_id: &str) -> Result<usize> {
    let (spec, fork) = crate::subtree::speculate_at_head(repo, ref_name, "asd-drop-edge-refs")?;
    let staged = (|| -> Result<usize> {
        let live = build_live_symbol_ids(repo, &fork);
        Ok(stage_edge_lists(repo, spec, &fork, &live, "callees")?
            + stage_edge_lists(repo, spec, &fork, &live, "callers")?)
    })();
    match staged {
        Ok(0) => {
            let _ = repo.discard_speculation(spec);
            Ok(0)
        }
        Ok(dropped) => {
            let opts = CommitOptions::new(
                agent_id,
                IntentCategory::Refine,
                format!("repair: drop {dropped} orphaned edge ref(s)"),
            );
            repo.commit_speculation(spec, opts)
                .map_err(|e| crate::error::AsdError::Other(e.to_string()))?;
            Ok(dropped)
        }
        Err(e) => {
            let _ = repo.discard_speculation(spec);
            Err(e)
        }
    }
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

// ---------------------------------------------------------------------------
// Plan K t-010: sidecar inclusion-rule lint
// ---------------------------------------------------------------------------

/// Scan `.asd/conclusions/` for files and directories that don't match
/// the Plan K sidecar inclusion rule:
///
/// > The committed sidecar carries **judgment** — anything an agent
/// > or human had to decide, classify, hypothesize, approve, or
/// > otherwise commit mental effort to. Anything mechanically
/// > derivable from source stays in the regenerable SQLite cache and
/// > is gitignored.
///
/// Concretely, `.asd/conclusions/` should ONLY contain:
///   - `<stem>.jsonl` for the default Class layout, where `<stem>`
///     is one of the 7 `ConclusionClass::filename_stem()` values
///   - `<stem>/<anything>.jsonl` for per-package layout (Plan K t-007)
///
/// Anything else — `.md` notes, `.json` dumps, `effects.jsonl`
/// (regenerable!), random subdirs — is leakage. We warn so a
/// contributor or `pre-commit` hook can review before the noise
/// lands in git.
///
/// Read-only; never deletes. Returns an empty list when the directory
/// doesn't exist (fresh project — nothing to lint yet).
pub fn scan_sidecar(conclusions_dir: &std::path::Path) -> Vec<RepairIssue> {
    use crate::schema::ConclusionClass;
    let mut issues = Vec::new();
    if !conclusions_dir.is_dir() {
        return issues;
    }
    let known_stems: std::collections::HashSet<&'static str> = ConclusionClass::all()
        .iter()
        .map(|c| c.filename_stem())
        .collect();

    let entries = match std::fs::read_dir(conclusions_dir) {
        Ok(e) => e,
        Err(_) => return issues,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(conclusions_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if path.is_dir() {
            // Per-package layout subdirectory — name must match a
            // known ConclusionClass stem.
            if !known_stems.contains(name.as_str()) {
                issues.push(RepairIssue {
                    kind: "sidecar_unknown_dir".into(),
                    severity: IssueSeverity::Warn,
                    path: format!(".asd/conclusions/{rel}"),
                    detail: format!(
                        "subdirectory `{name}` is not a known ConclusionClass; \
                         expected one of: {known_list}. If you intended a \
                         per-package layout (Plan K t-007), the subdir name \
                         must match a class stem.",
                        known_list = sorted_known_stems_list(&known_stems),
                    ),
                    auto_fixable: false,
                });
                continue;
            }
            // Inside a valid class subdir, every file must be *.jsonl.
            if let Ok(inner) = std::fs::read_dir(&path) {
                for sub in inner.flatten() {
                    let sub_path = sub.path();
                    let sub_rel = sub_path
                        .strip_prefix(conclusions_dir)
                        .unwrap_or(&sub_path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if sub_path.is_file()
                        && sub_path.extension().and_then(|s| s.to_str()) != Some("jsonl")
                    {
                        issues.push(RepairIssue {
                            kind: "sidecar_wrong_extension".into(),
                            severity: IssueSeverity::Warn,
                            path: format!(".asd/conclusions/{sub_rel}"),
                            detail: format!(
                                "file is not `.jsonl`; per-package shards must have \
                                 the `.jsonl` extension to be picked up by import."
                            ),
                            auto_fixable: false,
                        });
                    }
                }
            }
        } else if path.is_file() {
            // Top-level file — must be `<known-stem>.jsonl`.
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path.extension().and_then(|s| s.to_str());
            if ext != Some("jsonl") {
                issues.push(RepairIssue {
                    kind: "sidecar_wrong_extension".into(),
                    severity: IssueSeverity::Warn,
                    path: format!(".asd/conclusions/{rel}"),
                    detail: format!(
                        "file is not `.jsonl`; the committed sidecar should \
                         carry only JSONL ledger projections. Move other \
                         artifacts out of `.asd/conclusions/` — derived caches \
                         belong in `.asd/cache/` (gitignored)."
                    ),
                    auto_fixable: false,
                });
            } else if !known_stems.contains(stem.as_str()) {
                issues.push(RepairIssue {
                    kind: "sidecar_unknown_file".into(),
                    severity: IssueSeverity::Warn,
                    path: format!(".asd/conclusions/{rel}"),
                    detail: format!(
                        "file `{name}` doesn't match a known ConclusionClass \
                         (one of: {known_list}). If a new judgment class \
                         shipped, add the ConclusionClass variant. If this is \
                         a regenerable artifact (e.g. effects, FTS), move it \
                         to `.asd/cache/` (gitignored).",
                        known_list = sorted_known_stems_list(&known_stems),
                    ),
                    auto_fixable: false,
                });
            }
        }
    }
    issues
}

fn sorted_known_stems_list(stems: &std::collections::HashSet<&str>) -> String {
    let mut v: Vec<&str> = stems.iter().copied().collect();
    v.sort();
    v.join(", ")
}

#[cfg(test)]
mod sidecar_lint_tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(p: &std::path::Path) {
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn empty_or_missing_dir_returns_no_issues() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(scan_sidecar(&missing).is_empty());

        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(scan_sidecar(&empty).is_empty());
    }

    #[test]
    fn known_class_files_pass_clean() {
        let tmp = tempdir().unwrap();
        for stem in [
            "decisions",
            "classifications",
            "mappings",
            "hazards",
            "recipes",
            "followups",
            "thinking",
        ] {
            touch(&tmp.path().join(format!("{stem}.jsonl")));
        }
        let issues = scan_sidecar(tmp.path());
        assert!(
            issues.is_empty(),
            "all-known-stem files must lint clean; got: {issues:?}"
        );
    }

    #[test]
    fn flags_unknown_top_level_file() {
        let tmp = tempdir().unwrap();
        touch(&tmp.path().join("effects.jsonl")); // not a ConclusionClass
        let issues = scan_sidecar(tmp.path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "sidecar_unknown_file");
        assert!(issues[0].path.contains("effects.jsonl"));
    }

    #[test]
    fn flags_wrong_extension_at_top_level() {
        let tmp = tempdir().unwrap();
        touch(&tmp.path().join("notes.md"));
        let issues = scan_sidecar(tmp.path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "sidecar_wrong_extension");
    }

    #[test]
    fn flags_unknown_subdir_name() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("backups")).unwrap();
        let issues = scan_sidecar(tmp.path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "sidecar_unknown_dir");
    }

    #[test]
    fn accepts_known_class_subdir_with_jsonl_shards() {
        // Per-package layout: `.asd/conclusions/decisions/crates--core.jsonl`
        let tmp = tempdir().unwrap();
        let class_dir = tmp.path().join("decisions");
        std::fs::create_dir(&class_dir).unwrap();
        touch(&class_dir.join("crates--core.jsonl"));
        touch(&class_dir.join("crates--cli.jsonl"));
        let issues = scan_sidecar(tmp.path());
        assert!(
            issues.is_empty(),
            "known class subdir with .jsonl shards must lint clean; got: {issues:?}"
        );
    }

    #[test]
    fn flags_wrong_extension_inside_class_subdir() {
        let tmp = tempdir().unwrap();
        let class_dir = tmp.path().join("decisions");
        std::fs::create_dir(&class_dir).unwrap();
        touch(&class_dir.join("crates--core.json")); // wrong ext
        let issues = scan_sidecar(tmp.path());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, "sidecar_wrong_extension");
        assert!(issues[0].path.contains("decisions/"));
    }
}
