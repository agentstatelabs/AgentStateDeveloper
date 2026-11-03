use std::collections::{HashMap, HashSet};

use crate::cross_service::DetectedEndpoint;
use crate::dataflow::DetectedDataFlow;
use crate::error::Result;
use crate::schema::{Effect, SymbolKind};

/// Parsed symbol returned by a language adapter. Carries enough info to
/// build a full [`Symbol`](crate::schema::Symbol) node.
#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub qname: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub body: String,
    pub signature: Option<String>,
    pub doc: Option<String>,
}

/// A directed call edge from one qname to another, produced by a language
/// adapter's intra-module call-graph extractor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallEdge {
    pub caller_qname: String,
    pub callee_qname: String,
}

/// Parser-backed location for a resolved static edge. Import is a syntactic
/// candidate, not a complete resolution proof or graph-completeness claim.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CallSite {
    pub caller_qname: String,
    pub callee_qname: String,
    pub file: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub expression: String,
    pub expression_complete: bool,
    pub syntactic_import_candidate: Option<String>,
    pub import_candidate_omitted: bool,
    pub caller_symbol_fp: String,
    pub caller_start_line: u32,
    pub caller_end_line: u32,
}

/// Workspace-wide context passed to adapters when resolving call edges
/// across module boundaries.
///
/// ## Suffix-based lookup
///
/// Qnames include file-path prefixes (e.g., `Sources.Models.DriftCompiler.compile`).
/// A call site using `DriftCompiler.compile(...)` only sees the type+method tail.
/// After inserting all qnames, call [`build_suffix_index`] once so that
/// [`find_by_suffix`] can resolve `"DriftCompiler.compile"` → the full qname in O(1).
///
/// ## Cross-file property map
///
/// Stored property declarations (`let name: TypeName` / `var name: TypeName`)
/// are collected across ALL files in the current index run and stored in
/// `properties` as `"EnclosingTypeSimpleName.propName" → "TypeSimpleName"`.
/// Call adapters populate this via [`LanguageAdapter::extract_property_types`].
/// Swift uses it to resolve `pool.resolve()` → `DriftSynthPool.resolve` when
/// `pool: DriftSynthPool` is declared in a different file of the same project.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSymbols {
    /// All qnames known to the indexer, flat.
    pub qnames: HashSet<String>,
    /// Map from qname to its kind, so adapters can distinguish
    /// class-level methods from module-level functions during resolution.
    pub kinds: HashMap<String, SymbolKind>,
    /// suffix → Vec<full_qname>. Built by [`build_suffix_index`].
    /// Each key is a 2-or-more component tail of a qname, e.g.
    /// `"DriftCompiler.compile"` → `["Sources.Models.DriftCompiler.compile"]`.
    pub suffix_index: HashMap<String, Vec<String>>,
    /// Cross-file property map: `"EnclosingTypeSimple.propName"` →
    /// `"TypeSimpleName"`.  Populated by the index pipeline from ALL files
    /// via [`LanguageAdapter::extract_property_types`] before call-graph
    /// extraction runs.  Allows resolving instance property calls when the
    /// property declaration is in a different file than the method body.
    pub properties: HashMap<String, String>,
    /// Canonical symbol ownership, used to avoid binding exports to a different crate.
    pub symbol_files: HashMap<String, String>,
    /// Rust library entry-point path -> Cargo library name (normalized underscores).
    pub rust_crates: HashMap<String, String>,
    /// Source file -> consumer-scoped dependency key -> indexed library name.
    /// None blocks unsupported/unknown dependencies instead of guessing globally.
    pub rust_dependencies: HashMap<String, HashMap<String, Option<String>>>,
    /// Colliding Rust file modules: source file -> (legacy prefix, file-qualified prefix).
    pub rust_modules: HashMap<String, (String, String)>,
    /// Public reference paths -> possible canonical paths or other aliases.
    /// Multiple candidates are retained so conditional/ambiguous exports never guess.
    pub reference_aliases: HashMap<String, HashSet<String>>,
}

