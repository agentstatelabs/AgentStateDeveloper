//! `agentstatedeveloper-ratify` — Team-tier ledger ratification workflow.
//!
//! Provides two public types:
//!
//! - [`RatifyLedgerStore`]: a full [`LedgerStore`] backed by a borrowed
//!   [`Repository`]. For use in local scopes (e.g. tests, one-shot CLI calls).
//! - [`RatifyOpsImpl`]: a zero-sized struct implementing [`RatifyOps`] that
//!   can be boxed as `Arc<dyn RatifyOps>` and installed in `Engine` without
//!   any lifetime or clone constraints. `asd-pro` does this at startup.

use std::collections::HashSet;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use chrono::Utc;

use agentstatedeveloper_core::{
    AsdError,
    error::Result,
    ledger::{ApprovalOutcome, LedgerStore, RatifyOps, ReviewOutcome},
    paths,
    schema::LedgerEntry,
};

// ---------------------------------------------------------------------------
// RatifyLedgerStore — borrowed, implements LedgerStore
// ---------------------------------------------------------------------------

pub struct RatifyLedgerStore<'a> {
    pub repo: &'a Repository,
}

impl<'a> RatifyLedgerStore<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo }
    }
}

impl<'a> LedgerStore for RatifyLedgerStore<'a> {
    fn append_entry(&self, ref_name: &str, entry: &LedgerEntry, agent_id: &str) -> Result<()> {
        append_entry_impl(self.repo, ref_name, entry, agent_id)
    }

    fn list_entries_with_superseded(
        &self,
        ref_name: &str,
        symbol_id: &str,
    ) -> Result<Vec<LedgerEntry>> {
        list_entries_impl(self.repo, ref_name, symbol_id)
    }

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

    fn approve_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
        approver_id: &str,
        approver_kind: &str,
        message: Option<&str>,
        agent_id: &str,
    ) -> Result<ApprovalOutcome> {
        approve_impl(self.repo, ref_name, entry_id, approver_id, approver_kind, message, agent_id)
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
        reject_impl(self.repo, ref_name, entry_id, reviewer_id, reviewer_kind, reason, agent_id)
    }

    fn withdraw_entry(
        &self,
        ref_name: &str,
        entry_id: &str,
        author_id: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome> {
        withdraw_impl(self.repo, ref_name, entry_id, author_id, agent_id)
    }
}

// ---------------------------------------------------------------------------
// RatifyOpsImpl — zero-sized, implements RatifyOps for Engine storage
// ---------------------------------------------------------------------------

/// Zero-sized implementor of [`RatifyOps`].
///
/// Install via `engine.set_ratify_ops(Arc::new(RatifyOpsImpl))` in `asd-pro`.
/// The `repo` is passed per-call by the cli dispatch layer so no storage or
/// lifetime is needed here.
pub struct RatifyOpsImpl;

impl RatifyOps for RatifyOpsImpl {
    fn approve_entry(
        &self,
        repo: &Repository,
        ref_name: &str,
        entry_id: &str,
        approver_id: &str,
        approver_kind: &str,
        message: Option<&str>,
        agent_id: &str,
    ) -> Result<ApprovalOutcome> {
        approve_impl(repo, ref_name, entry_id, approver_id, approver_kind, message, agent_id)
    }

    fn reject_entry(
        &self,
        repo: &Repository,
        ref_name: &str,
        entry_id: &str,
        reviewer_id: &str,
        reviewer_kind: &str,
        reason: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome> {
        reject_impl(repo, ref_name, entry_id, reviewer_id, reviewer_kind, reason, agent_id)
    }

    fn withdraw_entry(
        &self,
        repo: &Repository,
        ref_name: &str,
        entry_id: &str,
        author_id: &str,
        agent_id: &str,
    ) -> Result<ReviewOutcome> {
        withdraw_impl(repo, ref_name, entry_id, author_id, agent_id)
    }
}

// ---------------------------------------------------------------------------
// Shared free-function implementations
// ---------------------------------------------------------------------------

fn append_entry_impl(
    repo: &Repository,
    ref_name: &str,
    entry: &LedgerEntry,
    agent_id: &str,
) -> Result<()> {
    let path = paths::ledger_entry_path(&entry.symbol_id, &entry.entry_id);
    let value = serde_json::to_value(entry)?;
    let opts = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("ledger {} for {}", entry.kind.as_str(), entry.symbol_id),
    );
    repo.set_json(ref_name, &path, &value, opts)?;

    // Write reverse index: entry_id → symbol_id for O(1) find_entry.
    let idx_path = paths::ledger_entry_index_path(&entry.entry_id);
    let idx_val = serde_json::Value::String(entry.symbol_id.clone());
    let idx_opts = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("ledger-idx {}", entry.entry_id),
    );
    repo.set_json(ref_name, &idx_path, &idx_val, idx_opts)?;
    Ok(())
}

