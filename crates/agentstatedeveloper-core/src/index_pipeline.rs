//! Shared indexing pipeline used by both the CLI `asd index` command and the
//! MCP `reindex` tool.
//!
//! # Performance design
//!
//! Each `repo.set_json` call creates one git-style commit (resolve HEAD,
//! tree-copy, write commit, update ref). Naive per-symbol writes produce
//! O(n) commits with O(n²) total cost on large repos.
//!
//! This pipeline batches every write phase via the stategraph speculation
//! API: accumulate all changes in-memory, then flush as **one** commit per
//! phase. Three commits are created regardless of repo size:
//!
//!   1. Pass 1 — symbols + effect declarations
//!   2. Pass 2 — callee / caller edge lists
//!   3. Transitive — updated EffectDecl.transitive fields
//!
//! Pass 2 edge resolution uses an in-memory qname→symbol_id map built
//! during Pass 1, avoiding per-edge `get_symbol_by_qname` repo reads.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;

use crate::adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
use crate::audit::{AuditEvent, AuditSink, event_types};
use crate::error::{AsdError, Result};
use crate::index::AsgIndexStore;
use crate::ledger::detect_orphaned_entries;
use crate::paths;
use crate::schema::{
    EffectCategory, EffectDecl, Position, Symbol, TransitiveEffect, Verification,
    VerificationSource, VerificationStatus,
};
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
}

/// Result of collecting source files under a path.
pub struct CollectResult {
    pub recognized: Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    pub skipped: Vec<PathBuf>,
}

