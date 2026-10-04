//! Moving a symbol's ledger history onto another symbol.
//!
//! A rebind is a move: every entry filed under the old symbol id leaves it
//! and lands under the new one, the entry index and the SQLite ledger cache
//! follow, and a record under `/asd/v1/rebinds/<old id>` says where it went.
//! The CLI and MCP used to copy instead — each entry ended up under both ids,
//! the index and cache kept pointing at the old one, and an orphaned entry
//! stayed orphaned — with a commit per entry.
//!
//! Any number of rebinds land in one commit, written on top of the state the
//! speculation forked from (see [`crate::subtree`]).

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::{LedgerEntry, Rebind, Symbol};
use crate::search_fts::SearchFtsDb;

/// What one rebind moved.
#[derive(Debug, Clone, Serialize)]
pub struct RebindOutcome {
    pub from_symbol_id: String,
    pub to_symbol_id: String,
    pub to_qname: String,
    pub entries_moved: usize,
}

/// Move the ledger entries filed under each `from` symbol id onto its target
/// symbol, in one commit.
///
/// Each moved entry loses its `orphaned`/`orphaned-at:` tags and gains
/// `rebound-at:<now>`. The new tag matters beyond the record: conclusions
/// import keeps whichever copy of an entry was revised last, judged by its
/// `*-at:` tags. An entry tagged orphaned whose symbol later came back is
/// exported with `orphaned-at:`, and without a later tag that copy would
/// count as newer and put the entry back where it came from.
///
/// Rebinding from a symbol with no entries still records the rebind (a
/// rename noted for later). A `from` id listed twice, or equal to its
/// target, is an error, and nothing is written.
pub fn rebind_ledger(
    repo: &Repository,
    fts: Option<&SearchFtsDb>,
    ref_name: &str,
    rebinds: &[(String, Symbol)],
    agent_id: &str,
) -> Result<Vec<RebindOutcome>> {
    let mut seen = std::collections::HashSet::new();
    for (from, to) in rebinds {
        if *from == to.symbol_id {
            return Err(AsdError::Other(format!(
                "{from} and {} are the same symbol — nothing to rebind",
                to.qname
            )));
        }
        if !seen.insert(from.as_str()) {
            return Err(AsdError::Other(format!("{from} is listed twice")));
        }
    }
    if rebinds.is_empty() {
        return Ok(Vec::new());
    }

    let (spec, fork) = crate::subtree::speculate_at_head(repo, ref_name, "asd-rebind")?;
    let staged = (|| -> Result<(Vec<RebindOutcome>, Vec<LedgerEntry>)> {
        let rebinds_root = format!("{}/rebinds", paths::ASD_ROOT);
        let mut ledger = crate::subtree::read_seed(repo, &fork, &paths::ledger_root())?;
        let mut index = crate::subtree::read_seed(repo, &fork, &paths::ledger_index_root())?;
        let mut records = crate::subtree::read_seed(repo, &fork, &rebinds_root)?;
        let now = Utc::now();
        let rebound_at = format!("rebound-at:{}", now.format("%Y-%m-%dT%H:%M:%SZ"));

        let mut outcomes = Vec::with_capacity(rebinds.len());
        let mut moved = Vec::new();
        for (from, to) in rebinds {
            let entries = match ledger.remove(from) {
                Some(Value::Object(entries)) => entries,
                _ => Default::default(),
            };
            let entries_moved = entries.len();
            if entries_moved > 0 {
                let Value::Object(dest) = ledger
                    .entry(to.symbol_id.clone())
                    .or_insert_with(|| Value::Object(Default::default()))
                else {
                    return Err(AsdError::Other(format!(
                        "ledger node for {} is not a map",
                        to.symbol_id
                    )));
                };
                for (entry_id, value) in entries {
                    let mut entry: LedgerEntry = serde_json::from_value(value).map_err(|e| {
                        AsdError::Other(format!("ledger entry {entry_id} under {from}: {e}"))
                    })?;
                    entry.symbol_id = to.symbol_id.clone();
                    entry
                        .tags
                        .retain(|t| t != "orphaned" && !t.starts_with("orphaned-at:"));
                    entry.tags.push(rebound_at.clone());
                    index.insert(entry_id.clone(), Value::String(to.symbol_id.clone()));
                    dest.insert(entry_id, serde_json::to_value(&entry)?);
                    moved.push(entry);
                }
            }
            let record = Rebind {
                from_symbol_id: from.clone(),
                to_symbol_id: to.symbol_id.clone(),
                to_qname: to.qname.clone(),
                at: now,
                by: agent_id.to_string(),
            };
            records.insert(from.clone(), serde_json::to_value(&record)?);
            outcomes.push(RebindOutcome {
                from_symbol_id: from.clone(),
                to_symbol_id: to.symbol_id.clone(),
                to_qname: to.qname.clone(),
                entries_moved,
            });
        }

        let write = |path: &str, map: serde_json::Map<String, Value>| {
            repo.spec_set_json(spec, path, &Value::Object(map))
                .map_err(|e| AsdError::Other(e.to_string()))
        };
        if !moved.is_empty() {
            write(&paths::ledger_root(), ledger)?;
            write(&paths::ledger_index_root(), index)?;
        }
        write(&rebinds_root, records)?;
        Ok((outcomes, moved))
    })();
    let (outcomes, moved) = match staged {
        Ok(staged) => staged,
        Err(e) => {
            let _ = repo.discard_speculation(spec);
            return Err(e);
        }
    };

    let reasoning = outcomes
        .iter()
        .map(|o| {
            format!(
                "{} → {} ({}, {} entries)",
                o.from_symbol_id, o.to_qname, o.to_symbol_id, o.entries_moved
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let opts = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!(
            "rebind {} symbol(s), moving {} ledger entr{}",
            outcomes.len(),
            moved.len(),
            if moved.len() == 1 { "y" } else { "ies" }
        ),
    )
    .with_reasoning(reasoning);
    repo.commit_speculation(spec, opts)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    // The cache answers `list_entries`; a row left on the old id would keep
    // showing the entry there.
    if let Some(fts) = fts {
        for entry in &moved {
            fts.upsert_ledger_entry(entry, ref_name)
                .map_err(|e| AsdError::Other(format!("ledger cache: {e}")))?;
        }
    }
    Ok(outcomes)
}
