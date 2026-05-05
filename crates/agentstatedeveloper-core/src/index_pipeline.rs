//! Shared indexing pipeline used by both the CLI `asd index` command and the
//! MCP `reindex` tool. Pass concrete [`LanguageAdapter`] instances at the
//! call site; this module dispatches by file extension and does not depend on
//! any specific adapter crate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;

use crate::adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
use crate::audit::{AuditEvent, AuditSink, event_types};
use crate::effects::{AsgEffectStore, EffectStore};
use crate::error::{AsdError, Result};
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::detect_orphaned_entries;
use crate::paths;
use crate::schema::{
    EffectDecl, Position, Symbol, Verification, VerificationSource, VerificationStatus,
};
use crate::symbol::{canonical_symbol_id, symbol_fingerprint};
use crate::transitive::propagate_transitive;

use agentstategraph::Repository;

/// Summary returned by [`run_index`]. Matches the JSON shape emitted by both
/// the CLI and MCP surfaces.
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
}

/// Result of collecting source files under a path.
pub struct CollectResult {
    /// Files paired with the adapter that will handle them.
    pub recognized: Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    /// Files that had no matching adapter.
    pub skipped: Vec<PathBuf>,
}

/// Run the full index pipeline over `path` using the provided adapters.
///
/// Adapters are matched to source files by extension via
/// [`LanguageAdapter::file_extensions`]. Files with no matching adapter are
/// silently skipped. The pipeline runs two passes (per-file parse + persist,
/// then workspace-wide call-graph resolution), writes callee/caller indexes,
/// propagates transitive effects, and tags any orphaned ledger entries.
///
/// `progress` is called before each file is processed with `(file, index, total)`.
/// Pass `None` for no progress reporting.
pub fn run_index(
    repo: &Repository,
    ref_name: &str,
    path: &Path,
    agent_id: &str,
    adapters: &[Arc<dyn LanguageAdapter>],
    audit: Option<&dyn AuditSink>,
    progress: Option<&dyn Fn(&Path, usize, usize)>,
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

    let index_store = AsgIndexStore { repo };
    let effect_store = AsgEffectStore { repo };

    let mut symbol_count = 0usize;
    let mut effect_count = 0usize;
    let mut all_symbol_ids: Vec<String> = Vec::new();
    let mut all_edges: Vec<CallEdge> = Vec::new();

    struct FileCtx {
        file_str: String,
        source: String,
        parsed: Vec<ParsedSymbol>,
        adapter: Arc<dyn LanguageAdapter>,
    }
    let mut file_ctxs: Vec<FileCtx> = Vec::with_capacity(files.len());

    let total = files.len();
    // Pass 1: parse and persist symbols + effects per file.
    for (idx, (file, adapter)) in files.iter().enumerate() {
        if let Some(cb) = progress {
            cb(file, idx + 1, total);
        }
        let source = std::fs::read_to_string(file)
            .map_err(|e| AsdError::Other(format!("read {}: {}", file.display(), e)))?;
        let rel = file.strip_prefix(&index_root).unwrap_or(file);
        let file_str = rel.to_string_lossy().replace('\\', "/");

        let parsed = adapter.parse_symbols(&file_str, &source)?;

        for p in &parsed {
            let symbol_id = canonical_symbol_id(&p.qname, p.kind, &file_str);
            let symbol_fp = symbol_fingerprint(&p.body);
            let sym = Symbol {
                symbol_id: symbol_id.clone(),
                symbol_fp,
                qname: p.qname.clone(),
                language: adapter.language().to_string(),
                kind: p.kind,
                file: file_str.clone(),
                start: Position { line: p.start_line, col: p.start_col },
                end: Position { line: p.end_line, col: p.end_col },
                signature: p.signature.clone(),
            };
            index_store.put_symbol(ref_name, &sym, agent_id)?;
            symbol_count += 1;
            all_symbol_ids.push(symbol_id.clone());

            let declared = adapter.infer_effects(&source, p);
            let decl = EffectDecl {
                symbol_id: symbol_id.clone(),
                declared,
                transitive: Vec::new(),
                verification: Some(Verification {
                    by: VerificationSource::StaticChecker,
                    at: Utc::now(),
                    status: VerificationStatus::Unverified,
                    mismatches: Vec::new(),
                }),
                confidence: None,
                matched_policy: None,
            };
            effect_store.put_effects(ref_name, &symbol_id, &decl, agent_id)?;
            effect_count += 1;
        }

        file_ctxs.push(FileCtx { file_str, source, parsed, adapter: Arc::clone(adapter) });
    }

    // Build workspace-wide qname context for cross-module call resolution.
    let mut workspace = WorkspaceSymbols::default();
    for ctx in &file_ctxs {
        for p in &ctx.parsed {
            workspace.qnames.insert(p.qname.clone());
            workspace.kinds.insert(p.qname.clone(), p.kind);
        }
    }

    // Pass 2: extract call edges with workspace context.
    for ctx in &file_ctxs {
        let edges = ctx.adapter.extract_call_edges(
            &ctx.file_str, &ctx.source, &ctx.parsed, &workspace,
        );
        all_edges.extend(edges);
    }

    // Resolve qname → symbol_id for each edge and aggregate into caller/callee maps.
    let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut callers_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_edge_count = 0usize;
    let mut cross_module_edges = 0usize;
    for edge in &all_edges {
        let caller_sym = match index_store.get_symbol_by_qname(ref_name, &edge.caller_qname)? {
            Some(s) => s.symbol_id,
            None => continue,
        };
        let callee_sym = match index_store.get_symbol_by_qname(ref_name, &edge.callee_qname)? {
            Some(s) => s.symbol_id,
            None => continue,
        };
        let cs = callees_of.entry(caller_sym.clone()).or_default();
        if !cs.contains(&callee_sym) { cs.push(callee_sym.clone()); }
        let rs = callers_of.entry(callee_sym).or_default();
        if !rs.contains(&caller_sym) { rs.push(caller_sym); }
        resolved_edge_count += 1;
        if !same_module(&edge.caller_qname, &edge.callee_qname) {
            cross_module_edges += 1;
        }
    }
    let intra_module_edges = resolved_edge_count.saturating_sub(cross_module_edges);

    for v in callees_of.values_mut() { v.sort(); }
    for v in callers_of.values_mut() { v.sort(); }

    for (sym_id, callees) in &callees_of {
        let path = paths::callees_path(sym_id);
        let value = serde_json::json!({ "callees": callees });
        let opts = CommitOptions::new(agent_id, IntentCategory::Refine, format!("write callees for {sym_id}"));
        repo.set_json(ref_name, &path, &value, opts)
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    for (sym_id, callers) in &callers_of {
        let path = paths::callers_path(sym_id);
        let value = serde_json::json!({ "callers": callers });
        let opts = CommitOptions::new(agent_id, IntentCategory::Refine, format!("write callers for {sym_id}"));
        repo.set_json(ref_name, &path, &value, opts)
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }

    let transitive_updates =
        propagate_transitive(&index_store, &effect_store, ref_name, &all_symbol_ids)?;

    let orphaned_tagged = detect_orphaned_entries(repo, ref_name, agent_id)?;

    if let Some(sink) = audit {
        let event = AuditEvent::new(event_types::INDEX_RUN, agent_id, "agent", "allow")
            .with_payload(serde_json::json!({
                "path": path.to_string_lossy(),
                "files": files.len(),
                "symbols": symbol_count,
                "effects": effect_count,
                "edges": resolved_edge_count,
                "transitive_updates": transitive_updates,
                "orphaned_tagged": orphaned_tagged,
            }));
        let _ = sink.emit(&event);
    }

    Ok(IndexSummary {
        files: files.len(),
        skipped: skipped_files.len(),
        symbols: symbol_count,
        effects: effect_count,
        edges: resolved_edge_count,
        intra_module_edges,
        cross_module_edges,
        transitive_updates,
        orphaned_tagged,
    })
}

fn same_module(caller: &str, callee: &str) -> bool {
    let cm = caller.split('.').next().unwrap_or("");
    let ee = callee.split('.').next().unwrap_or("");
    !cm.is_empty() && cm == ee
}

/// Collect source files under `root`, pairing each with the first adapter
/// whose [`LanguageAdapter::file_extensions`] includes the file's extension.
/// Files with no matching adapter are collected into `CollectResult::skipped`.
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
        return Ok(CollectResult { recognized, skipped });
    }
    walk(root, adapters, &mut recognized, &mut skipped)?;
    Ok(CollectResult { recognized, skipped })
}

fn adapter_for_path(
    p: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
) -> Option<Arc<dyn LanguageAdapter>> {
    let ext = p.extension().and_then(|s| s.to_str())?;
    adapters.iter().find(|a| a.file_extensions().contains(&ext)).cloned()
}

fn walk(
    dir: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
    out: &mut Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    skipped: &mut Vec<PathBuf>,
) -> Result<()> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| AsdError::Other(format!("read_dir {}: {}", dir.display(), e)))?;
    for entry in rd {
        let entry = entry.map_err(|e| AsdError::Other(e.to_string()))?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|e| AsdError::Other(e.to_string()))?;
        if ft.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                name,
                ".git" | ".venv" | "venv" | "__pycache__" | "node_modules"
                    | ".tox" | ".mypy_cache" | "dist" | "build" | ".next"
            ) {
                continue;
            }
            walk(&path, adapters, out, skipped)?;
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
