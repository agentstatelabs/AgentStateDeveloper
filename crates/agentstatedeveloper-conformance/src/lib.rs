//! Cross-language conformance harness for AgentStateDeveloper.
//!
//! Per-crate unit tests prove each adapter parses *its own* language. They
//! cannot prove two things this harness exists to prove:
//!
//! 1. **Adapter parity** — every adapter in [`default_adapters`] implements the
//!    same set of `LanguageAdapter` capabilities. When a trait method is added
//!    (e.g. `infer_service_endpoints`), nothing fails today if one adapter
//!    silently returns the default empty `Vec`. The capability matrix turns
//!    that into a visible red cell.
//!
//! 2. **Cross-language contracts** — the whole point of the contract-keyed
//!    cross-service layer is that a Python client and a Go server normalize to
//!    the *same* `http:GET /users/{}` string. No per-crate test can check that,
//!    because each crate only knows one language.
//!
//! ## Design rule: assert invariants, not outputs
//!
//! Per CLAUDE.md (the 1.0.85→1.0.88 cliff arc), scores shift at every pipeline
//! stage and across versions. This harness therefore asserts *structural*
//! facts only — "a symbol was found", "an inbound HTTP endpoint was detected",
//! "the contract matched" — never an exact score or a full-output snapshot.
//!
//! ## The matrix is a spec, not a wish
//!
//! Known gaps (e.g. Swift has no outbound HTTP client detector yet — tracked as
//! competitive-harvest t-016) are encoded as `false` in [`expected_matrix`].
//! A capability that *regresses* flips a cell and fails. A gap that gets
//! *fixed* also flips a cell and fails — forcing whoever closed it to update
//! the spec. The table is the single source of truth for "what every adapter
//! can do today."

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use agentstatedeveloper_core::adapter::{LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
use agentstatedeveloper_core::cross_service::{
    match_edges, CrossServiceEdge, Direction, ServiceEndpoint, Transport,
};
use agentstatedeveloper_core::schema::EffectCategory;
use agentstatedeveloper_adapters::default_adapters;

pub mod fixtures;

/// One capability cell per `LanguageAdapter` trait method we exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `parse_symbols` returned at least one declared symbol.
    pub symbols: bool,
    /// `infer_effects` returned at least one non-`Pure` effect for some symbol.
    pub effects: bool,
    /// `extract_call_edges` resolved at least one intra-file caller→callee edge.
    pub call_edges: bool,
    /// `infer_service_endpoints` detected an inbound HTTP route.
    pub inbound_http: bool,
    /// `infer_service_endpoints` detected an outbound HTTP client call.
    pub outbound_http: bool,
}

impl Capabilities {
    /// Render as a fixed-width row of ✓/✗ for the printed matrix.
    pub fn cells(&self) -> [bool; 5] {
        [
            self.symbols,
            self.effects,
            self.call_edges,
            self.inbound_http,
            self.outbound_http,
        ]
    }
}

/// Column headers, in the same order as [`Capabilities::cells`].
pub const COLUMNS: [&str; 5] = ["symbols", "effects", "call_edges", "inbound", "outbound"];

/// Build a [`WorkspaceSymbols`] from a single file's parsed symbols so
/// `extract_call_edges` can resolve intra-file calls by suffix.
fn workspace_for(symbols: &[ParsedSymbol]) -> WorkspaceSymbols {
    let mut ws = WorkspaceSymbols {
        qnames: symbols.iter().map(|s| s.qname.clone()).collect::<HashSet<_>>(),
        kinds: symbols
            .iter()
            .map(|s| (s.qname.clone(), s.kind.clone()))
            .collect::<HashMap<_, _>>(),
        suffix_index: HashMap::new(),
        properties: HashMap::new(),
    };
    ws.build_suffix_index();
    ws
}

/// Run every capability probe for one adapter against its conformance fixture.
pub fn probe(adapter: &dyn LanguageAdapter, file: &str, source: &str) -> Capabilities {
    let symbols = adapter.parse_symbols(file, source).unwrap_or_default();

    let effects = symbols.iter().any(|s| {
        adapter
            .infer_effects(source, s)
            .iter()
            .any(|e| e.effect != EffectCategory::Pure)
    });

    let ws = workspace_for(&symbols);
    let call_edges = !adapter
        .extract_call_edges(file, source, &symbols, &ws)
        .is_empty();

    let endpoints = adapter.infer_service_endpoints(file, source, &symbols);
    let inbound_http = endpoints
        .iter()
        .any(|e| e.direction == Direction::Inbound && e.transport == Transport::Http);
    let outbound_http = endpoints
        .iter()
        .any(|e| e.direction == Direction::Outbound && e.transport == Transport::Http);

    Capabilities {
        symbols: !symbols.is_empty(),
        effects,
        call_edges,
        inbound_http,
        outbound_http,
    }
}

