use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use chrono::Utc;

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

pub trait LedgerStore {
    fn append_entry(&self, ref_name: &str, entry: &LedgerEntry, agent_id: &str) -> Result<()>;
    fn list_entries(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<LedgerEntry>>;

    /// Locate an entry by id anywhere in the ledger tree, flip its
    /// `awaiting-approval` tag to `approved`, record `approved-by:<id>`
    /// and `approved-at:<iso>` tags, and rewrite it at the same path.
    /// Returns an error if the entry has no `awaiting-approval` tag or
    /// if the policy-declared `approver:*` list disallows the approver.
    fn approve_entry(
        &self,
        _ref_name: &str,
        _entry_id: &str,
        _approver_id: &str,
        _approver_kind: &str,
        _agent_id: &str,
    ) -> Result<ApprovalOutcome> {
        Err(AsdError::Other(
            "approve_entry not implemented for this store".into(),
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

    fn list_entries(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<LedgerEntry>> {
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

    fn approve_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
        approver_id: &str,
        approver_kind: &str,
        agent_id: &str,
    ) -> Result<ApprovalOutcome> {
        // Scan the ledger tree to find (symbol_id, entry) pair matching entry_id.
        let (symbol_id, mut entry) = self
            .find_entry(ref_name, entry_id)?
            .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

        // Idempotency: already approved → return unchanged with a flag.
        if entry.tags.iter().any(|t| t == "approved") {
            return Ok(ApprovalOutcome {
                entry,
                already_approved: true,
            });
        }

        // Must be awaiting approval.
        if !entry.tags.iter().any(|t| t == "awaiting-approval") {
            return Err(AsdError::Other(format!(
                "entry {} is not awaiting approval",
                entry_id
            )));
        }

        // Check approver matches one of the policy-declared approvers.
        // Accept either "approver:<kind>" (e.g. approver:human) or
        // "approver:<id>". If no approver:* tags are present we allow
        // any approver (permissive default — shouldn't happen in
        // practice because awaiting-approval is always paired).
        let required_approvers: Vec<&str> = entry
            .tags
            .iter()
            .filter_map(|t| t.strip_prefix("approver:"))
            .collect();
        if !required_approvers.is_empty() {
            let kind_matches = required_approvers.iter().any(|r| *r == approver_kind);
            let id_matches = required_approvers.iter().any(|r| *r == approver_id);
            if !(kind_matches || id_matches) {
                return Err(AsdError::Other(format!(
                    "approver {} (kind={}) does not match any required approver: {:?}",
                    approver_id, approver_kind, required_approvers
                )));
            }
        }

        // Flip the tags.
        entry.tags.retain(|t| t != "awaiting-approval");
        entry.tags.push("approved".to_string());
        entry.tags.push(format!("approved-by:{}", approver_id));
        entry.tags.push(format!(
            "approved-at:{}",
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ));

        // Rewrite at the same path. Intent category = Refine so it lands
        // as an ordinary metadata update; a later ratification-aware
        // intent could be introduced when the policy crate ships.
        let path = paths::ledger_entry_path(&symbol_id, &entry.entry_id);
        let value = serde_json::to_value(&entry)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!(
                "approve ledger entry {} for {}",
                entry.entry_id, symbol_id
            ),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;

        Ok(ApprovalOutcome {
            entry,
            already_approved: false,
        })
    }
}

impl<'a> AsgLedgerStore<'a> {
    /// Walk the ledger tree and return the (symbol_id, entry) pair whose
    /// entry_id matches. O(N) across all ledger entries; acceptable at
    /// solo-dev scale.
    fn find_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
    ) -> Result<Option<(String, LedgerEntry)>> {
        let root = format!("{}/ledger", crate::paths::ASD_ROOT);
        let tree = match self.repo.get_tree(ref_name, &root) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let serde_json::Value::Object(by_symbol) = tree else {
            return Ok(None);
        };
        for (symbol_id, entries_json) in by_symbol {
            let serde_json::Value::Object(entries) = entries_json else {
                continue;
            };
            for (_, entry_json) in entries {
                if let Ok(entry) = serde_json::from_value::<LedgerEntry>(entry_json) {
                    if entry.entry_id == entry_id {
                        return Ok(Some((symbol_id, entry)));
                    }
                }
            }
        }
        Ok(None)
    }
}
