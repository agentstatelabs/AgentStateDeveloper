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

    /// Plan J t-009: cross-language qname resolution.
    ///
    /// When two adapters produce the same `qname` (e.g. `auth.User`
    /// from a Python module and a Swift struct with the same fully
    /// qualified name), the primary qname index at
    /// `/asd/v1/index/by-qname/{qname}` holds **whichever was
    /// written last** — every prior language's symbol at that qname
    /// is overwritten in that secondary index. The authoritative
    /// per-language code tree at `/asd/v1/code/{lang}/{file}/...`
    /// still has both, but lookups by qname alone silently pick the
    /// winner of an arbitrary write order.
    ///
    /// This method takes an explicit language hint. If the primary
    /// qname-index entry's language matches the hint, return it
    /// (fast path). Otherwise walk the `code/{hint}/` tree for a
    /// symbol with the requested qname; return it if found, else
    /// fall back to the primary entry (better to return *something*
    /// matching the qname than nothing — the caller can still see
    /// the language mismatch on the returned Symbol).
    ///
    /// Default impl forwards to the language-agnostic
    /// `get_symbol_by_qname`. Adapters that can resolve polyglot
    /// collisions override this.
    fn get_symbol_by_qname_lang(
        &self,
        ref_name: &str,
        qname: &str,
        _language_hint: Option<&str>,
    ) -> Result<Option<Symbol>> {
        self.get_symbol_by_qname(ref_name, qname)
    }

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
        Self {
            repo: &engine.repo,
            fts: engine.fts.as_ref(),
        }
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

    /// Build both full edge maps — `(callers_of, callees_of)`, each
    /// `symbol_id → [neighbor_id, …]` — using the borrowed FTS connection,
    /// falling back to one git tree walk per direction on cache miss.
    ///
    /// The bulk analog of `get_callers`/`get_callees`: callers that touch
    /// many nodes per request (e.g. the `/graph` BFS) use this instead of one
    /// per-node git read. Same cache guard as the per-symbol getters
    /// (`symbols_cached_for`), so cached and per-symbol reads always agree.
    pub fn build_edge_maps(
        &self,
        engine: &Engine,
    ) -> (HashMap<String, Vec<String>>, HashMap<String, Vec<String>>) {
        if let Some(fts) = &self.fts {
            if fts.symbols_cached_for(&engine.ref_name) {
                return fts.build_edge_maps_cached(&engine.ref_name);
            }
        }
        // Git fallback (cold cache or first run). Tree shape per direction:
        //   /asd/v1/index/callers/{symbol_id} = {"callers": [id, …]}
        //   /asd/v1/index/callees/{symbol_id} = {"callees": [id, …]}
        let read_direction = |dir: &str, field: &str| -> HashMap<String, Vec<String>> {
            let prefix = format!("{}/index/{}", paths::ASD_ROOT, dir);
            match engine.repo.get_tree(&engine.ref_name, &prefix) {
                Ok(serde_json::Value::Object(map)) => map
                    .into_iter()
                    .map(|(symbol_id, v)| (symbol_id, extract_string_array(&v, field)))
                    .collect(),
                _ => HashMap::new(),
            }
        };
        (
            read_direction("callers", "callers"),
            read_direction("callees", "callees"),
        )
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

    /// Plan J t-009: cross-language qname resolution. See trait
    /// docs. The lookup is two-step:
    ///
    /// 1. Try the primary qname index. If its language matches the
    ///    hint (or no hint was given), return it — no extra reads.
    /// 2. Otherwise walk `/asd/v1/code/{lang_hint}/` for a symbol
    ///    whose qname equals `qname`. Return the first match.
    /// 3. If the per-language tree has no match, fall back to the
    ///    primary entry. Returning the qname-index hit (even
    ///    language-mismatched) is more useful than `None` —
    ///    downstream code can inspect `Symbol.language` and decide.
    fn get_symbol_by_qname_lang(
        &self,
        ref_name: &str,
        qname: &str,
        language_hint: Option<&str>,
    ) -> Result<Option<Symbol>> {
        let primary = self.get_symbol_by_qname(ref_name, qname)?;
        let hint = match language_hint {
            Some(h) if !h.is_empty() => h,
            _ => return Ok(primary), // No hint → nothing to disambiguate.
        };
        if let Some(ref sym) = primary {
            if sym.language == hint {
                return Ok(primary);
            }
        }
        // Walk the per-language code tree. Path shape:
        //   /asd/v1/code/{lang}/{file}/{symbol_fp}
        // We don't know the file or fp, so list and filter.
        let lang_root = format!("{}/code/{}", paths::ASD_ROOT, hint);
        if let Ok(tree) = self.repo.get_tree(ref_name, &lang_root) {
            for_each_symbol(&tree, &mut |sym_value| {
                if let Ok(sym) = serde_json::from_value::<Symbol>(sym_value.clone()) {
                    if sym.qname == qname && sym.language == hint {
                        return Some(sym);
                    }
                }
                None
            })
            .map(|found| Ok(Some(found)))
            .unwrap_or(Ok(primary))
        } else {
            Ok(primary)
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

/// Plan J t-009: walk a `code/{lang}/` subtree depth-first looking
/// for a Symbol JSON leaf. Returns the first leaf for which `f`
/// produces `Some(_)`. The tree shape under `code/{lang}/` is
/// `{file_segment}/.../ {symbol_fp}: Symbol`, but `get_tree` flattens
/// it into a nested object so we just recurse over Object children
/// until we hit a leaf whose value parses as a Symbol.
fn for_each_symbol<R>(
    tree: &serde_json::Value,
    f: &mut dyn FnMut(&serde_json::Value) -> Option<R>,
) -> Option<R> {
    // Try this node first — leaves (Symbol JSONs) are objects with
    // the fields f's closure cares about; if it matches, return.
    if let Some(found) = f(tree) {
        return Some(found);
    }
    if let serde_json::Value::Object(map) = tree {
        for child in map.values() {
            if let Some(found) = for_each_symbol(child, f) {
                return Some(found);
            }
        }
    }
    None
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
