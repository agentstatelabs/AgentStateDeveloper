//! Scratchpad store for ephemeral agent working notes.
//!
//! Scratch entries are stored locally at `/asd/v1/scratch/<scratch_id>`
//! and are **not** synced to the sidecar or subject to the policy gate.
//!
//! ## Lifecycle
//!
//! ```text
//! scratch_write  →  Draft
//! scratch_update →  Draft   (content updated, updated_at refreshed)
//! scratch_discard →  Discarded
//! scratch_promote → Promoted  (ledger entry created first; pass entry_id here)
//! scratch_clean  → permanent delete of Discarded/Promoted older than filter
//! ```
//!
//! ## Filtering
//!
//! [`ScratchFilter`] is OR-free: every `Some` field must match (AND semantics).
//! `exclude_expired` defaults to `true` — expired drafts are hidden unless
//! the caller opts in.

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use chrono::Utc;
use serde_json::Value;

use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::{ScratchEntry, ScratchStatus};

// ---------------------------------------------------------------------------
// Filter types
// ---------------------------------------------------------------------------

/// Predicate applied by [`ScratchStore::list_entries`].
/// All `Some` fields must match (AND semantics).
#[derive(Debug, Default, Clone)]
pub struct ScratchFilter {
    /// Restrict to entries scoped to this symbol_id.
    pub symbol_id: Option<String>,
    /// Restrict to entries with this workflow name.
    pub workflow: Option<String>,
    /// Restrict to entries written by this session/agent_id.
    pub session: Option<String>,
    /// Restrict to entries with this status. `None` means "any status".
    pub status: Option<ScratchStatus>,
    /// When `true` (default), hide entries whose `expires_at` is in the past.
    pub exclude_expired: bool,
}

impl ScratchFilter {
    /// A filter that returns only live (non-expired) draft entries.
    pub fn drafts() -> Self {
        Self {
            status: Some(ScratchStatus::Draft),
            exclude_expired: true,
            ..Default::default()
        }
    }

    /// A filter that returns everything (no restrictions).
    pub fn all() -> Self {
        Self {
            exclude_expired: false,
            ..Default::default()
        }
    }

    fn matches(&self, entry: &ScratchEntry) -> bool {
        if let Some(ref sid) = self.symbol_id {
            if entry.symbol_id.as_deref() != Some(sid.as_str()) {
                return false;
            }
        }
        if let Some(ref wf) = self.workflow {
            if entry.workflow.as_deref() != Some(wf.as_str()) {
                return false;
            }
        }
        if let Some(ref sess) = self.session {
            if &entry.session != sess {
                return false;
            }
        }
        if let Some(ref st) = self.status {
            if &entry.status != st {
                return false;
            }
        }
        if self.exclude_expired && entry.is_expired() {
            return false;
        }
        true
    }
}

/// Predicate used by [`ScratchStore::clean_entries`] to select entries
/// for permanent deletion.
#[derive(Debug, Default, Clone)]
pub struct CleanFilter {
    /// Only delete entries older than this duration (measured from `updated_at`).
    pub older_than: Option<chrono::Duration>,
    /// Delete entries whose status is in this list. Empty = all statuses.
    pub statuses: Vec<ScratchStatus>,
}

