use std::collections::HashMap;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::engine::Engine;
use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::Symbol;
use crate::search_fts::SearchFtsDb;

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

/// ASD index reader/writer.
///
/// Hot-path read methods (`get_symbol_by_qname`, `get_callers`, `get_callees`,
/// `build_id_map`) hit the SQLite cache populated at `asd index` time.  Git is
/// only consulted on cache miss.
///
/// The FTS connection is **borrowed** from the owning `Engine` (opened once in
/// `Engine::open_sqlite`) so no `Connection::open` occurs at store construction
/// or on any method call.
pub struct AsgIndexStore<'a> {
    pub repo: &'a Repository,
    /// Borrowed FTS connection from the owning `Engine`.
    fts: Option<&'a SearchFtsDb>,
}

impl<'a> AsgIndexStore<'a> {
    /// Construct without SQLite caching (tests, one-off internal calls).
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo, fts: None }
    }

    /// Convenience: borrow the FTS connection already open in `engine`.
    pub fn from_engine(engine: &'a Engine) -> Self {
        Self { repo: &engine.repo, fts: engine.fts.as_ref() }
    }

    /// Build the full `symbol_id → Symbol` map using the borrowed FTS connection.
    /// Falls back to the git by-qname tree walk on cache miss.
    pub fn build_id_map(&self, engine: &Engine) -> HashMap<String, Symbol> {
        if let Some(fts) = &self.fts {
            if fts.symbols_cached_for(&engine.ref_name) {
                let map = fts.build_id_map_cached(&engine.ref_name);
                if !map.is_empty() {
                    return map;
                }
            }
        }
        // Git fallback (cold cache or first run).
        let tree = engine
            .repo
            .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(serde_json::Value::Object(Default::default()));
        tree.as_object()
            .map(|m| {
                m.values()
                    .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                    .map(|s| (s.symbol_id.clone(), s))
                    .collect()
            })
            .unwrap_or_default()
    }
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
        // Fast path: reuse the open FTS connection — no extra open().
        if let Some(fts) = &self.fts {
            if fts.symbols_cached_for(ref_name) {
                return Ok(fts.get_symbol_by_qname_cached(qname, ref_name));
            }
        }
        // Authoritative git path.
        let path = paths::qname_index_path(qname);
        match self.repo.get_json(ref_name, &path) {
            Ok(v) => Ok(Some(serde_json::from_value(v)?)),
            Err(agentstategraph::RepoError::Tree(_)) => Ok(None),
            Err(e) => Err(AsdError::Repo(e)),
        }
    }

    fn get_callees(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<String>> {
        // Fast path: reuse open FTS connection.
        if let Some(fts) = &self.fts {
            if fts.symbols_cached_for(ref_name) {
                return Ok(fts.get_neighbors_cached(symbol_id, "callee", ref_name));
            }
        }
        // Authoritative git path.
        let path = paths::callees_path(symbol_id);
        match self.repo.get_json(ref_name, &path) {
            Ok(v) => Ok(extract_string_array(&v, "callees")),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn get_callers(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<String>> {
        // Fast path: reuse open FTS connection.
        if let Some(fts) = &self.fts {
            if fts.symbols_cached_for(ref_name) {
                return Ok(fts.get_neighbors_cached(symbol_id, "caller", ref_name));
            }
        }
        // Authoritative git path.
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
