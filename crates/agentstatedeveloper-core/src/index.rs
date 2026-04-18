use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::Result;
use crate::paths;
use crate::schema::Symbol;

pub trait IndexStore {
    fn put_symbol(&self, ref_name: &str, symbol: &Symbol, agent_id: &str) -> Result<()>;
    fn get_symbol_by_qname(&self, ref_name: &str, qname: &str) -> Result<Option<Symbol>>;

    /// Read the callees list previously written for `symbol_id`. Returns an
    /// empty Vec if no edges have been recorded.
    fn get_callees(&self, _ref_name: &str, _symbol_id: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Read the callers list previously written for `symbol_id`. Returns an
    /// empty Vec if no edges have been recorded.
    fn get_callers(&self, _ref_name: &str, _symbol_id: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
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

    fn get_callees(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<String>> {
        let path = paths::callees_path(symbol_id);
        match self.repo.get_json(ref_name, &path) {
            Ok(v) => Ok(extract_string_array(&v, "callees")),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn get_callers(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<String>> {
        let path = paths::callers_path(symbol_id);
        match self.repo.get_json(ref_name, &path) {
            Ok(v) => Ok(extract_string_array(&v, "callers")),
            Err(_) => Ok(Vec::new()),
        }
    }
}

fn extract_string_array(v: &serde_json::Value, field: &str) -> Vec<String> {
    v.get(field)
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
