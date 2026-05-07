use std::collections::{HashMap, HashSet};

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
}

impl WorkspaceSymbols {
    /// Exact qname membership test.
    pub fn contains(&self, qname: &str) -> bool {
        self.qnames.contains(qname)
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