/// The full set of inbound HTTP `ServiceEndpoint`s a fixture exposes, tagged
/// with a synthetic repo id — for the cross-language contract test.
pub fn inbound_endpoints(
    adapter: &dyn LanguageAdapter,
    file: &str,
    source: &str,
    repo_id: &str,
) -> Vec<ServiceEndpoint> {
    let symbols = adapter.parse_symbols(file, source).unwrap_or_default();
    adapter
        .infer_service_endpoints(file, source, &symbols)
        .into_iter()
        .filter(|e| e.direction == Direction::Inbound && e.transport == Transport::Http)
        .enumerate()
        .map(|(i, e)| e.into_endpoint(repo_id, &format!("{repo_id}#in{i}")))
        .collect()
}

/// The outbound counterpart of [`inbound_endpoints`].
pub fn outbound_endpoints(
    adapter: &dyn LanguageAdapter,
    file: &str,
    source: &str,
    repo_id: &str,
) -> Vec<ServiceEndpoint> {
    let symbols = adapter.parse_symbols(file, source).unwrap_or_default();
    adapter
        .infer_service_endpoints(file, source, &symbols)
        .into_iter()
        .filter(|e| e.direction == Direction::Outbound && e.transport == Transport::Http)
        .enumerate()
        .map(|(i, e)| e.into_endpoint(repo_id, &format!("{repo_id}#out{i}")))
        .collect()
}

/// Match every fixture's inbound against every other fixture's outbound.
pub fn cross_repo_edges(across: &[ServiceEndpoint]) -> Vec<CrossServiceEdge> {
    match_edges(across)
        .into_iter()
        .filter(|e| e.cross_repo)
        .collect()
}

/// Look up the conformance fixture for a language id (matches
/// `adapter.language()`), returning `(filename, source)`.
pub fn fixture_for(language: &str) -> Option<(&'static str, &'static str)> {
    fixtures::ALL
        .iter()
        .find(|f| f.language == language)
        .map(|f| (f.file, f.source))
}

/// Compute the live capability matrix for all built-in adapters, paired with
/// each adapter's language id. Adapters without a fixture are skipped (and the
/// matrix test asserts none are).
pub fn live_matrix() -> Vec<(String, Capabilities)> {
    default_adapters()
        .iter()
        .filter_map(|a| {
            let lang = a.language().to_string();
            fixture_for(&lang).map(|(file, source)| (lang, probe(a.as_ref(), file, source)))
        })
        .collect()
}

/// The expected capability matrix — the conformance *spec*. `(language,
/// [symbols, effects, call_edges, inbound, outbound])`.
///
/// Filled from the first print-only run, then any deviation (regression OR a
/// closed gap) fails the matrix test until this table is updated to match.
pub fn expected_matrix() -> Vec<(&'static str, [bool; 5])> {
    // Columns: [symbols, effects, call_edges, inbound_http, outbound_http].
    // All capabilities present for every language EXCEPT `swift.outbound`:
    // Swift has no cross-service outbound client detector yet (URLSession is
    // recognized as a network *effect* but not wired into the contract layer).
    // Tracked as competitive-harvest t-016. When that lands, flip the last
    // Swift cell to `true` — this test will fail until you do, by design.
    let t = true;
    vec![
        ("python", [t, t, t, t, t]),
        ("typescript", [t, t, t, t, t]),
        ("rust", [t, t, t, t, t]),
        ("go", [t, t, t, t, t]),
        ("java", [t, t, t, t, t]),
        ("csharp", [t, t, t, t, t]),
        ("ruby", [t, t, t, t, t]),
        ("kotlin", [t, t, t, t, t]),
        ("swift", [t, t, t, t, false]),
    ]
}

// ---------------------------------------------------------------------------
// Tier-2 realism: run the full pipeline over a REAL source tree.
//
// Tier-1 (the matrix above) proves each adapter handles code we wrote to
// trigger it. Tier-2 proves the adapters survive code we DIDN'T write — the
// incidental mess of real projects (deep nesting, odd Unicode, huge files,
// route-like strings inside comments). It asserts only coarse invariants:
// nothing panics, and aggregate counts clear a floor. Never exact values.
// ---------------------------------------------------------------------------