fn list_entries_impl(
    repo: &Repository,
    ref_name: &str,
    symbol_id: &str,
) -> Result<Vec<LedgerEntry>> {
    let parent = paths::ledger_symbol_path(symbol_id);
    let json = match repo.get_json(ref_name, &parent) {
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
    Ok(entries)
}

fn approve_impl(
    repo: &Repository,
    ref_name: &str,
    entry_id: &str,
    approver_id: &str,
    approver_kind: &str,
    message: Option<&str>,
    agent_id: &str,
) -> Result<ApprovalOutcome> {
    let (symbol_id, mut entry) = find_entry(repo, ref_name, entry_id)?
        .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

    if entry.tags.iter().any(|t| t == "approved") {
        return Ok(ApprovalOutcome { entry, already_approved: true });
    }
    if let Some(bad) = entry.tags.iter().find(|t| *t == "rejected" || *t == "withdrawn") {
        return Err(AsdError::Other(format!(
            "entry {} is already {} and cannot be approved",
            entry_id, bad
        )));
    }
    if !entry.tags.iter().any(|t| t == "awaiting-approval") {
        return Err(AsdError::Other(format!("entry {} is not awaiting approval", entry_id)));
    }
    authorize_reviewer(&entry, approver_id, approver_kind)?;

    entry.tags.retain(|t| t != "awaiting-approval");
    entry.tags.push("approved".to_string());
    entry.tags.push(format!("approved-by:{}", approver_id));
    entry.tags.push(format!("approved-at:{}", iso_now()));
    if let Some(msg) = message {
        append_to_body(&mut entry, "Approver note", approver_id, msg);
    }
    rewrite(repo, ref_name, &symbol_id, &entry, agent_id, "approve")?;
    Ok(ApprovalOutcome { entry, already_approved: false })
}

fn reject_impl(
    repo: &Repository,
    ref_name: &str,
    entry_id: &str,
    reviewer_id: &str,
    reviewer_kind: &str,
    reason: &str,
    agent_id: &str,
) -> Result<ReviewOutcome> {
    let (symbol_id, mut entry) = find_entry(repo, ref_name, entry_id)?
        .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

    if entry.tags.iter().any(|t| t == "rejected") {
        return Ok(ReviewOutcome { entry, already_resolved: true });
    }
    if let Some(bad) = entry.tags.iter().find(|t| *t == "approved" || *t == "withdrawn") {
        return Err(AsdError::Other(format!(
            "entry {} is already {} and cannot be rejected",
            entry_id, bad
        )));
    }
    if !entry.tags.iter().any(|t| t == "awaiting-approval") {
        return Err(AsdError::Other(format!("entry {} is not awaiting approval", entry_id)));
    }
    authorize_reviewer(&entry, reviewer_id, reviewer_kind)?;
    if reason.trim().is_empty() {
        return Err(AsdError::Other("reject requires a non-empty reason".into()));
    }
    entry.tags.retain(|t| t != "awaiting-approval");
    entry.tags.push("rejected".to_string());
    entry.tags.push(format!("rejected-by:{}", reviewer_id));
    entry.tags.push(format!("rejected-at:{}", iso_now()));
    append_to_body(&mut entry, "Rejection reason", reviewer_id, reason);
    rewrite(repo, ref_name, &symbol_id, &entry, agent_id, "reject")?;
    Ok(ReviewOutcome { entry, already_resolved: false })
}

fn withdraw_impl(
    repo: &Repository,
    ref_name: &str,
    entry_id: &str,
    author_id: &str,
    agent_id: &str,
) -> Result<ReviewOutcome> {
    let (symbol_id, mut entry) = find_entry(repo, ref_name, entry_id)?
        .ok_or_else(|| AsdError::Other(format!("ledger entry not found: {}", entry_id)))?;

    if entry.tags.iter().any(|t| t == "withdrawn") {
        return Ok(ReviewOutcome { entry, already_resolved: true });
    }
    if let Some(bad) = entry.tags.iter().find(|t| *t == "approved" || *t == "rejected") {
        return Err(AsdError::Other(format!(
            "entry {} is already {} and cannot be withdrawn",
            entry_id, bad
        )));
    }
    if !entry.tags.iter().any(|t| t == "awaiting-approval") {
        return Err(AsdError::Other(format!("entry {} is not awaiting approval", entry_id)));
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
    rewrite(repo, ref_name, &symbol_id, &entry, agent_id, "withdraw")?;
    Ok(ReviewOutcome { entry, already_resolved: false })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn find_entry(
    repo: &Repository,
    ref_name: &str,
    entry_id: &str,
) -> Result<Option<(String, LedgerEntry)>> {
    // Fast path: use the reverse index written by append_entry_impl.
    let idx_path = paths::ledger_entry_index_path(entry_id);
    if let Ok(val) = repo.get_json(ref_name, &idx_path) {
        if let Some(symbol_id) = val.as_str() {
            let entry_path = paths::ledger_entry_path(symbol_id, entry_id);
            if let Ok(ev) = repo.get_json(ref_name, &entry_path) {
                if let Ok(entry) = serde_json::from_value::<LedgerEntry>(ev) {
                    return Ok(Some((symbol_id.to_string(), entry)));
                }
            }
        }
    }

    // Fallback: full scan for entries written before the index existed.
    let root = format!("{}/ledger", paths::ASD_ROOT);
    let tree = match repo.get_tree(ref_name, &root) {
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
    repo: &Repository,
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
    repo.set_json(ref_name, &path, &value, opts)?;
    Ok(())
}

fn authorize_reviewer(entry: &LedgerEntry, reviewer_id: &str, reviewer_kind: &str) -> Result<()> {
    let required: Vec<&str> = entry
        .tags
        .iter()
        .filter_map(|t| t.strip_prefix("approver:"))
        .collect();
    if required.is_empty() {
        return Ok(());
    }
    let ok = required.iter().any(|r| *r == reviewer_kind || *r == reviewer_id);
    if ok {
        Ok(())
    } else {
        Err(AsdError::Other(format!(
            "reviewer {} (kind={}) does not match any required approver: {:?}",
            reviewer_id, reviewer_kind, required
        )))
    }
}

fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn append_to_body(entry: &mut LedgerEntry, label: &str, author: &str, message: &str) {
    let section = format!("\n\n--- {} by {} ---\n{}", label, author, message);
    match &mut entry.body {
        Some(b) => b.push_str(&section),
        None => entry.body = Some(section.trim_start().to_string()),
    }
}
