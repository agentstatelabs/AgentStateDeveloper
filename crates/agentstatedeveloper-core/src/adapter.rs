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
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSymbols {
    /// All qnames known to the indexer, flat.
    pub qnames: HashSet<String>,
    /// Map from qname to its kind, so adapters can distinguish
    /// class-level methods from module-level functions during resolution.
    pub kinds: HashMap<String, SymbolKind>,
}

impl WorkspaceSymbols {
    pub fn contains(&self, qname: &str) -> bool {
        self.qnames.contains(qname)
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
}
