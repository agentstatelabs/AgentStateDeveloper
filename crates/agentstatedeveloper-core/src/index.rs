use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::Result;
use crate::paths;
use crate::schema::Symbol;

pub trait IndexStore {
    fn put_symbol(&self, ref_name: &str, symbol: &Symbol, agent_id: &str) -> Result<()>;
    fn get_symbol_by_qname(&self, ref_name: &str, qname: &str) -> Result<Option<Symbol>>;
}

pub struct AsgIndexStore<'a> {
    pub repo: &'a Repository,
}

impl<'a> IndexStore for AsgIndexStore<'a> {
    fn put_symbol(&self, ref_name: &str, symbol: &Symbol, agent_id: &str) -> Result<()> {
        let code = paths::code_path(&symbol.language, &symbol.file, &symbol.symbol_fp);
        let qname = paths::qname_index_path(&symbol.qname);
        let value = serde_json::to_value(symbol)?;

        let opts1 = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("index symbol {}", symbol.qname),
        );
        self.repo.set_json(ref_name, &code, &value, opts1)?;

        // Secondary index: duplicate symbol under qname for O(1) lookup.
        // Content-addressing dedups the storage.
        let opts2 = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("qname index {}", symbol.qname),
        );
        self.repo.set_json(ref_name, &qname, &value, opts2)?;
        Ok(())
    }

    fn get_symbol_by_qname(&self, ref_name: &str, qname: &str) -> Result<Option<Symbol>> {
        let path = paths::qname_index_path(qname);
        match self.repo.get_json(ref_name, &path) {
            Ok(v) => Ok(Some(serde_json::from_value(v)?)),
            Err(_) => Ok(None),
        }
    }
}