impl WorkspaceSymbols {
    /// Apply only this consumer's Cargo dependency aliases, never global aliases.
    pub fn rust_reference_path(&self, file: &str, reference: &str) -> Option<String> {
        let (head, tail) = reference.split_once('.').unwrap_or((reference, ""));
        if let Some(binding) = self
            .rust_dependencies
            .get(file)
            .and_then(|scope| scope.get(head))
        {
            let library = binding.as_ref()?;
            return Some(if tail.is_empty() {
                library.clone()
            } else {
                format!("{library}.{tail}")
            });
        }
        // Local file modules must resolve within this package, even when another
        // package has an identically named worker.rs/function.
        if let Some((package, _)) = file.rsplit_once("/src/") {
            let local = reference
                .strip_prefix("crate.")
                .or_else(|| reference.strip_prefix("self."))
                .unwrap_or(reference);
            for (module_file, (legacy, qualified)) in &self.rust_modules {
                if module_file
                    .rsplit_once("/src/")
                    .is_some_and(|(owner, _)| owner == package)
                {
                    if local == legacy {
                        return Some(qualified.clone());
                    }
                    if let Some(tail) = local.strip_prefix(&format!("{legacy}.")) {
                        return Some(format!("{qualified}.{tail}"));
                    }
                }
            }
        }
        Some(reference.to_owned())
    }

    pub fn resolve_rust_reference(&self, file: &str, reference: &str) -> Option<String> {
        self.resolve_reference(&self.rust_reference_path(file, reference)?)
    }

    /// Exact qname membership test.
    pub fn contains(&self, qname: &str) -> bool {
        self.qnames.contains(qname)
    }

    /// Resolve exact definitions or explicit aliases, including module-prefix aliases.
    /// Every branch must terminate at the same definition; cycles/unknowns are rejected.
    pub fn resolve_reference(&self, reference: &str) -> Option<String> {
        fn visit(
            ws: &WorkspaceSymbols,
            name: &str,
            active: &mut HashSet<String>,
        ) -> Option<String> {
            if ws.contains(name) {
                return Some(name.to_string());
            }
            if active.len() >= 128 || !active.insert(name.to_string()) {
                return None;
            }
            let result = (|| {
                let mut prefix = name;
                loop {
                    if let Some(targets) = ws.reference_aliases.get(prefix) {
                        let suffix = &name[prefix.len()..];
                        let mut found = None;
                        for target in targets {
                            let resolved = visit(ws, &format!("{target}{suffix}"), active)?;
                            if found.as_ref().is_some_and(|old| old != &resolved) {
                                return None;
                            }
                            found = Some(resolved);
                        }
                        return found;
                    }
                    prefix = prefix.rsplit_once('.')?.0;
                }
            })();
            active.remove(name);
            result
        }
        visit(self, reference, &mut HashSet::new())
    }

    /// Build the suffix index from the current `qnames` set. Must be called
    /// once after all qnames have been inserted, before any `find_by_suffix`
    /// calls. Safe to call multiple times (rebuilds from scratch each time).
    ///
    /// For every qname with N dot-separated components, indexes all tails
    /// of length 2..=N so that `"TypeName.method"` matches
    /// `"file.path.TypeName.method"`.
    pub fn build_suffix_index(&mut self) {
        self.suffix_index.clear();
        for qname in &self.qnames {
            let parts: Vec<&str> = qname.split('.').collect();
            // Only index tails with 2+ components (1-component is too ambiguous).
            for start in 1..parts.len() {
                let suffix = parts[start..].join(".");
                self.suffix_index
                    .entry(suffix)
                    .or_default()
                    .push(qname.clone());
            }
        }
    }

    /// Look up a qname by suffix (e.g., `"DriftCompiler.compile"`).
    ///
    /// Returns the unique full qname when exactly one workspace qname ends
    /// with `.<suffix>` or equals `suffix` exactly.  Returns `None` when no
    /// match is found or when the suffix is ambiguous (multiple matches).
    ///
    /// Requires [`build_suffix_index`] to have been called after the last
    /// batch of qname inserts.
    pub fn find_by_suffix<'a>(&'a self, suffix: &str) -> Option<&'a str> {
        // Exact match first (no prefix stripping needed).
        if let Some(q) = self.qnames.get(suffix) {
            return Some(q.as_str());
        }
        let matches = self.suffix_index.get(suffix)?;
        if matches.len() == 1 {
            Some(matches[0].as_str())
        } else {
            // Ambiguous — don't guess.
            None
        }
    }
}

