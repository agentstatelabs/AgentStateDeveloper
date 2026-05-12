//! Feedback store — durable (query, symbol, verdict) triples.
//!
//! Agents and users record verdicts on search results via `asd feedback mark`
//! or the MCP `feedback_mark` tool. Verdicts are stored in the ASD sidecar
//! and applied as score adjustments in `apply_feedback_adjustments`.
//!
//! ## SQLite write-through cache
//!
//! `AsgFeedbackStore` optionally holds a `db_path` — when present, `list_all`
//! returns the SQLite cache (fast, ~0 git reads) and `record` additionally
//! writes to SQLite after the git commit.  The git object store remains
//! authoritative: if SQLite is empty (e.g. first run after `git pull`), we
//! fall back to the git tree walk and re-populate the cache as a side effect.
//! Running `asd index` / `asd reindex` calls `sync_feedback_entries` for a
//! full reconciliation.

use std::path::Path;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::Result;
use crate::paths;
use crate::schema::{FeedbackEntry, FeedbackVerdict};
use crate::search_fts::SearchFtsDb;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub trait FeedbackStore {
    /// Record a verdict for a (query, symbol) pair.
    fn record(&self, ref_name: &str, entry: &FeedbackEntry, agent_id: &str) -> Result<()>;

    /// All feedback entries recorded for a specific symbol.
    fn list_for_symbol(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<FeedbackEntry>>;

    /// Every feedback entry in the store.
    fn list_all(&self, ref_name: &str) -> Result<Vec<FeedbackEntry>>;

    /// Flatten all feedback into (symbol_id, query, verdict) triples for
    /// use in `apply_feedback_adjustments`.
    fn flat_verdicts(
        &self,
        ref_name: &str,
    ) -> Result<Vec<(String, String, FeedbackVerdict)>> {
        Ok(self
            .list_all(ref_name)?
            .into_iter()
            .filter(|e| e.file_scope.is_none())
            .map(|e| (e.symbol_id, e.query, e.verdict))
            .collect())
    }

    /// Flatten file-scoped feedback into (file_glob, verdict, query) triples for
    /// use in `apply_file_scope_feedback`.
    fn flat_file_scope_verdicts(
        &self,
        ref_name: &str,
    ) -> Result<Vec<(String, FeedbackVerdict, String)>> {
        Ok(self
            .list_all(ref_name)?
            .into_iter()
            .filter_map(|e| e.file_scope.map(|glob| (glob, e.verdict, e.query)))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// ASG-backed implementation
// ---------------------------------------------------------------------------

pub struct AsgFeedbackStore<'a> {
    pub repo: &'a Repository,
    /// When `Some`, enables the SQLite write-through cache: `list_all` reads
    /// from SQLite if populated, `record` writes to SQLite after the git commit.
    pub db_path: Option<&'a Path>,
}

impl<'a> FeedbackStore for AsgFeedbackStore<'a> {
    fn record(&self, ref_name: &str, entry: &FeedbackEntry, agent_id: &str) -> Result<()> {
        // Git is always written first — it's the authoritative store.
        let path = paths::feedback_entry_path(&entry.symbol_id, &entry.entry_id);
        let value = serde_json::to_value(entry)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("feedback {} for {}", entry.verdict.as_str(), entry.symbol_qname),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;
        // Best-effort SQLite write-through; failures are non-fatal.
        if let Some(db) = self.db_path {
            if let Ok(fts) = SearchFtsDb::open(db) {
                let _ = fts.upsert_feedback(entry);
            }
        }
        Ok(())
    }

    fn list_for_symbol(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<FeedbackEntry>> {
        let prefix = paths::feedback_symbol_path(symbol_id);
        match self.repo.get_tree(ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => {
                let mut entries: Vec<FeedbackEntry> = map
                    .values()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect();
                entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(entries)
            }
            _ => Ok(vec![]),
        }
    }

    fn list_all(&self, ref_name: &str) -> Result<Vec<FeedbackEntry>> {
        // Fast path: SQLite cache — zero git tree walks when populated.
        if let Some(db) = self.db_path {
            if let Ok(fts) = SearchFtsDb::open(db) {
                if fts.feedback_count() > 0 {
                    if let Ok(entries) = fts.list_all_feedback() {
                        return Ok(entries);
                    }
                }
            }
        }

        // Authoritative git path — also runs when SQLite is empty (e.g. first
        // run after `git pull`).  Re-populate the cache as a side effect so
        // subsequent calls are fast.
        let prefix = format!("{}/feedback", paths::ASD_ROOT);
        let mut entries = Vec::new();
        if let Ok(serde_json::Value::Object(by_symbol)) =
            self.repo.get_tree(ref_name, &prefix)
        {
            for symbol_val in by_symbol.values() {
                if let serde_json::Value::Object(symbol_entries) = symbol_val {
                    for ev in symbol_entries.values() {
                        if let Ok(e) =
                            serde_json::from_value::<FeedbackEntry>(ev.clone())
                        {
                            entries.push(e);
                        }
                    }
                }
            }
        }
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Populate the SQLite cache for the next call — best effort.
        if !entries.is_empty() {
            if let Some(db) = self.db_path {
                if let Ok(fts) = SearchFtsDb::open(db) {
                    let _ = fts.sync_feedback_entries(&entries);
                }
            }
        }

        Ok(entries)
    }
}