/// Run the full index pipeline over `path`.
///
/// All writes are batched into three commits (symbols, edges, transitive)
/// regardless of repo size, keeping cost O(n) rather than O(n²).
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

    let mut symbol_count = 0usize;
    let mut effect_count = 0usize;
    let mut all_symbol_ids: Vec<String> = Vec::new();
    let mut all_edges: Vec<CallEdge> = Vec::new();
    // In-memory qname→symbol_id map built during Pass 1; used for O(1)
    // edge resolution in Pass 2 instead of per-edge repo reads.
    let mut qname_to_sym_id: HashMap<String, String> = HashMap::new();

    struct FileCtx {
        file_str: String,
        source: String,
        parsed: Vec<ParsedSymbol>,
        adapter: Arc<dyn LanguageAdapter>,
    }
    let mut file_ctxs: Vec<FileCtx> = Vec::with_capacity(files.len());

    // -----------------------------------------------------------------------
    // Pass 1: parse symbols + effects, batch-write via speculation → 1 commit
    // -----------------------------------------------------------------------
    let spec1 = repo
        .speculate(ref_name, Some("asd-index-pass1".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;

    let total = files.len();
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

            let sym_val = serde_json::to_value(&sym)
                .map_err(|e| AsdError::Other(e.to_string()))?;

            // Write symbol to both storage paths in the speculation.
            let code_path = paths::code_path(&sym.language, &sym.file, &sym.symbol_fp);
            let qname_path = paths::qname_index_path(&sym.qname);
            repo.spec_set_json(spec1, &code_path, &sym_val)
                .map_err(|e| AsdError::Other(e.to_string()))?;
            repo.spec_set_json(spec1, &qname_path, &sym_val)
                .map_err(|e| AsdError::Other(e.to_string()))?;

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
            let eff_val = serde_json::to_value(&decl)
                .map_err(|e| AsdError::Other(e.to_string()))?;
            let eff_path = paths::effects_path(&symbol_id);
            repo.spec_set_json(spec1, &eff_path, &eff_val)
                .map_err(|e| AsdError::Other(e.to_string()))?;

            qname_to_sym_id.insert(p.qname.clone(), symbol_id.clone());
            all_symbol_ids.push(symbol_id);
            symbol_count += 1;
            effect_count += 1;
        }

        file_ctxs.push(FileCtx { file_str, source, parsed, adapter: Arc::clone(adapter) });
    }

    if let Some(f) = on_phase {
        f(&format!("  {} files parsed — committing symbols + effects…", symbol_count));
    }

    // Flush Pass 1 as a single commit.
    let opts1 = CommitOptions::new(
        agent_id,
        IntentCategory::Checkpoint,
        format!("asd index: {} symbols across {} files", symbol_count, files.len()),
    );
    repo.commit_speculation(spec1, opts1)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    // -----------------------------------------------------------------------
    // Build workspace-wide qname context for cross-module call resolution.
    // -----------------------------------------------------------------------
    let mut workspace = WorkspaceSymbols::default();
    for ctx in &file_ctxs {
        for p in &ctx.parsed {
            workspace.qnames.insert(p.qname.clone());
            workspace.kinds.insert(p.qname.clone(), p.kind);
        }
    }

    // -----------------------------------------------------------------------
    // Pass 2: extract call edges, resolve via in-memory map, batch-write
    // callee/caller lists → 1 commit.
    // -----------------------------------------------------------------------
    if let Some(f) = on_phase {
        f("  building call graph…");
    }
    for ctx in &file_ctxs {
        let edges = ctx.adapter.extract_call_edges(
            &ctx.file_str, &ctx.source, &ctx.parsed, &workspace,
        );
        all_edges.extend(edges);
    }

    let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut callers_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_edge_count = 0usize;
    let mut cross_module_edges = 0usize;

    for edge in &all_edges {
        // Use the in-memory map — no repo reads needed.
        let Some(caller_sym) = qname_to_sym_id.get(&edge.caller_qname) else { continue; };
        let Some(callee_sym) = qname_to_sym_id.get(&edge.callee_qname) else { continue; };
        let cs = callees_of.entry(caller_sym.clone()).or_default();
        if !cs.contains(callee_sym) { cs.push(callee_sym.clone()); }
        let rs = callers_of.entry(callee_sym.clone()).or_default();
        if !rs.contains(caller_sym) { rs.push(caller_sym.clone()); }
        resolved_edge_count += 1;
        if !same_module(&edge.caller_qname, &edge.callee_qname) {
            cross_module_edges += 1;
        }
    }
    let intra_module_edges = resolved_edge_count.saturating_sub(cross_module_edges);

    for v in callees_of.values_mut() { v.sort(); }
    for v in callers_of.values_mut() { v.sort(); }

    let spec2 = repo
        .speculate(ref_name, Some("asd-index-pass2-edges".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;

    for (sym_id, callees) in &callees_of {
        let path = paths::callees_path(sym_id);
        repo.spec_set_json(spec2, &path, &serde_json::json!({ "callees": callees }))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    for (sym_id, callers) in &callers_of {
        let path = paths::callers_path(sym_id);
        repo.spec_set_json(spec2, &path, &serde_json::json!({ "callers": callers }))
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
    // Transitive effect propagation: compute in-memory via DFS, then
    // batch-write only changed EffectDecls → 1 commit.
    // -----------------------------------------------------------------------
    if let Some(f) = on_phase {
        f(&format!("  propagating transitive effects ({} edges)…", resolved_edge_count));
    }
    let index_store = AsgIndexStore { repo };
    let transitive_updates =
        propagate_transitive_batched(repo, &index_store, ref_name, &all_symbol_ids, agent_id)?;

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

/// Compute transitive effects for all `symbol_ids`, then flush all changed
/// `EffectDecl` records as a single speculation commit.
fn propagate_transitive_batched(
    repo: &Repository,
    index: &AsgIndexStore,
    ref_name: &str,
    symbol_ids: &[String],
    agent_id: &str,
) -> Result<usize> {
    use crate::effects::{AsgEffectStore, EffectStore};

    let effect_store = AsgEffectStore { repo };

    // --- DFS to compute all transitive maps (reads only) ---
    let mut memo: HashMap<String, HashMap<EffectCategory, BTreeSet<String>>> = HashMap::new();

    // Collect (symbol_id, updated EffectDecl) pairs for changed symbols.
    let mut updates: Vec<(String, EffectDecl)> = Vec::new();

    for sym in symbol_ids {
        let mut stack: HashSet<String> = HashSet::new();
        let computed =
            compute_transitive(index, &effect_store, ref_name, sym, &mut memo, &mut stack)?;

        let Some(mut decl) = effect_store.get_effects(ref_name, sym)? else {
            continue;
        };

        let declared_cats: HashSet<EffectCategory> =
            decl.declared.iter().map(|e| e.effect).collect();

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
            a.effect.as_str().cmp(b.effect.as_str()).then_with(|| a.via.cmp(&b.via))
        });

        if !transitive_eq(&decl.transitive, &new_transitive) {
            decl.transitive = new_transitive;
            updates.push((sym.clone(), decl));
        }
    }

    let updated = updates.len();
    if updated == 0 {
        return Ok(0);
    }

    // --- Batch-write all changed EffectDecls in one commit ---
    let spec = repo
        .speculate(ref_name, Some("asd-index-transitive".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;

    for (sym_id, decl) in &updates {
        let path = paths::effects_path(sym_id);
        let val = serde_json::to_value(decl).map_err(|e| AsdError::Other(e.to_string()))?;
        repo.spec_set_json(spec, &path, &val)
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }

    let opts = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("asd index: transitive effects for {} symbols", updated),
    );
    repo.commit_speculation(spec, opts)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    Ok(updated)
}

fn compute_transitive(
    index: &AsgIndexStore,
    effects: &crate::effects::AsgEffectStore,
    ref_name: &str,
    sym: &str,
    memo: &mut HashMap<String, HashMap<EffectCategory, BTreeSet<String>>>,
    stack: &mut HashSet<String>,
) -> Result<HashMap<EffectCategory, BTreeSet<String>>> {
    use crate::effects::EffectStore;
    use crate::index::IndexStore;

    if let Some(cached) = memo.get(sym) {
        return Ok(cached.clone());
    }
    if stack.contains(sym) {
        return Ok(HashMap::new());
    }
    stack.insert(sym.to_string());

    let mut acc: HashMap<EffectCategory, BTreeSet<String>> = HashMap::new();
    let callees = index.get_callees(ref_name, sym)?;

    for callee in &callees {
        if let Some(decl) = effects.get_effects(ref_name, callee)? {
            for e in &decl.declared {
                acc.entry(e.effect).or_default().insert(callee.clone());
            }
        }
        let callee_transitive =
            compute_transitive(index, effects, ref_name, callee, memo, stack)?;
        for (cat, _) in callee_transitive {
            acc.entry(cat).or_default().insert(callee.clone());
        }
    }

    stack.remove(sym);
    memo.insert(sym.to_string(), acc.clone());
    Ok(acc)
}

fn transitive_eq(a: &[TransitiveEffect], b: &[TransitiveEffect]) -> bool {
    if a.len() != b.len() { return false; }
    let to_key = |t: &TransitiveEffect| {
        let mut via = t.via.clone();
        via.sort();
        (t.effect, via)
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

/// Collect source files under `root`.
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