/// Plan L t-006: one call site the static walker tried to resolve but
/// couldn't. May be stdlib (`print`, `len`), third-party (an
/// unindexed dependency), or genuinely dynamic. Aggregated by the
/// indexer to expose the static-resolution coverage gap so agents
/// don't assume missing edges are bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedCall {
    pub file: String,
    pub line: u32,
    /// The function-position text as it appears in source (e.g.
    /// `requests.get`, `Foo.bar`, `local_var`).
    pub callee_text: String,
}

/// Plan L t-005: one dynamic-dispatch call site detected by an adapter.
/// `pattern` is a short label (`getattr` / `callback-arg` / `metaclass`);
/// `snippet` is the source line trimmed for display in CLI warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicDispatchHint {
    pub file: String,
    pub line: u32,
    pub pattern: String,
    pub snippet: String,
}

/// Language-specific parsing + effect inference. Implementations live in
/// sibling crates: `agentstatedeveloper-python`, `-typescript`, etc.
pub trait LanguageAdapter: Send + Sync {
    /// Stable language identifier — "python", "typescript", ...
    fn language(&self) -> &str;

    /// File extensions this adapter handles, without the leading dot.
    /// Used by the shared index pipeline for file dispatch.
    /// Default: empty (adapter is never auto-selected by extension).
    fn file_extensions(&self) -> &'static [&'static str] {
        &[]
    }

    /// Parse a file and return declared symbols.
    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>>;

    /// Heuristic effect inference for a symbol. Returns an empty Vec when
    /// the adapter has no opinion (author must declare explicitly).
    fn infer_effects(&self, source: &str, symbol: &ParsedSymbol) -> Vec<Effect>;

    /// Plan L t-005: report dynamic-dispatch sites in the file's source.
    /// These are call patterns the static call-graph walker can't resolve
    /// (e.g. `getattr(obj, attr_name)(args)`, callback-by-argument,
    /// metaclass tricks). Returning a non-empty list signals to the
    /// indexer that some call paths in this file won't show up in the
    /// graph; the indexer surfaces a one-line warning so agents/humans
    /// know the missing edges aren't a bug, they're a known limit.
    ///
    /// Default: empty. Adapters with no dynamic-dispatch story return
    /// nothing.
    fn scan_dynamic_dispatch(&self, _file: &str, _source: &str) -> Vec<DynamicDispatchHint> {
        Vec::new()
    }

    /// Plan L t-006: report call sites in this file whose function
    /// position couldn't be resolved to a workspace qname. Default:
    /// empty. Adapters that can compute this cheaply override.
    fn report_unresolved_calls(
        &self,
        _file: &str,
        _source: &str,
        _symbols: &[ParsedSymbol],
        _workspace: &WorkspaceSymbols,
    ) -> Vec<UnresolvedCall> {
        Vec::new()
    }

    /// Extract call edges from a file's parsed symbols. Returned pairs are
    /// (caller_qname, callee_qname). The adapter is free to use heuristics;
    /// resolution quality is best-effort.
    ///
    /// `workspace` carries workspace-wide qname/kind context so adapters
    /// can resolve cross-module calls (via imports). Adapters that only
    /// care about intra-module edges may safely ignore it.
    fn extract_call_edges(
        &self,
        _file: &str,
        _source: &str,
        _symbols: &[ParsedSymbol],
        _workspace: &WorkspaceSymbols,
    ) -> Vec<CallEdge> {
        Vec::new()
    }

    /// Extract graph and optional site locations in one adapter pass.
    fn extract_call_graph(
        &self,
        file: &str,
        source: &str,
        symbols: &[ParsedSymbol],
        workspace: &WorkspaceSymbols,
    ) -> (Vec<CallEdge>, Vec<CallSite>) {
        (
            self.extract_call_edges(file, source, symbols, workspace),
            Vec::new(),
        )
    }

    /// Detect cross-service endpoints (HTTP routes/clients, pub-sub
    /// emit/listen) in a file. Returns [`DetectedEndpoint`]s attributed to the
    /// owning symbol by qname; the index pipeline enriches them with repo +
    /// symbol identity. Whole-file signature (not per-symbol) because the
    /// signal often lives just outside a symbol's body (e.g. a route decorator
    /// above a `def`). Default: empty — adapters opt in.
    ///
    /// See [`crate::cross_service`] for the contract-keyed matching model.
    fn infer_service_endpoints(
        &self,
        _file: &str,
        _source: &str,
        _symbols: &[ParsedSymbol],
    ) -> Vec<DetectedEndpoint> {
        Vec::new()
    }

    /// Project-level pass (Plan Q t-004b): rewrite endpoint contracts using
    /// cross-file context — e.g. resolving nested router-mount prefix chains
    /// (`app.include_router(api, prefix="/api")` → `api.include_router(calc,
    /// prefix="/calc")` → `@calc.get("/x")` = `/api/calc/x`) that span files.
    /// `files` is `(path, source)` for THIS adapter's language; `endpoints` is
    /// the full project set — the adapter must only touch its own. Runs once
    /// after per-file detection. Default: no-op.
    fn resolve_endpoint_prefixes(
        &self,
        _files: &[(String, String)],
        _endpoints: &mut Vec<crate::cross_service::ServiceEndpoint>,
    ) {
    }

    /// Detect intra-process data-flow sites (`arg → param` at call sites) in a
    /// file. Returns [`DetectedDataFlow`] naming caller/callee by qname plus the
    /// argument and its position; the index pipeline resolves the callee's
    /// parameter name and symbol identity. Default: empty — adapters opt in.
    ///
    /// See [`crate::dataflow`] for the model.
    fn extract_dataflow(
        &self,
        _file: &str,
        _source: &str,
        _symbols: &[ParsedSymbol],
        _workspace: &WorkspaceSymbols,
    ) -> Vec<DetectedDataFlow> {
        Vec::new()
    }

    /// Public reference aliases contributed by an adapter after all definitions and
    /// ownership metadata are known. Raw alias chains are resolved at call sites.
    fn extract_reference_aliases(
        &self,
        _file: &str,
        _source: &str,
        _symbols: &[ParsedSymbol],
        _workspace: &WorkspaceSymbols,
    ) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Extract stored property declarations from a file's parsed symbols,
    /// returning `"EnclosingTypeSimpleName.propertyName" → "TypeSimpleName"`.
    ///
    /// Called by the index pipeline for EVERY file in a run so that
    /// `workspace.properties` accumulates a project-wide property map.
    /// That map is then available to `extract_call_edges` to resolve
    /// instance property calls across file boundaries.
    ///
    /// Default: returns an empty map (most adapters don't need this).
    fn extract_property_types(&self, _symbols: &[ParsedSymbol]) -> HashMap<String, String> {
        HashMap::new()
    }
}

#[cfg(test)]
mod reference_alias_tests {
    use super::*;
    #[test]
    fn aliases_accept_convergent_paths_and_modules_but_reject_cycles_and_unknowns() {
        let mut ws = WorkspaceSymbols::default();
        ws.qnames.insert("doctor.inspect".into());
        for (alias, target) in [
            ("api.inspect", "doctor.inspect"),
            ("api.inspect", "bridge.inspect"),
            ("bridge.inspect", "doctor.inspect"),
            ("api.doctor", "doctor"),
            ("cycle.a", "cycle.b"),
            ("cycle.b", "cycle.a"),
        ] {
            ws.reference_aliases
                .entry(alias.into())
                .or_default()
                .insert(target.into());
        }
        assert_eq!(
            ws.resolve_reference("api.inspect").as_deref(),
            Some("doctor.inspect")
        );
        assert_eq!(
            ws.resolve_reference("api.doctor.inspect").as_deref(),
            Some("doctor.inspect")
        );
        assert_eq!(ws.resolve_reference("cycle.a"), None);
        ws.reference_aliases
            .get_mut("api.inspect")
            .unwrap()
            .insert("missing.inspect".into());
        assert_eq!(ws.resolve_reference("api.inspect"), None);
    }
}
