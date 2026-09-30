//! Ledger integrity across the three places an entry lives.
//!
//! - **cache** — `asd_ledger_cache` in SQLite. Written by every
//!   `append_entry` and never deleted from, so it holds every entry ever
//!   acknowledged. Readers serve from it first.
//! - **ASG** — `/asd/v1/ledger/<symbol>/<entry>`, the authoritative store
//!   and the only thing `asd sync` exports.
//! - **sidecar** — `.asd/v1/ledger/<symbol>/<entry>.json`, what `asd sync`
//!   wrote and what `asd hydrate` restores from.
//!
//! Before ASG compare-and-swapped its refs, concurrent writers and `asd
//! index` speculation commits discarded ledger commits from ASG after the
//! cache had recorded them (1,684 of 12,085 on one store). Every surface kept
//! working because reads come from the cache, so the loss was invisible
//! until a hydrate came back short. This module makes the three counts
//! visible and restores cache-only entries into ASG.

use std::collections::BTreeSet;
use std::path::Path;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use serde::Serialize;
use serde_json::Value;

use crate::engine::Engine;
use crate::error::{AsdError, Result};
use crate::paths;
use crate::search_fts::SearchFtsDb;

/// Ledger entry counts in each store, plus the gaps between them.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerCounts {
    /// Rows in `asd_ledger_cache` for the ref. `None` when no cache is open.
    pub cache: Option<usize>,
    /// Entries in the ASG ledger tree.
    pub asg: usize,
    /// Entry files under `.asd/v1/ledger/`. `None` when there is no sidecar.
    pub sidecar: Option<usize>,
    /// Cached entries absent from ASG — lost writes. `asd repair --fix`
    /// restores them.
    pub missing_from_asg: usize,
    /// ASG entries absent from the sidecar — `asd sync` has not run since.
    pub missing_from_sidecar: usize,
}

impl LedgerCounts {
    /// True when nothing the cache acknowledged is missing downstream.
    pub fn is_consistent(&self) -> bool {
        self.missing_from_asg == 0 && self.missing_from_sidecar == 0
    }

    /// One-line warning for a mismatch, or `None` when consistent.
    pub fn warning(&self) -> Option<String> {
        if self.is_consistent() {
            return None;
        }
        let mut parts = Vec::new();
        if self.missing_from_asg > 0 {
            parts.push(format!(
                "{} ledger entr{} in the cache but missing from the store — \
                 run `asd repair` to review and `asd repair --fix` to restore",
                self.missing_from_asg,
                if self.missing_from_asg == 1 {
                    "y is"
                } else {
                    "ies are"
                }
            ));
        }
        if self.missing_from_sidecar > 0 {
            parts.push(format!(
                "{} ledger entr{} not in .asd/ — run `asd sync`",
                self.missing_from_sidecar,
                if self.missing_from_sidecar == 1 {
                    "y is"
                } else {
                    "ies are"
                }
            ));
        }
        Some(parts.join("; "))
    }
}

/// Every entry id in the ASG ledger tree, across all symbols.
///
/// Reads keys only: capped one level below the ledger root, each symbol's
/// bucket comes back as its key list and no entry is ever loaded. `asd
/// status` calls this, so it has to stay cheap on a store with tens of
/// thousands of entries.
pub fn asg_ledger_entry_ids(repo: &Repository, ref_name: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    // A store with no ledger yet has no `/asd/v1/ledger` path at all.
    if let Ok(Value::Object(by_symbol)) = repo.get_json_capped(ref_name, &paths::ledger_root(), 1) {
        for bucket in by_symbol.values() {
            match bucket.get("_keys") {
                Some(Value::Array(keys)) => {
                    ids.extend(keys.iter().filter_map(|k| k.as_str().map(str::to_string)));
                }
                // Not truncated (e.g. an empty bucket): read the keys directly.
                _ => {
                    if let Value::Object(entries) = bucket {
                        ids.extend(entries.keys().cloned());
                    }
                }
            }
        }
    }
    ids
}

/// Every entry id under `<root>/.asd/v1/ledger/`, or `None` when the sidecar
/// has no ledger directory.
pub fn sidecar_ledger_entry_ids(root: &Path) -> Option<BTreeSet<String>> {
    let ledger = root.join(".asd").join("v1").join("ledger");
    let symbols = std::fs::read_dir(&ledger).ok()?;
    let mut ids = BTreeSet::new();
    for sym in symbols.flatten() {
        let Ok(files) = std::fs::read_dir(sym.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                ids.insert(stem.to_string());
            }
        }
    }
    Some(ids)
}

