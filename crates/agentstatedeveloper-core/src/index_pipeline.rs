//! Shared indexing pipeline used by both the CLI `asd index` command and the
//! MCP `reindex` tool.
//!
//! # Performance design
//!
//! ## Old approach (O(N²) storage)
//! Using `spec_set_json` per symbol caused structural sharing to work against
//! us: every write rebuilt the growing `by-qname` Map node (1 entry, 2
//! entries, … N entries). 13 000 symbols × 3 paths = 39 000 growing copies
//! → 59 GB DB for a 1 341-file project.
//!
//! ## New approach (O(N) storage)
//! Each pass assembles the **complete** subtree JSON in memory, then writes
//! it with a **single** `spec_set_json` call per prefix. `json_to_tree`
//! creates the Map node exactly once with all N entries.
//!
//!   1. Pass 1 — symbols + effect declarations
//!      • `/asd/v1/index/by-qname`  (merged with existing)
//!      • `/asd/v1/effects`          (merged with existing)
//!      • `/asd/v1/code`             (merged with existing)
//!   2. Pass 2 — callee / caller edge lists
//!      • `/asd/v1/index/callees`
//!      • `/asd/v1/index/callers`
//!   3. Transitive — updated EffectDecl.transitive fields
//!      • `/asd/v1/effects`          (merged with Pass-1 state)
//!
//! Total object count is O(N) regardless of repo size.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use chrono::Utc;
use serde_json::Value;

use crate::adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
use crate::audit::{AuditEvent, AuditSink, event_types};
use crate::doc_adapters::{adapt_document, is_doc_file};
use crate::error::{AsdError, Result};
use crate::ledger::detect_orphaned_entries;
use crate::paths;
use crate::schema::{
    EffectCategory, EffectDecl, Position, Symbol, TransitiveEffect, Verification,
    VerificationSource, VerificationStatus,
};
use crate::search_fts::{SearchDocsDb, SearchFtsDb};
use crate::symbol::{canonical_symbol_id, symbol_fingerprint};

use agentstategraph::Repository;

/// Summary returned by [`run_index`].
#[derive(Debug, Clone, Default)]
pub struct IndexSummary {
    pub files: usize,
    pub skipped: usize,
    pub symbols: usize,
    pub effects: usize,
    pub edges: usize,
    pub intra_module_edges: usize,
    pub cross_module_edges: usize,
    pub transitive_updates: usize,
    pub orphaned_tagged: usize,
    /// Number of symbols that received a :line suffix to resolve a same-file
    /// qname collision.  0 means the index is collision-free.
    pub disambiguated: usize,
    /// Top cross-file qname collisions: (qname, first_file, second_file).
    /// Only populated when collisions occur; capped at 10 for display.
    pub top_collisions: Vec<(String, String, String)>,
    /// Number of document files processed by document adapters.
    pub doc_files: usize,
    /// Total document chunks indexed into asd_search_docs.
    pub docs_indexed: usize,
    /// Plan L t-005: dynamic-dispatch sites detected by adapters
    /// (`getattr(obj, x)(…)`, `__getattr__`, etc.). These are call
    /// patterns the static walker can't resolve into edges; surfaced
    /// so agents/humans know the missing edges are by design.
    pub dynamic_dispatch_sites: usize,
    /// Top dynamic-dispatch hits, capped at 5 for display.
    pub dynamic_dispatch_samples: Vec<crate::adapter::DynamicDispatchHint>,
    /// Plan L t-006: call sites the static resolver couldn't bind to
    /// a workspace qname. Includes stdlib (minus a known allowlist),
    /// third-party, and dynamic — treat as a "static-resolution gap"
    /// signal, not a bug count.
    pub dropped_call_edges: usize,
    /// Top unresolved calls, capped at 5 for display.
    pub sample_unresolved: Vec<crate::adapter::UnresolvedCall>,
}

/// Result of collecting source files under a path.
pub struct CollectResult {
    pub recognized: Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    pub skipped: Vec<PathBuf>,
}

