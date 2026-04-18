use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::Result;
use crate::paths;
use crate::schema::LedgerEntry;

pub trait LedgerStore {
    fn append_entry(&self, ref_name: &str, entry: &LedgerEntry, agent_id: &str) -> Result<()>;
    fn list_entries(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<LedgerEntry>>;
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
}
