use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use std::collections::HashSet;

use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::LedgerEntry;

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
            "ledger approve is a commercial feature — install asd-pro (Team tier) to enable".into(),
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
            "ledger reject is a commercial feature — install asd-pro (Team tier) to enable".into(),
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
            "ledger withdraw is a commercial feature — install asd-pro (Team tier) to enable"
                .into(),
        ))
    }
}

pub struct AsgLedgerStore<'a> {
    pub repo: &'a Repository,
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
        Ok(())
    }

    fn list_entries_with_superseded(
        &self,
        ref_name: &str,
        symbol_id: &str,
    ) -> Result<Vec<LedgerEntry>> {
        let parent = paths::ledger_symbol_path(symbol_id);
        let json = match self.repo.get_json(ref_name, &parent) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        let mut entries = Vec::new();
        if let serde_json::Value::Object(map) = json {
            for (_k, v) in map {
                if let Ok(e) = serde_json::from_value::<LedgerEntry>(v) {
                    entries.push(e);
                }
            }
        }
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(entries)
    }
}