/// Run the full index pipeline over `path`.
///
/// All writes are batched into three commits (symbols, edges, transitive)
/// regardless of repo size, with O(N) total object cost.
///
/// `progress` is called before each file: `(file, index, total)`.
/// `on_phase` is called when post-processing phases begin, with a short
/// human-readable description (e.g. `"building call graph…"`).
/// Pass `None` for either to suppress that output.
pub fn run_index(
    repo: &Repository,
    ref_name: &str,
    path: &Path,
    agent_id: &str,
    adapters: &[Arc<dyn LanguageAdapter>],
    audit: Option<&dyn AuditSink>,
    progress: Option<&dyn Fn(&Path, usize, usize)>,
    on_phase: Option<&dyn Fn(&str)>,
    db_path: Option<&Path>,
) -> Result<IndexSummary> {
    let collected = collect_source_files(path, adapters)?;
    let files = collected.recognized;
    let skipped_files = collected.skipped;
    let index_root = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };

    // -----------------------------------------------------------------------
    // Pass 1: parse symbols + effects, assemble complete subtrees in memory,
    // write as a single spec per prefix → O(N) objects, 1 commit.
    // -----------------------------------------------------------------------
    let total = files.len();

    // Seed from existing state so incremental re-index preserves prior data.
    let mut by_qname: serde_json::Map<String, Value> = repo
        .get_tree(ref_name, "/asd/v1/index/by-qname")
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let mut by_effects: serde_json::Map<String, Value> = repo
        .get_tree(ref_name, "/asd/v1/effects")
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    // code tree: lang → { "clean_file/symbol_fp" → Symbol }
    // Seed from existing state.
    let mut by_code: BTreeMap<String, serde_json::Map<String, Value>> = {
        let existing = repo
            .get_tree(ref_name, "/asd/v1/code")
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        existing
            .into_iter()
            .filter_map(|(lang, subtree)| subtree.as_object().cloned().map(|m| (lang, m)))
            .collect()
    };

    let mut symbol_count = 0usize;
    let mut disambiguated_count = 0usize;
    let mut all_edges: Vec<CallEdge> = Vec::new();
    // Track first-seen file for each qname to report cross-file collisions.
    let mut qname_first_file: HashMap<String, String> = HashMap::new();
    let mut collision_log: Vec<(String, String, String)> = Vec::new();

    // Pre-populate qname_to_sym_id from previously-indexed symbols so that
    // cross-package call edges (caller in this run → callee from a prior run)
    // are preserved.  The parsing loop below will overwrite entries for any
    // symbol that is re-indexed in the current run.
    let mut qname_to_sym_id: HashMap<String, String> = by_qname
        .iter()
        .filter_map(|(qname, sym_val)| {
            sym_val
                .get("symbol_id")
                .and_then(|v| v.as_str())
                .map(|id| (qname.clone(), id.to_string()))
        })
        .collect();

    struct FileCtx {
        file_str: String,
        source: String,
        parsed: Vec<ParsedSymbol>,
        adapter: Arc<dyn LanguageAdapter>,
    }
    let mut file_ctxs: Vec<FileCtx> = Vec::with_capacity(files.len());
    let mut indexed_symbols: Vec<Symbol> = Vec::new();
    // Plan L t-005: aggregate dynamic-dispatch hints across all files.
    let mut all_dynamic_dispatch: Vec<crate::adapter::DynamicDispatchHint> = Vec::new();

    for (idx, (file, adapter)) in files.iter().enumerate() {
        if let Some(cb) = progress {
            cb(file, idx + 1, total);
        }
        let source = std::fs::read_to_string(file)
            .map_err(|e| AsdError::Other(format!("read {}: {}", file.display(), e)))?;
        let rel = file.strip_prefix(&index_root).unwrap_or(file);
        let file_str = rel.to_string_lossy().replace('\\', "/");

        let mut parsed = adapter.parse_symbols(&file_str, &source)?;
        disambiguated_count += disambiguate_qnames(&mut parsed);

        // Plan L t-005: detect dynamic-dispatch sites the call-graph
        // walker can't resolve. Default impl returns empty, so this
        // is a no-op for adapters without a story.
        all_dynamic_dispatch.extend(adapter.scan_dynamic_dispatch(&file_str, &source));

        for p in &parsed {
            let symbol_id = canonical_symbol_id(&p.qname, p.kind, &file_str);
            let symbol_fp = symbol_fingerprint(&p.body);
            let sym = Symbol {
                symbol_id: symbol_id.clone(),
                symbol_fp: symbol_fp.clone(),
                qname: p.qname.clone(),
                language: adapter.language().to_string(),
                kind: p.kind,
                file: file_str.clone(),
                start: Position {
                    line: p.start_line,
                    col: p.start_col,
                },
                end: Position {
                    line: p.end_line,
                    col: p.end_col,
                },
                signature: p.signature.clone(),
                doc: p.doc.clone(),
            };

            let sym_val = serde_json::to_value(&sym).map_err(|e| AsdError::Other(e.to_string()))?;

            // Accumulate into in-memory maps — no repo writes yet.
            // Detect collisions: same qname parsed from two different files.
            if let Some(prev_file) = qname_first_file.get(&p.qname) {
                if prev_file != &file_str {
                    collision_log.push((p.qname.clone(), prev_file.clone(), file_str.clone()));
                }
            } else {
                qname_first_file.insert(p.qname.clone(), file_str.clone());
            }
            by_qname.insert(p.qname.clone(), sym_val.clone());
            let inferred = adapter.infer_effects(&source, p);
            // Effects returned by the static checker were confirmed from
            // source — mark Ok. An empty list means the adapter couldn't
            // determine effects (not that it confirmed purity), so stay
            // Unverified until a runtime trace or manual declaration says more.
            let verification_status = if inferred.is_empty() {
                VerificationStatus::Unverified
            } else {
                VerificationStatus::Ok
            };
            by_effects.insert(
                symbol_id.clone(),
                serde_json::to_value(&EffectDecl {
                    symbol_id: symbol_id.clone(),
                    declared: inferred,
                    transitive: Vec::new(),
                    verification: Some(Verification {
                        by: VerificationSource::StaticChecker,
                        at: Utc::now(),
                        status: verification_status,
                        mismatches: Vec::new(),
                    }),
                    confidence: None,
                    runtime: None,
                    matched_policy: None,
                })
                .map_err(|e| AsdError::Other(e.to_string()))?,
            );

            let code_key = format!("{}/{}", paths::clean(&file_str), symbol_fp);
            by_code
                .entry(sym.language.clone())
                .or_default()
                .insert(code_key, sym_val);

            qname_to_sym_id.insert(p.qname.clone(), symbol_id.clone());
            symbol_count += 1;
            indexed_symbols.push(sym);
        }

        file_ctxs.push(FileCtx {
            file_str,
            source,
            parsed,
            adapter: Arc::clone(adapter),
        });
    }

    let unique_symbol_count = by_qname.len();
    let unique_effect_count = by_effects.len();
    if disambiguated_count > 0 || !collision_log.is_empty() {
        if disambiguated_count > 0 {
            eprintln!(
                "  note: disambiguated {} same-file qname collision(s) with :line suffix",
                disambiguated_count,
            );
        }
        for (qname, f1, f2) in collision_log.iter().take(5) {
            eprintln!("    cross-file collision: {qname:?}  {f1}  ↔  {f2}");
        }
        if collision_log.len() > 5 {
            eprintln!(
                "    … and {} more cross-file collisions",
                collision_log.len() - 5
            );
        }
    }

    if let Some(f) = on_phase {
        f(&format!(
            "  {} files parsed — committing symbols + effects…",
            symbol_count
        ));
    }

    // -----------------------------------------------------------------------
    // Build workspace-wide qname context for cross-module call resolution.
    //
    // Seed from the FULL by-qname map — which at this point contains both
    // previously-indexed symbols (seeded from the repo at the start of Pass 1)
    // AND the symbols parsed in this run.  This allows cross-package edges to
    // resolve: e.g., when indexing ExampleFlow, calls to DriftCompiler.compile
    // resolve because SequencerCore was indexed in a prior run and its symbols
    // are already in by_qname.
    //
    // Must happen BEFORE the Pass 1 commit because spec_set_json consumes
    // by_qname (moves it into a Value::Object).
    // -----------------------------------------------------------------------
    let mut workspace = WorkspaceSymbols::default();
    for (qname, sym_val) in &by_qname {
        workspace.qnames.insert(qname.clone());
        // Extract kind from the serialized Symbol JSON (e.g. "method", "class").
        if let Some(kind_str) = sym_val.get("kind").and_then(|v| v.as_str()) {
            if let Ok(kind) = serde_json::from_value::<crate::schema::SymbolKind>(
                serde_json::Value::String(kind_str.to_string()),
            ) {
                workspace.kinds.insert(qname.clone(), kind);
            }
        }
    }
    // Build suffix index after all qnames are inserted so adapters can do
    // O(1) suffix-based lookup (e.g., "DriftCompiler.compile" →
    // "Sources.Models.DriftCompiler.compile").
    workspace.build_suffix_index();

    // Populate the workspace property map from ALL files so that instance
    // property calls (e.g., `pool.resolve()` where `pool: DriftSynthPool` is
    // declared in a different file) can be resolved across file boundaries.
    // Each adapter contributes its language-specific property extraction.
    for ctx in &file_ctxs {
        let props = ctx.adapter.extract_property_types(&ctx.parsed);
        workspace.properties.extend(props);
    }

    // Build the nested code tree JSON: { lang: { "file/fp": Symbol, … }, … }
    let code_tree: serde_json::Map<String, Value> = by_code
        .into_iter()
        .map(|(lang, subtree)| (lang, Value::Object(subtree)))
        .collect();

    // Flush Pass 1: 3 spec_set_json calls (complete subtrees) → O(N) objects.
    let spec1 = repo
        .speculate(ref_name, Some("asd-index-pass1".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    repo.spec_set_json(spec1, "/asd/v1/index/by-qname", &Value::Object(by_qname))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    repo.spec_set_json(spec1, "/asd/v1/effects", &Value::Object(by_effects))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    if !code_tree.is_empty() {
        repo.spec_set_json(spec1, "/asd/v1/code", &Value::Object(code_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    let opts1 = CommitOptions::new(
        agent_id,
        IntentCategory::Checkpoint,
        format!(
            "asd index: {} symbols across {} files",
            unique_symbol_count,
            files.len()
        ),
    );
    repo.commit_speculation(spec1, opts1)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    // -----------------------------------------------------------------------
    // Pass 2: extract call edges, resolve, write callees+callers as two
    // complete subtree writes → O(N) objects, 1 commit.
    // -----------------------------------------------------------------------
    // Rebuild all_symbol_ids from the winning qname→sym_id mapping so that
    // transitive propagation only processes symbol_ids that are actually
    // present in by_effects (avoids wasted DFS over orphaned IDs from qname
    // collisions where the loser's symbol_id was pushed but never "won" the
    // by_qname slot).
    let all_symbol_ids: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        qname_to_sym_id
            .values()
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect()
    };

    if let Some(f) = on_phase {
        f("  building call graph…");
    }
    // Plan L t-006: aggregate unresolved-call hints across all files.
    let mut all_unresolved: Vec<crate::adapter::UnresolvedCall> = Vec::new();
    for ctx in &file_ctxs {
        let edges =
            ctx.adapter
                .extract_call_edges(&ctx.file_str, &ctx.source, &ctx.parsed, &workspace);
        all_edges.extend(edges);
        // Per-file unresolved-call report. Default trait impl returns
        // empty for adapters that don't implement static resolution.
        all_unresolved.extend(ctx.adapter.report_unresolved_calls(
            &ctx.file_str,
            &ctx.source,
            &ctx.parsed,
            &workspace,
        ));
    }

    let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut callers_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_edge_count = 0usize;
    let mut cross_module_edges = 0usize;

    for edge in &all_edges {
        let Some(caller_sym) = qname_to_sym_id.get(&edge.caller_qname) else {
            continue;
        };
        let Some(callee_sym) = qname_to_sym_id.get(&edge.callee_qname) else {
            continue;
        };
        let cs = callees_of.entry(caller_sym.clone()).or_default();
        if !cs.contains(callee_sym) {
            cs.push(callee_sym.clone());
        }
        let rs = callers_of.entry(callee_sym.clone()).or_default();
        if !rs.contains(caller_sym) {
            rs.push(caller_sym.clone());
        }
        resolved_edge_count += 1;
        if !same_module(&edge.caller_qname, &edge.callee_qname) {
            cross_module_edges += 1;
        }
    }
    let intra_module_edges = resolved_edge_count.saturating_sub(cross_module_edges);

    for v in callees_of.values_mut() {
        v.sort();
    }
    for v in callers_of.values_mut() {
        v.sort();
    }

    // Assemble complete callees / callers subtrees in memory.
    let callees_tree: serde_json::Map<String, Value> = callees_of
        .iter()
        .map(|(sym_id, callees)| (sym_id.clone(), serde_json::json!({ "callees": callees })))
        .collect();
    let callers_tree: serde_json::Map<String, Value> = callers_of
        .iter()
        .map(|(sym_id, callers)| (sym_id.clone(), serde_json::json!({ "callers": callers })))
        .collect();

    let spec2 = repo
        .speculate(ref_name, Some("asd-index-pass2-edges".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    if !callees_tree.is_empty() {
        repo.spec_set_json(spec2, "/asd/v1/index/callees", &Value::Object(callees_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    if !callers_tree.is_empty() {
        repo.spec_set_json(spec2, "/asd/v1/index/callers", &Value::Object(callers_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    let opts2 = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("asd index: {} call edges", resolved_edge_count),
    );
    repo.commit_speculation(spec2, opts2)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    // -----------------------------------------------------------------------
    // Transitive effect propagation — fully in-memory, then one bulk write.
    // -----------------------------------------------------------------------
    if let Some(f) = on_phase {
        f(&format!(
            "  propagating transitive effects ({} edges)…",
            resolved_edge_count
        ));
    }
    let transitive_updates =
        propagate_transitive_batched(repo, ref_name, &all_symbol_ids, &callees_of, agent_id)?;

    let orphaned_tagged = detect_orphaned_entries(repo, ref_name, agent_id)?;

    if let Some(sink) = audit {
        let event = AuditEvent::new(event_types::INDEX_RUN, agent_id, "agent", "allow")
            .with_payload(serde_json::json!({
                "path": path.to_string_lossy(),
                "files": files.len(),
                "symbols": unique_symbol_count,
                "effects": unique_effect_count,
                "edges": resolved_edge_count,
                "transitive_updates": transitive_updates,
                "orphaned_tagged": orphaned_tagged,
            }));
        let _ = sink.emit(&event);
    }

    // FTS5 atomic rebuild — replace the entire index from the current snapshot.
    // Full rebuild avoids stale rows from deleted or renamed files; the indexer
    // already owns the complete world, so incremental tracking adds no benefit.
    // Errors are non-fatal; search falls back to in-memory until next index.
    //
    // Deduplicate by qname before rebuilding so FTS row count matches the ASG
    // by-qname tree count (by_qname silently overwrites duplicates; FTS must
    // mirror that behaviour so `asd status` and `asd list stats` agree).
    if let Some(db) = db_path {
        if let Some(f) = on_phase {
            f("  rebuilding FTS search index…");
        }

        // M59: build ledger_data map (symbol_id → (ledger_text, ledger_flags))
        // from the ledger tree so FTS rows carry denormalized summaries.
        // One get_tree call, no per-symbol git reads needed later.
        let ledger_data: HashMap<String, (String, String)> = {
            use crate::schema::{LedgerEntry, LedgerKind};
            let ledger_prefix = format!("{}/ledger", crate::paths::ASD_ROOT);
            match repo.get_tree(ref_name, &ledger_prefix) {
                Ok(serde_json::Value::Object(by_symbol)) => {
                    let mut map = HashMap::with_capacity(by_symbol.len());
                    for (sym_id, per_symbol) in by_symbol {
                        if let serde_json::Value::Object(entries_map) = per_symbol {
                            let mut texts: Vec<String> = Vec::new();
                            let mut flags: std::collections::BTreeSet<&'static str> =
                                std::collections::BTreeSet::new();
                            for (_entry_id, v) in entries_map {
                                if let Ok(entry) = serde_json::from_value::<LedgerEntry>(v) {
                                    if !entry.summary.is_empty() {
                                        texts.push(entry.summary.to_lowercase());
                                    }
                                    match entry.kind {
                                        LedgerKind::Ownership => {
                                            flags.insert("ownership");
                                        }
                                        LedgerKind::Invariant => {
                                            flags.insert("invariant");
                                        }
                                        LedgerKind::Hazard => {
                                            flags.insert("hazard");
                                        }
                                        LedgerKind::Decision => {
                                            flags.insert("decision");
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if !texts.is_empty() || !flags.is_empty() {
                                map.insert(
                                    sym_id,
                                    (
                                        texts.join(" "),
                                        flags.into_iter().collect::<Vec<_>>().join(","),
                                    ),
                                );
                            }
                        }
                    }
                    map
                }
                _ => HashMap::new(),
            }
        };

        let fts_ok = match SearchFtsDb::open(db) {
            Ok(fts) => {
                // Keep last-seen symbol per qname, matching by_qname semantics.
                let mut seen: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for (i, sym) in indexed_symbols.iter().enumerate() {
                    seen.insert(sym.qname.as_str(), i);
                }
                let mut deduped: Vec<&Symbol> =
                    seen.values().map(|&i| &indexed_symbols[i]).collect();
                deduped.sort_by(|a, b| a.qname.cmp(&b.qname));
                if let Err(e) = fts.rebuild_refs(&deduped, &ledger_data) {
                    eprintln!("asd: FTS rebuild warning: {e}");
                    false
                } else {
                    true
                }
            }
            Err(e) => {
                eprintln!("asd: FTS index unavailable (non-fatal): {e}");
                false
            }
        };

        // Record the FTS rebuild outcome so `asd status` can distinguish
        // "symbols fresh / FTS stale" from a fully-fresh index. Best-effort:
        // a second open may also fail if the DB is still locked, in which case
        // stale_warning() falls back to the previous behaviour (old timestamp).
        if let Ok(meta) = SearchFtsDb::open(db) {
            let _ = meta.mark_symbols_indexed(fts_ok);
        }

        // Populate the symbol and call-edge SQLite caches so subsequent
        // `callers`, `callees`, `context-for`, and `investigate` calls can
        // skip the full git tree walk entirely.  Non-fatal: a cache miss just
        // falls back to the authoritative git path.
        if let Ok(cache) = SearchFtsDb::open(db) {
            if let Some(f) = on_phase {
                f("  caching symbols and edges…");
            }
            let sym_refs: Vec<&Symbol> = indexed_symbols.iter().collect();
            if let Err(e) = cache.sync_symbols(&sym_refs, ref_name) {
                eprintln!("asd: symbol cache sync warning: {e}");
            }
            if let Err(e) = cache.sync_call_edges(&callees_of, &callers_of, ref_name) {
                eprintln!("asd: edge cache sync warning: {e}");
            }
        }
    }

    // Document search index — walk the index root for doc-adapter files and
    // rebuild asd_search_docs in one atomic pass (full replace, like symbol FTS).
    let mut doc_files_count = 0usize;
    let mut docs_indexed_count = 0usize;
    if let Some(db) = db_path {
        if let Some(f) = on_phase {
            f("  rebuilding document search index…");
        }
        let mut all_docs = Vec::new();
        collect_doc_files_recursive(&index_root, &mut all_docs, &mut doc_files_count);
        docs_indexed_count = all_docs.len();
        match SearchDocsDb::open(db) {
            Ok(docs_db) => {
                if let Err(e) = docs_db.rebuild(&all_docs) {
                    eprintln!("asd: document index rebuild warning: {e}");
                }
            }
            Err(e) => {
                eprintln!("asd: document index unavailable (non-fatal): {e}");
            }
        }
    }

    let top_collisions = collision_log.into_iter().take(10).collect();
    Ok(IndexSummary {
        files: files.len(),
        skipped: skipped_files.len(),
        symbols: unique_symbol_count,
        effects: unique_effect_count,
        edges: resolved_edge_count,
        intra_module_edges,
        cross_module_edges,
        transitive_updates,
        orphaned_tagged,
        disambiguated: disambiguated_count,
        top_collisions,
        doc_files: doc_files_count,
        docs_indexed: docs_indexed_count,
        dynamic_dispatch_sites: all_dynamic_dispatch.len(),
        dynamic_dispatch_samples: {
            let mut v = all_dynamic_dispatch;
            v.truncate(5);
            v
        },
        dropped_call_edges: all_unresolved.len(),
        sample_unresolved: {
            let mut v = all_unresolved;
            v.truncate(5);
            v
        },
    })
}

/// Walk a directory recursively, collect doc chunks from all recognised document files.
/// Skips hidden dirs, .git, target/, node_modules/, and binary-looking files.
fn collect_doc_files_recursive(
    root: &Path,
    out: &mut Vec<crate::search_fts::SearchDoc>,
    file_count: &mut usize,
) {
    let skip_dirs = [
        "target",
        "node_modules",
        ".git",
        ".build",
        "DerivedData",
        "dist",
        ".cache",
    ];
    let dir = match std::fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return,
    };
    for entry in dir.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if skip_dirs.contains(&name.as_str()) {
                continue;
            }
            collect_doc_files_recursive(&path, out, file_count);
        } else if is_doc_file(&path) {
            *file_count += 1;
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Some(docs) = adapt_document(&path, &content) {
                    out.extend(docs);
                }
            }
        }
    }
}

/// Append `:line` to the qname of every symbol that collides within a single
/// file's parse output.  Only symbols that actually collide are touched —
/// unique qnames are left unchanged so existing ledger/call-graph data is
/// not invalidated.  Returns the number of symbols that were renamed.
fn disambiguate_qnames(parsed: &mut Vec<ParsedSymbol>) -> usize {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in parsed.iter() {
        *counts.entry(p.qname.clone()).or_insert(0) += 1;
    }
    let mut renamed = 0usize;
    for p in parsed.iter_mut() {
        if counts.get(&p.qname).copied().unwrap_or(0) > 1 {
            p.qname = format!("{}:{}", p.qname, p.start_line);
            renamed += 1;
        }
    }
    renamed
}

/// Compute transitive effects entirely in memory, then flush changed
/// EffectDecls as a single `spec_set_json` call → O(N) objects.
///
/// Takes `callees_of` from the in-memory Pass-2 map to avoid repo reads
/// during the DFS.
fn propagate_transitive_batched(
    repo: &Repository,
    ref_name: &str,
    symbol_ids: &[String],
    callees_of: &HashMap<String, Vec<String>>,
    agent_id: &str,
) -> Result<usize> {
    // Read the complete effects tree once.
    let effects_tree = repo
        .get_tree(ref_name, "/asd/v1/effects")
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    // Deserialize into a local cache for fast access.
    let mut effects_cache: HashMap<String, EffectDecl> = effects_tree
        .iter()
        .filter_map(|(k, v)| {
            serde_json::from_value::<EffectDecl>(v.clone())
                .ok()
                .map(|d| (k.clone(), d))
        })
        .collect();

    let mut memo: HashMap<String, HashMap<EffectCategory, BTreeSet<String>>> = HashMap::new();
    let mut updates: Vec<(String, EffectDecl)> = Vec::new();

    for sym in symbol_ids {
        let mut stack: HashSet<String> = HashSet::new();
        let computed =
            compute_transitive_mem(callees_of, &effects_cache, sym, &mut memo, &mut stack);

        let Some(decl) = effects_cache.get(sym) else {
            continue;
        };

        let declared_cats: HashSet<EffectCategory> =
            decl.declared.iter().map(|e| e.effect.clone()).collect();

        let mut new_transitive: Vec<TransitiveEffect> = computed
            .into_iter()
            .filter(|(cat, _)| !declared_cats.contains(cat))
            .map(|(cat, via_set)| TransitiveEffect {
                effect: cat,
                via: via_set.into_iter().collect(),
                qualifiers: serde_json::Value::Null,
            })
            .collect();

        new_transitive.sort_by(|a, b| {
            a.effect
                .as_str()
                .cmp(b.effect.as_str())
                .then_with(|| a.via.cmp(&b.via))
        });

        if !transitive_eq(&decl.transitive, &new_transitive) {
            let mut updated = decl.clone();
            updated.transitive = new_transitive;
            updates.push((sym.clone(), updated));
        }
    }

    let updated = updates.len();
    if updated == 0 {
        return Ok(0);
    }

    // Apply updates to the local cache, then rebuild the complete effects map.
    for (sym_id, decl) in &updates {
        effects_cache.insert(sym_id.clone(), decl.clone());
    }

    let effects_map: serde_json::Map<String, Value> = effects_cache
        .iter()
        .filter_map(|(k, v)| serde_json::to_value(v).ok().map(|val| (k.clone(), val)))
        .collect();

    let spec = repo
        .speculate(ref_name, Some("asd-index-transitive".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    repo.spec_set_json(spec, "/asd/v1/effects", &Value::Object(effects_map))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    let opts = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("asd index: transitive effects for {} symbols", updated),
    );
    repo.commit_speculation(spec, opts)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    Ok(updated)
}

/// DFS over the in-memory `callees_of` map — no repo reads.
fn compute_transitive_mem(
    callees_of: &HashMap<String, Vec<String>>,
    effects: &HashMap<String, EffectDecl>,
    sym: &str,
    memo: &mut HashMap<String, HashMap<EffectCategory, BTreeSet<String>>>,
    stack: &mut HashSet<String>,
) -> HashMap<EffectCategory, BTreeSet<String>> {
    if let Some(cached) = memo.get(sym) {
        return cached.clone();
    }
    if stack.contains(sym) {
        return HashMap::new();
    }
    stack.insert(sym.to_string());

    let mut acc: HashMap<EffectCategory, BTreeSet<String>> = HashMap::new();
    let empty = Vec::new();
    let callees = callees_of.get(sym).unwrap_or(&empty);

    for callee in callees {
        if let Some(decl) = effects.get(callee) {
            for e in &decl.declared {
                acc.entry(e.effect.clone())
                    .or_default()
                    .insert(callee.clone());
            }
        }
        let callee_trans = compute_transitive_mem(callees_of, effects, callee, memo, stack);
        for (cat, _) in callee_trans {
            acc.entry(cat).or_default().insert(callee.clone());
        }
    }

    stack.remove(sym);
    memo.insert(sym.to_string(), acc.clone());
    acc
}

fn transitive_eq(a: &[TransitiveEffect], b: &[TransitiveEffect]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let to_key = |t: &TransitiveEffect| {
        let mut via = t.via.clone();
        via.sort();
        (t.effect.clone(), via)
    };
    let mut a_keys: Vec<_> = a.iter().map(to_key).collect();
    let mut b_keys: Vec<_> = b.iter().map(to_key).collect();
    a_keys.sort();
    b_keys.sort();
    a_keys == b_keys
}

fn same_module(caller: &str, callee: &str) -> bool {
    let cm = caller.split('.').next().unwrap_or("");
    let ee = callee.split('.').next().unwrap_or("");
    !cm.is_empty() && cm == ee
}

/// Collect source files under `root`, respecting built-in exclusions and an
/// optional `.asdignore` file in `root` (one directory-name pattern per line).
pub fn collect_source_files(
    root: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
) -> Result<CollectResult> {
    let mut recognized = Vec::new();
    let mut skipped = Vec::new();
    if root.is_file() {
        if let Some(adapter) = adapter_for_path(root, adapters) {
            recognized.push((root.to_path_buf(), adapter));
        } else {
            skipped.push(root.to_path_buf());
        }
        return Ok(CollectResult {
            recognized,
            skipped,
        });
    }
    let extra_excludes = load_asdignore(root);
    walk(
        root,
        adapters,
        &mut recognized,
        &mut skipped,
        &extra_excludes,
    )?;
    Ok(CollectResult {
        recognized,
        skipped,
    })
}

/// Read `.asdignore` from `root` and return non-empty, non-comment lines.
fn load_asdignore(root: &Path) -> Vec<String> {
    let path = root.join(".asdignore");
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn adapter_for_path(
    p: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
) -> Option<Arc<dyn LanguageAdapter>> {
    let ext = p.extension().and_then(|s| s.to_str())?;
    adapters
        .iter()
        .find(|a| a.file_extensions().contains(&ext))
        .cloned()
}

/// Built-in directory names that are always excluded from indexing.
fn is_builtin_excluded(name: &str) -> bool {
    matches!(
        name,
        // VCS / tooling
        ".git" | ".svn" | ".hg"
        // Python
        | ".venv" | "venv" | "__pycache__" | ".tox" | ".mypy_cache"
        // JS / TS
        | "node_modules" | "dist" | ".next" | ".turbo"
        // Rust
        | "target"
        // Swift / Xcode
        | ".build" | "DerivedData" | "xcuserdata" | ".xcodeproj"
        // Generic build outputs
        | "build" | "out" | ".cache"
        // Claude / AI worktrees
        | ".claude"
        // ASD's own state dir
        | ".asd"
    )
}

fn walk(
    dir: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
    out: &mut Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    skipped: &mut Vec<PathBuf>,
    extra_excludes: &[String],
) -> Result<()> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| AsdError::Other(format!("read_dir {}: {}", dir.display(), e)))?;
    for entry in rd {
        let entry = entry.map_err(|e| AsdError::Other(e.to_string()))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| AsdError::Other(e.to_string()))?;
        if ft.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if is_builtin_excluded(name) {
                continue;
            }
            if extra_excludes.iter().any(|pat| name == pat.as_str()) {
                continue;
            }
            walk(&path, adapters, out, skipped, extra_excludes)?;
        } else if ft.is_file() {
            if let Some(adapter) = adapter_for_path(&path, adapters) {
                out.push((path, adapter));
            } else {
                skipped.push(path);
            }
        }
    }
    Ok(())
}