impl CleanFilter {
    fn matches(&self, entry: &ScratchEntry) -> bool {
        // Status check.
        if !self.statuses.is_empty() && !self.statuses.contains(&entry.status) {
            return false;
        }
        // Age check.
        if let Some(min_age) = self.older_than {
            let age = Utc::now() - entry.updated_at;
            if age < min_age {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Scratch-entry CRUD operations.
///
/// All methods are direct (no policy gate). Only `mark_promoted` is expected
/// to be called after the caller has already created a [`LedgerEntry`] via
/// the policy-gated ledger store.
pub trait ScratchStore {
    /// Persist a new scratch entry. Returns the stored entry (may have
    /// `updated_at` refreshed by the impl).
    fn write_entry(
        &self,
        ref_name: &str,
        entry: &ScratchEntry,
        agent_id: &str,
    ) -> Result<ScratchEntry>;

    /// Read a single entry by scratch_id.
    fn read_entry(&self, ref_name: &str, scratch_id: &str) -> Result<ScratchEntry>;

    /// Replace the content of an existing draft entry and refresh `updated_at`.
    fn update_entry(
        &self,
        ref_name: &str,
        scratch_id: &str,
        content: &str,
        agent_id: &str,
    ) -> Result<ScratchEntry>;

    /// List entries matching the filter, newest first.
    fn list_entries(&self, ref_name: &str, filter: &ScratchFilter) -> Result<Vec<ScratchEntry>>;

    /// Transition status to `Discarded`. No data is deleted; the entry remains
    /// until `clean_entries` removes it.
    fn discard_entry(&self, ref_name: &str, scratch_id: &str, agent_id: &str) -> Result<()>;

    /// Transition status to `Promoted` and record which ledger entry it became.
    /// The caller is responsible for creating the ledger entry first.
    fn mark_promoted(
        &self,
        ref_name: &str,
        scratch_id: &str,
        ledger_entry_id: &str,
        agent_id: &str,
    ) -> Result<ScratchEntry>;

    /// Permanently delete entries matching the filter. Returns the number
    /// of entries removed (0 when `dry_run` is true).
    fn clean_entries(&self, ref_name: &str, filter: &CleanFilter, dry_run: bool) -> Result<usize>;
}

// ---------------------------------------------------------------------------
// ASG-backed implementation
// ---------------------------------------------------------------------------

/// [`ScratchStore`] backed by an ASG [`Repository`].
pub struct AsgScratchStore<'a> {
    pub repo: &'a Repository,
}

impl<'a> ScratchStore for AsgScratchStore<'a> {
    fn write_entry(
        &self,
        ref_name: &str,
        entry: &ScratchEntry,
        agent_id: &str,
    ) -> Result<ScratchEntry> {
        let path = paths::scratch_entry_path(&entry.scratch_id);
        let val = serde_json::to_value(entry)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Checkpoint,
            format!("scratch: write {}", entry.scratch_id),
        );
        self.repo
            .set_json(ref_name, &path, &val, opts)
            .map_err(|e| AsdError::Other(e.to_string()))?;
        Ok(entry.clone())
    }

    fn read_entry(&self, ref_name: &str, scratch_id: &str) -> Result<ScratchEntry> {
        let path = paths::scratch_entry_path(scratch_id);
        let val = self
            .repo
            .get_json(ref_name, &path)
            .map_err(|_| AsdError::Other(format!("scratch entry not found: {scratch_id}")))?;
        serde_json::from_value(val).map_err(|e| AsdError::Other(e.to_string()))
    }

    fn update_entry(
        &self,
        ref_name: &str,
        scratch_id: &str,
        content: &str,
        agent_id: &str,
    ) -> Result<ScratchEntry> {
        let mut entry = self.read_entry(ref_name, scratch_id)?;
        if entry.status != ScratchStatus::Draft {
            return Err(AsdError::Other(format!(
                "scratch entry {} is {:?}, not Draft — cannot update",
                scratch_id, entry.status
            )));
        }
        entry.content = content.to_string();
        entry.updated_at = Utc::now();
        self.write_entry(ref_name, &entry, agent_id)
    }

    fn list_entries(&self, ref_name: &str, filter: &ScratchFilter) -> Result<Vec<ScratchEntry>> {
        let prefix = paths::scratch_root();
        let map = match self.repo.get_tree(ref_name, prefix) {
            Ok(Value::Object(m)) => m,
            Ok(_) | Err(_) => return Ok(Vec::new()),
        };
        let mut entries: Vec<ScratchEntry> = map
            .values()
            .filter_map(|v| serde_json::from_value::<ScratchEntry>(v.clone()).ok())
            .filter(|e| filter.matches(e))
            .collect();
        // Newest first.
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(entries)
    }

    fn discard_entry(&self, ref_name: &str, scratch_id: &str, agent_id: &str) -> Result<()> {
        let mut entry = self.read_entry(ref_name, scratch_id)?;
        entry.status = ScratchStatus::Discarded;
        entry.updated_at = Utc::now();
        self.write_entry(ref_name, &entry, agent_id)?;
        Ok(())
    }

    fn mark_promoted(
        &self,
        ref_name: &str,
        scratch_id: &str,
        ledger_entry_id: &str,
        agent_id: &str,
    ) -> Result<ScratchEntry> {
        let mut entry = self.read_entry(ref_name, scratch_id)?;
        entry.status = ScratchStatus::Promoted;
        entry.promoted_to = Some(ledger_entry_id.to_string());
        entry.updated_at = Utc::now();
        self.write_entry(ref_name, &entry, agent_id)
    }

    fn clean_entries(&self, ref_name: &str, filter: &CleanFilter, dry_run: bool) -> Result<usize> {
        let all_filter = ScratchFilter::all();
        let entries = self.list_entries(ref_name, &all_filter)?;
        let to_delete: Vec<ScratchEntry> =
            entries.into_iter().filter(|e| filter.matches(e)).collect();
        let count = to_delete.len();
        if dry_run {
            return Ok(count);
        }
        for entry in &to_delete {
            let path = paths::scratch_entry_path(&entry.scratch_id);
            let opts = CommitOptions::new(
                "asd-clean",
                IntentCategory::Refine,
                format!("scratch: clean {}", entry.scratch_id),
            );
            self.repo
                .delete(ref_name, &path, opts)
                .map_err(|e| AsdError::Other(e.to_string()))?;
        }
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ScratchStatus;

    fn make_entry(content: &str) -> ScratchEntry {
        ScratchEntry::new(content, "test-agent")
    }

    #[test]
    fn scratch_entry_new_defaults() {
        let e = make_entry("working hypothesis");
        assert_eq!(e.status, ScratchStatus::Draft);
        assert!(e.promoted_to.is_none());
        assert!(e.symbol_id.is_none());
        assert!(e.workflow.is_none());
        assert!(!e.is_expired());
        assert!(e.scratch_id.starts_with("scr_"));
    }

    #[test]
    fn scratch_entry_expired() {
        let mut e = make_entry("stale");
        e.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(e.is_expired());
    }

    #[test]
    fn scratch_filter_exclude_expired() {
        let mut e = make_entry("expired draft");
        e.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        let filter = ScratchFilter::drafts();
        assert!(!filter.matches(&e));
    }

    #[test]
    fn scratch_filter_workflow() {
        let mut e = make_entry("note");
        e.workflow = Some("tracing-sync-bug".to_string());
        let mut filter = ScratchFilter::drafts();
        filter.workflow = Some("tracing-sync-bug".to_string());
        assert!(filter.matches(&e));
        filter.workflow = Some("other-bug".to_string());
        assert!(!filter.matches(&e));
    }

    #[test]
    fn clean_filter_status_match() {
        let e = make_entry("note");
        // Draft entry should not match a filter for Discarded+Promoted.
        let filter = CleanFilter {
            statuses: vec![ScratchStatus::Discarded, ScratchStatus::Promoted],
            older_than: None,
        };
        assert!(!filter.matches(&e));
    }

    #[test]
    fn clean_filter_age_match() {
        let mut e = make_entry("old note");
        // Backdate updated_at by 2 hours.
        e.updated_at = Utc::now() - chrono::Duration::hours(2);
        let filter = CleanFilter {
            statuses: vec![],
            older_than: Some(chrono::Duration::hours(1)),
        };
        assert!(filter.matches(&e));

        // Too young:
        let filter2 = CleanFilter {
            statuses: vec![],
            older_than: Some(chrono::Duration::hours(3)),
        };
        assert!(!filter2.matches(&e));
    }
}
