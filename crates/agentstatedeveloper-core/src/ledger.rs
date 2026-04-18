use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use chrono::Utc;
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

    /// Full list including superseded entries. Default impl just calls
    /// list_entries — concrete stores should override for correctness.
    fn list_entries_with_superseded(
        &self,
        ref_name: &str,
        symbol_id: &str,
    ) -> Result<Vec<LedgerEntry>>;

    /// Locate an entry by id anywhere in the ledger tree, flip its
    /// `awaiting-approval` tag to `approved`, record `approved-by:<id>`
    /// and `approved-at:<iso>` tags, and rewrite it at the same path.
    /// When `message` is Some it's appended to the entry body.
    /// Returns an error if the entry has no `awaiting-approval` tag or
    /// if the policy-declared `approver:*` list disallows the approver.
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
            "approve_entry not implemented for this store".into(),
        ))
    }

    /// Reject an awaiting-approval entry. Flips `awaiting-approval` →
    /// `rejected`, adds `rejected-by:<id>` + `rejected-at:<iso>`. The
    /// `reason` is required and gets appended to `entry.body`.
    /// Authority check matches approve_entry: reviewer kind/id must
    /// match one of the entry's `approver:*` tags.
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
            "reject_entry not implemented for this store".into(),
        ))
    }

    /// Withdraw an awaiting-approval entry. Only the original author
    /// may withdraw (matched by `entry.author.id`). Flips
    /// `awaiting-approval` → `withdrawn`, adds `withdrawn-at:<iso>`.
    fn withdraw_entry(
        &self,
        _ref_name: &str,
        _entry_id: &str,
        _author_id: &str,
        _agent_id: &str,
    ) -> Result<ReviewOutcome> {
        Err(AsdError::Other(
            "withdraw_entry not implemented for this store".into(),
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

    fn approve_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
        approver_id: &str,
        approver_kind: &str,
        message: Option<&str>,
        agent_id: &str,
    ) -> Result<ApprovalOutcome> {
        let (symbol_id, mut entry) = self
            .find_entry(ref_name, entry_id)?
            .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

        // Idempotency.
        if entry.tags.iter().any(|t| t == "approved") {
            return Ok(ApprovalOutcome {
                entry,
                already_approved: true,
            });
        }
        // Can't approve a rejected or withdrawn entry.
        if let Some(bad) = entry
            .tags
            .iter()
            .find(|t| *t == "rejected" || *t == "withdrawn")
        {
            return Err(AsdError::Other(format!(
                "entry {} is already {} and cannot be approved",
                entry_id, bad
            )));
        }
        // Must be awaiting approval.
        if !entry.tags.iter().any(|t| t == "awaiting-approval") {
            return Err(AsdError::Other(format!(
                "entry {} is not awaiting approval",
                entry_id
            )));
        }

        Self::authorize_reviewer(&entry, approver_id, approver_kind)?;

        // Flip tags.
        entry.tags.retain(|t| t != "awaiting-approval");
        entry.tags.push("approved".to_string());
        entry.tags.push(format!("approved-by:{}", approver_id));
        entry.tags.push(format!("approved-at:{}", iso_now()));

        if let Some(msg) = message {
            append_to_body(&mut entry, "Approver note", approver_id, msg);
        }

        self.rewrite(ref_name, &symbol_id, &entry, agent_id, "approve")?;
        Ok(ApprovalOutcome {
            entry,
            already_approved: false,
        })
    }

    fn reject_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
        reviewer_id: &str,
        reviewer_kind: &str,
        reason: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome> {
        let (symbol_id, mut entry) = self
            .find_entry(ref_name, entry_id)?
            .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

        if entry.tags.iter().any(|t| t == "rejected") {
            return Ok(ReviewOutcome {
                entry,
                already_resolved: true,
            });
        }
        if let Some(bad) = entry
            .tags
            .iter()
            .find(|t| *t == "approved" || *t == "withdrawn")
        {
            return Err(AsdError::Other(format!(
                "entry {} is already {} and cannot be rejected",
                entry_id, bad
            )));
        }
        if !entry.tags.iter().any(|t| t == "awaiting-approval") {
            return Err(AsdError::Other(format!(
                "entry {} is not awaiting approval",
                entry_id
            )));
        }
        Self::authorize_reviewer(&entry, reviewer_id, reviewer_kind)?;
        if reason.trim().is_empty() {
            return Err(AsdError::Other(
                "reject requires a non-empty reason".into(),
            ));
        }

        entry.tags.retain(|t| t != "awaiting-approval");
        entry.tags.push("rejected".to_string());
        entry.tags.push(format!("rejected-by:{}", reviewer_id));
        entry.tags.push(format!("rejected-at:{}", iso_now()));

        append_to_body(&mut entry, "Rejection reason", reviewer_id, reason);

        self.rewrite(ref_name, &symbol_id, &entry, agent_id, "reject")?;
        Ok(ReviewOutcome {
            entry,
            already_resolved: false,
        })
    }

    fn withdraw_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
        author_id: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome> {
        let (symbol_id, mut entry) = self
            .find_entry(ref_name, entry_id)?
            .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

        if entry.tags.iter().any(|t| t == "withdrawn") {
            return Ok(ReviewOutcome {
                entry,
                already_resolved: true,
            });
        }
        if let Some(bad) = entry
            .tags
            .iter()
            .find(|t| *t == "approved" || *t == "rejected")
        {
            return Err(AsdError::Other(format!(
                "entry {} is already {} and cannot be withdrawn",
                entry_id, bad
            )));
        }
        if !entry.tags.iter().any(|t| t == "awaiting-approval") {
            return Err(AsdError::Other(format!(
                "entry {} is not awaiting approval",
                entry_id
            )));
        }
        if entry.author.id != author_id {
            return Err(AsdError::Other(format!(
                "withdraw requires the original author; entry author is {}",
                entry.author.id
            )));
        }

        entry.tags.retain(|t| t != "awaiting-approval");
        entry.tags.push("withdrawn".to_string());
        entry.tags.push(format!("withdrawn-at:{}", iso_now()));

        self.rewrite(ref_name, &symbol_id, &entry, agent_id, "withdraw")?;
        Ok(ReviewOutcome {
            entry,
            already_resolved: false,
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

    fn rewrite(
        &self,
        ref_name: &str,
        symbol_id: &str,
        entry: &LedgerEntry,
        agent_id: &str,
        op: &str,
    ) -> Result<()> {
        let path = paths::ledger_entry_path(symbol_id, &entry.entry_id);
        let value = serde_json::to_value(entry)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("{} ledger entry {} for {}", op, entry.entry_id, symbol_id),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;
        Ok(())
    }

    /// Enforce the approver-match rule shared by approve + reject.
    /// Reviewer id OR kind must match one of the entry's `approver:*`
    /// tags. When there are no `approver:*` tags (shouldn't normally
    /// happen for awaiting-approval entries) the call is permitted.
    fn authorize_reviewer(
        entry: &LedgerEntry,
        reviewer_id: &str,
        reviewer_kind: &str,
    ) -> Result<()> {
        let required: Vec<&str> = entry
            .tags
            .iter()
            .filter_map(|t| t.strip_prefix("approver:"))
            .collect();
        if required.is_empty() {
            return Ok(());
        }
        let ok = required
            .iter()
            .any(|r| *r == reviewer_kind || *r == reviewer_id);
        if ok {
            Ok(())
        } else {
            Err(AsdError::Other(format!(
                "reviewer {} (kind={}) does not match any required approver: {:?}",
                reviewer_id, reviewer_kind, required
            )))
        }
    }
}

fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Append a labeled section to `entry.body`. Preserves prior body
/// content; separates sections with `---`.
fn append_to_body(entry: &mut LedgerEntry, label: &str, author: &str, message: &str) {
    let section = format!("\n\n--- {} by {} ---\n{}", label, author, message);
    match &mut entry.body {
        Some(b) => b.push_str(&section),
        None => entry.body = Some(section.trim_start().to_string()),
    }
}
