use std::collections::HashSet;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::engine::Engine;
use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::LedgerEntry;
use crate::search_fts::SearchFtsDb;

// ---------------------------------------------------------------------------
// RatifyOps — narrower trait for the approve/reject/withdraw operations.
// Lives in core so Engine can hold an Arc<dyn RatifyOps> without depending
// on the commercial ratify crate. agentstatedeveloper-ratify implements this.
// ---------------------------------------------------------------------------

/// The three ledger review operations. Implemented by `RatifyOpsImpl`
/// in `agentstatedeveloper-ratify` (Team tier). Methods take an explicit
/// `repo: &Repository` so the impl can be a zero-sized struct — no lifetime
/// or clone needed when stored as `Arc<dyn RatifyOps>` in `Engine`.
pub trait RatifyOps: Send + Sync {
    fn approve_entry(
        &self,
        repo: &Repository,
        ref_name: &str,
        entry_id: &str,
        approver_id: &str,
        approver_kind: &str,
        message: Option<&str>,
        agent_id: &str,
    ) -> Result<ApprovalOutcome>;

    fn reject_entry(
        &self,
        repo: &Repository,
        ref_name: &str,
        entry_id: &str,
        reviewer_id: &str,
        reviewer_kind: &str,
        reason: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome>;

    fn withdraw_entry(
        &self,
        repo: &Repository,
        ref_name: &str,
        entry_id: &str,
        author_id: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome>;
}

/// Outcome of [`LedgerStore::approve_entry`]. Carries the updated entry
/// and whether it was already approved (caller idempotency hint).
#[derive(Debug, Clone)]
pub struct ApprovalOutcome {
    pub entry: LedgerEntry,
    pub already_approved: bool,
}

/// Outcome of [`LedgerStore::reject_entry`] / `withdraw_entry`. Same
/// idempotency shape as [`ApprovalOutcome`].
#[derive(Debug, Clone)]
pub struct ReviewOutcome {
    pub entry: LedgerEntry,
    pub already_resolved: bool,
}

pub trait LedgerStore {
    fn append_entry(&self, ref_name: &str, entry: &LedgerEntry, agent_id: &str) -> Result<()>;

    /// List entries for a symbol, newest first. By default, entries that
    /// have been superseded by a later entry are filtered out.
    fn list_entries(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<LedgerEntry>> {
        let all = self.list_entries_with_superseded(ref_name, symbol_id)?;
        let superseded: HashSet<String> = all
            .iter()
            .flat_map(|e| e.supersedes.iter().cloned())
            .collect();
        Ok(all
            .into_iter()
            .filter(|e| !superseded.contains(&e.entry_id))
            .collect())
    }

    /// Full list including superseded entries.
    fn list_entries_with_superseded(
        &self,
        ref_name: &str,
        symbol_id: &str,
    ) -> Result<Vec<LedgerEntry>>;

    /// Approve an awaiting-approval entry. The OSS default returns a
    /// "commercial feature" error — the real implementation lives in
    /// the `agentstatedeveloper-ratify` crate (Team-tier).
    fn approve_entry(
        &self,
        _ref_name: &str,
        _entry_id: &str,
        _approver_id: &str,
        _approver_kind: &str,
        _message: Option<&str>,
        _agent_id: &str,
    ) -> Result<ApprovalOutcome> {
        Err(AsdError::Other(
            "ledger approve is a commercial feature (Team tier) — \
             install asd-pro to enable. See https://agentstatedeveloper.dev/pricing"
                .into(),
        ))
    }

    /// Reject an awaiting-approval entry. See `approve_entry` — OSS
    /// default errors; real impl in `agentstatedeveloper-ratify`.
    fn reject_entry(
        &self,
        _ref_name: &str,
        _entry_id: &str,
        _reviewer_id: &str,
        _reviewer_kind: &str,
        _reason: &str,
        _agent_id: &str,
    ) -> Result<ReviewOutcome> {
        Err(AsdError::Other(
            "ledger reject is a commercial feature (Team tier) — \
             install asd-pro to enable. See https://agentstatedeveloper.dev/pricing"
                .into(),
        ))
    }

    /// Withdraw an awaiting-approval entry. See `approve_entry` — OSS
    /// default errors; real impl in `agentstatedeveloper-ratify`.
    fn withdraw_entry(
        &self,
        _ref_name: &str,
        _entry_id: &str,
        _author_id: &str,
        _agent_id: &str,
    ) -> Result<ReviewOutcome> {
        Err(AsdError::Other(
            "ledger withdraw is a commercial feature (Team tier) — \
             install asd-pro to enable. See https://agentstatedeveloper.dev/pricing"
                .into(),
        ))
    }
}

pub struct AsgLedgerStore<'a> {
    pub repo: &'a Repository,
    /// Borrowed FTS connection from the owning `Engine`.  When `Some`,
    /// enables the SQLite write-through cache: `list_entries` reads from
    /// SQLite when populated, `append_entry` writes to SQLite after the git
    /// commit.  No `Connection::open` on every method call.
    pub fts: Option<&'a SearchFtsDb>,
}

impl<'a> AsgLedgerStore<'a> {
    /// Construct without SQLite caching (tests, internal engine calls).
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo, fts: None }
    }
    /// Convenience: borrow the FTS connection already open in `engine`.
    pub fn from_engine(engine: &'a Engine) -> Self {
        Self {
            repo: &engine.repo,
            fts: engine.fts.as_ref(),
        }
    }
}

