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

use chrono::Utc;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use serde_json::Value;

use crate::adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
use crate::audit::{AuditEvent, AuditSink, event_types};
use crate::error::{AsdError, Result};
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
        existing.into_iter().filter_map(|(lang, subtree)| {
            subtree.as_object().cloned().map(|m| (lang, m))
        }).collect()
    };

    let mut symbol_count = 0usize;
    let mut effect_count = 0usize;
    let mut qname_to_sym_id: HashMap<String, String> = HashMap::new();
    let mut all_edges: Vec<CallEdge> = Vec::new();
    let mut all_symbol_ids: Vec<String> = Vec::new();

    struct FileCtx {
        file_str: String,
        source: String,
        parsed: Vec<ParsedSymbol>,
        adapter: Arc<dyn LanguageAdapter>,
    }
    let mut file_ctxs: Vec<FileCtx> = Vec::with_capacity(files.len());

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
                symbol_fp: symbol_fp.clone(),
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

            // Accumulate into in-memory maps — no repo writes yet.
            by_qname.insert(p.qname.clone(), sym_val.clone());
            by_effects.insert(
                symbol_id.clone(),
                serde_json::to_value(&EffectDecl {
                    symbol_id: symbol_id.clone(),
                    declared: adapter.infer_effects(&source, p),
                    transitive: Vec::new(),
                    verification: Some(Verification {
                        by: VerificationSource::StaticChecker,
                        at: Utc::now(),
                        status: VerificationStatus::Unverified,
                        mismatches: Vec::new(),
                    }),
                    confidence: None,
                    matched_policy: None,
                })
                .map_err(|e| AsdError::Other(e.to_string()))?,
            );

            let code_key = format!(
                "{}/{}",
                paths::clean(&file_str),
                symbol_fp
            );
            by_code
                .entry(sym.language.clone())
                .or_default()
                .insert(code_key, sym_val);

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
    // Pass 2: extract call edges, resolve, write callees+callers as two
    // complete subtree writes → O(N) objects, 1 commit.
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
        f(&format!("  propagating transitive effects ({} edges)…", resolved_edge_count));
    }
    let transitive_updates = propagate_transitive_batched(
        repo,
        ref_name,
        &all_symbol_ids,
        &callees_of,
        agent_id,
    )?;

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
            serde_json::from_value::<EffectDecl>(v.clone()).ok().map(|d| (k.clone(), d))
        })
        .collect();

    let mut memo: HashMap<String, HashMap<EffectCategory, BTreeSet<String>>> = HashMap::new();
    let mut updates: Vec<(String, EffectDecl)> = Vec::new();

    for sym in symbol_ids {
        let mut stack: HashSet<String> = HashSet::new();
        let computed =
            compute_transitive_mem(callees_of, &effects_cache, sym, &mut memo, &mut stack);

        let Some(decl) = effects_cache.get(sym) else { continue; };

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
        .filter_map(|(k, v)| {
            serde_json::to_value(v).ok().map(|val| (k.clone(), val))
        })
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
                acc.entry(e.effect).or_default().insert(callee.clone());
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