/// Coarse stats from running the adapter pipeline over a file tree.
#[derive(Debug, Default, Clone)]
pub struct TreeStats {
    pub files_parsed: usize,
    pub symbols: usize,
    pub files_with_effects: usize,
    pub call_edges: usize,
    pub inbound_endpoints: usize,
    pub outbound_endpoints: usize,
    /// Files whose pipeline panicked — MUST be empty. Each entry names the
    /// file and the stage, so a real-world panic is actionable, not a mystery.
    pub panicked_files: Vec<String>,
    /// `(language, files_parsed)` for the printed summary.
    pub by_language: Vec<(String, usize)>,
}

/// Recursively collect files under `root`, skipping build/VCS/vendor and
/// hidden directories.
pub fn collect_source_files(root: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "target",
        "node_modules",
        "vendor",
        "dist",
        "build",
        "__pycache__",
        ".git",
    ];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Run parse → effects → call-edges → endpoints over every recognized file in
/// `root`, dispatching by file extension to the built-in adapters. Each
/// per-file call is wrapped in `catch_unwind` so a panic is recorded against
/// the offending file+stage rather than aborting the whole pass.
pub fn run_pipeline_over_tree(root: &Path) -> TreeStats {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let adapters = default_adapters();
    let mut by_ext: HashMap<&'static str, usize> = HashMap::new();
    for (i, a) in adapters.iter().enumerate() {
        for ext in a.file_extensions() {
            by_ext.insert(ext, i);
        }
    }

    struct ParsedFile {
        file: String,
        source: String,
        symbols: Vec<ParsedSymbol>,
    }

    let mut stats = TreeStats::default();
    let mut per_adapter: HashMap<usize, Vec<ParsedFile>> = HashMap::new();

    // Parse pass.
    for path in collect_source_files(root) {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Some(&ai) = by_ext.get(ext) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file = path.to_string_lossy().into_owned();
        let a = adapters[ai].as_ref();
        match catch_unwind(AssertUnwindSafe(|| {
            a.parse_symbols(&file, &source).unwrap_or_default()
        })) {
            Ok(symbols) => {
                stats.files_parsed += 1;
                stats.symbols += symbols.len();
                per_adapter.entry(ai).or_default().push(ParsedFile {
                    file,
                    source,
                    symbols,
                });
            }
            Err(_) => stats.panicked_files.push(format!("{file} :: parse_symbols")),
        }
    }

    // Per-adapter: build a workspace from all of that language's files (so
    // cross-file call resolution behaves like the real index pipeline), then
    // run effects + call-edges + endpoints per file.
    for (ai, parsed) in &per_adapter {
        let a = adapters[*ai].as_ref();
        stats.by_language.push((a.language().to_string(), parsed.len()));

        let mut ws = WorkspaceSymbols {
            qnames: HashSet::new(),
            kinds: HashMap::new(),
            suffix_index: HashMap::new(),
            properties: HashMap::new(),
        };
        for pf in parsed {
            for s in &pf.symbols {
                ws.qnames.insert(s.qname.clone());
                ws.kinds.insert(s.qname.clone(), s.kind.clone());
            }
            for (k, v) in a.extract_property_types(&pf.symbols) {
                ws.properties.insert(k, v);
            }
        }
        ws.build_suffix_index();

        for pf in parsed {
            match catch_unwind(AssertUnwindSafe(|| {
                pf.symbols.iter().any(|s| {
                    a.infer_effects(&pf.source, s)
                        .iter()
                        .any(|e| e.effect != EffectCategory::Pure)
                })
            })) {
                Ok(true) => stats.files_with_effects += 1,
                Ok(false) => {}
                Err(_) => stats
                    .panicked_files
                    .push(format!("{} :: infer_effects", pf.file)),
            }

            match catch_unwind(AssertUnwindSafe(|| {
                a.extract_call_edges(&pf.file, &pf.source, &pf.symbols, &ws)
                    .len()
            })) {
                Ok(n) => stats.call_edges += n,
                Err(_) => stats
                    .panicked_files
                    .push(format!("{} :: extract_call_edges", pf.file)),
            }

            match catch_unwind(AssertUnwindSafe(|| {
                a.infer_service_endpoints(&pf.file, &pf.source, &pf.symbols)
            })) {
                Ok(eps) => {
                    stats.inbound_endpoints += eps
                        .iter()
                        .filter(|e| e.direction == Direction::Inbound)
                        .count();
                    stats.outbound_endpoints += eps
                        .iter()
                        .filter(|e| e.direction == Direction::Outbound)
                        .count();
                }
                Err(_) => stats
                    .panicked_files
                    .push(format!("{} :: infer_service_endpoints", pf.file)),
            }
        }
    }

    stats.by_language.sort();
    stats
}