impl<'a> LedgerStore for AsgLedgerStore<'a> {
    fn append_entry(&self, ref_name: &str, entry: &LedgerEntry, agent_id: &str) -> Result<()> {
        let path = paths::ledger_entry_path(&entry.symbol_id, &entry.entry_id);
        let value = serde_json::to_value(entry)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("ledger {} for {}", entry.kind.as_str(), entry.symbol_id),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;

        // Write reverse index: entry_id → symbol_id for O(1) find_entry.
        let idx_path = paths::ledger_entry_index_path(&entry.entry_id);
        let idx_val = serde_json::Value::String(entry.symbol_id.clone());
        let idx_opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("ledger-idx {}", entry.entry_id),
        );
        self.repo
            .set_json(ref_name, &idx_path, &idx_val, idx_opts)?;

        // Best-effort SQLite write-through; failures are non-fatal.
        if let Some(fts) = self.fts {
            let _ = fts.upsert_ledger_entry(entry, ref_name);
        }
        Ok(())
    }

    fn list_entries_with_superseded(
        &self,
        ref_name: &str,
        symbol_id: &str,
    ) -> Result<Vec<LedgerEntry>> {
        // Fast path: SQLite cache — zero git tree walks when populated.
        if let Some(fts) = self.fts {
            if fts.ledger_entry_count_for(symbol_id, ref_name) > 0 {
                if let Ok(entries) = fts.list_ledger_entries_for(symbol_id, ref_name) {
                    return Ok(entries);
                }
            }
        }

        // Authoritative git path — also runs when SQLite is empty (first run
        // after `git pull`).  Populates the cache as a side effect.
        let parent = paths::ledger_symbol_path(symbol_id);
        let json = match self.repo.get_json(ref_name, &parent) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        let mut entries = Vec::new();
        if let serde_json::Value::Object(map) = json {
            for (k, v) in map {
                match serde_json::from_value::<LedgerEntry>(v) {
                    Ok(e) => entries.push(e),
                    Err(err) => eprintln!(
                        "warning: skipping malformed ledger entry {}/{}: {}",
                        symbol_id, k, err
                    ),
                }
            }
        }
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Populate cache for next call — best effort.
        if !entries.is_empty() {
            if let Some(fts) = self.fts {
                for entry in &entries {
                    let _ = fts.upsert_ledger_entry(entry, ref_name);
                }
            }
        }

        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// Orphan detection
// ---------------------------------------------------------------------------

/// Walk every ledger entry and tag those whose `symbol_id` is no longer
/// present in the qname index as `"orphaned"` and `"orphaned-at:<timestamp>"`.
///
/// Returns the number of entries newly tagged. Already-orphaned entries
/// (already carrying the `"orphaned"` tag) are skipped. Runs in O(symbols +
/// ledger_entries) — safe to call at the end of `asd index` or from `health`.
pub fn detect_orphaned_entries(repo: &Repository, ref_name: &str, agent_id: &str) -> Result<usize> {
    use chrono::Utc;

    // Build set of all symbol_ids currently in the index.
    let qname_prefix = format!("{}/index/by-qname", paths::ASD_ROOT);
    let indexed: HashSet<String> = match repo.get_tree(ref_name, &qname_prefix) {
        Ok(serde_json::Value::Object(map)) => map
            .values()
            .filter_map(|v| v.get("symbol_id")?.as_str().map(|s| s.to_string()))
            .collect(),
        _ => HashSet::new(),
    };

    // Walk every ledger entry.
    let ledger_prefix = format!("{}/ledger", paths::ASD_ROOT);
    let ledger_tree = match repo.get_tree(ref_name, &ledger_prefix) {
        Ok(v) => v,
        Err(_) => return Ok(0),
    };

    let mut tagged = 0usize;
    let now_tag = format!("orphaned-at:{}", Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));

    if let serde_json::Value::Object(by_symbol) = ledger_tree {
        for (sym_id, per_symbol) in by_symbol {
            if indexed.contains(&sym_id) {
                continue;
            }
            if let serde_json::Value::Object(entries_map) = per_symbol {
                for (entry_id, v) in entries_map {
                    if let Ok(mut entry) = serde_json::from_value::<LedgerEntry>(v) {
                        if entry.tags.iter().any(|t| t == "orphaned") {
                            continue;
                        }
                        entry.tags.push("orphaned".to_string());
                        entry.tags.push(now_tag.clone());
                        let path = paths::ledger_entry_path(&sym_id, &entry_id);
                        let value = serde_json::to_value(&entry)?;
                        let opts = CommitOptions::new(
                            agent_id,
                            IntentCategory::Refine,
                            format!("tag orphaned entry {}", entry_id),
                        );
                        repo.set_json(ref_name, &path, &value, opts)
                            .map_err(|e| AsdError::Other(e.to_string()))?;
                        tagged += 1;
                    }
                }
            }
        }
    }
    Ok(tagged)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NopStore;

    impl LedgerStore for NopStore {
        fn append_entry(&self, _: &str, _: &LedgerEntry, _: &str) -> crate::error::Result<()> {
            Ok(())
        }
        fn list_entries_with_superseded(
            &self,
            _: &str,
            _: &str,
        ) -> crate::error::Result<Vec<LedgerEntry>> {
            Ok(vec![])
        }
    }

    #[test]
    fn oss_approve_returns_commercial_error_with_url() {
        let store = NopStore;
        let err = store
            .approve_entry("main", "e1", "alice", "human", None, "agent")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("commercial feature"), "msg: {}", msg);
        assert!(
            msg.contains("agentstatedeveloper.dev/pricing"),
            "msg: {}",
            msg
        );
    }

    #[test]
    fn oss_reject_returns_commercial_error_with_url() {
        let store = NopStore;
        let err = store
            .reject_entry("main", "e1", "alice", "human", "reason", "agent")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("commercial feature"), "msg: {}", msg);
        assert!(
            msg.contains("agentstatedeveloper.dev/pricing"),
            "msg: {}",
            msg
        );
    }

    #[test]
    fn oss_withdraw_returns_commercial_error_with_url() {
        let store = NopStore;
        let err = store
            .withdraw_entry("main", "e1", "alice", "agent")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("commercial feature"), "msg: {}", msg);
        assert!(
            msg.contains("agentstatedeveloper.dev/pricing"),
            "msg: {}",
            msg
        );
    }
}