/// Count ledger entries in the cache, ASG and (when `sidecar_root` is given
/// and has one) the sidecar, and the gaps between them.
pub fn ledger_counts(
    repo: &Repository,
    ref_name: &str,
    fts: Option<&SearchFtsDb>,
    sidecar_root: Option<&Path>,
) -> Result<LedgerCounts> {
    let asg = asg_ledger_entry_ids(repo, ref_name);

    let (cache, missing_from_asg) = match fts {
        Some(fts) => {
            let ids = fts
                .ledger_cache_entry_ids(ref_name)
                .map_err(|e| AsdError::Other(format!("read ledger cache: {e}")))?;
            let missing = ids.iter().filter(|id| !asg.contains(*id)).count();
            (Some(ids.len()), missing)
        }
        None => (None, 0),
    };

    let sidecar_ids = sidecar_root.and_then(sidecar_ledger_entry_ids);
    let missing_from_sidecar = sidecar_ids
        .as_ref()
        .map(|s| asg.difference(s).count())
        .unwrap_or(0);

    Ok(LedgerCounts {
        cache,
        asg: asg.len(),
        sidecar: sidecar_ids.map(|s| s.len()),
        missing_from_asg,
        missing_from_sidecar,
    })
}

/// Outcome of [`restore_missing_ledger_entries`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct LedgerRestoreReport {
    /// Cached entries that were missing from ASG before the restore.
    pub missing: usize,
    /// Entries written back into ASG.
    pub restored: usize,
    /// `(entry_id, error)` for entries that could not be written.
    pub failed: Vec<(String, String)>,
    /// Cache rows missing from ASG whose body no longer parses, so there is
    /// nothing to restore them from.
    pub unparseable: Vec<String>,
}

/// Cached ledger entries whose `entry_id` appears nowhere in the ASG ledger
/// tree. Matching by id rather than path means an entry moved by `ledger
/// rebind` (whose cache row follows it to the new symbol) is never
/// mistaken for a lost one.
///
/// Returns the restorable entries and the ids of missing rows whose cached
/// body no longer parses.
pub fn missing_ledger_entries(
    engine: &Engine,
) -> Result<(Vec<crate::schema::LedgerEntry>, Vec<String>)> {
    let Some(fts) = engine.fts.as_ref() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let asg = asg_ledger_entry_ids(&engine.repo, &engine.ref_name);
    let (entries, unparseable) = fts
        .all_ledger_entries(&engine.ref_name)
        .map_err(|e| AsdError::Other(format!("read ledger cache: {e}")))?;
    Ok((
        entries
            .into_iter()
            .filter(|e| !asg.contains(&e.entry_id))
            .collect(),
        unparseable
            .into_iter()
            .filter(|id| !asg.contains(id))
            .collect(),
    ))
}

/// Write every cache-only ledger entry back into ASG, with its reverse-index
/// record, exactly as the cache holds it. Each restore is its own commit so
/// the audit trail says what was recovered and from where.
pub fn restore_missing_ledger_entries(
    engine: &Engine,
    agent_id: &str,
) -> Result<LedgerRestoreReport> {
    let (missing, unparseable) = missing_ledger_entries(engine)?;
    let mut report = LedgerRestoreReport {
        missing: missing.len() + unparseable.len(),
        unparseable,
        ..Default::default()
    };
    for entry in &missing {
        let result = (|| -> Result<()> {
            let value = serde_json::to_value(entry)?;
            engine.repo.set_json(
                &engine.ref_name,
                &paths::ledger_entry_path(&entry.symbol_id, &entry.entry_id),
                &value,
                CommitOptions::new(
                    agent_id,
                    IntentCategory::Refine,
                    format!(
                        "restore ledger {} {} for {} (lost from the store, recovered from the ledger cache)",
                        entry.kind.as_str(),
                        entry.entry_id,
                        entry.symbol_id
                    ),
                ),
            )?;
            engine.repo.set_json(
                &engine.ref_name,
                &paths::ledger_entry_index_path(&entry.entry_id),
                &Value::String(entry.symbol_id.clone()),
                CommitOptions::new(
                    agent_id,
                    IntentCategory::Refine,
                    format!("restore ledger-idx {}", entry.entry_id),
                ),
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => report.restored += 1,
            Err(e) => report.failed.push((entry.entry_id.clone(), e.to_string())),
        }
    }
    Ok(report)
}
