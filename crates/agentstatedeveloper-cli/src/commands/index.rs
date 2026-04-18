//! `asd index <path>` — walk a directory for `*.py` files, parse them
//! with the Python adapter, and write Symbol + EffectDecl records into
//! the ASG.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use serde_json::json;

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;

use agentstatedeveloper_core::{
    canonical_symbol_id, paths, propagate_transitive, symbol_fingerprint, AsgEffectStore,
    AsgIndexStore, CallEdge, EffectDecl, EffectStore, Engine, IndexStore, LanguageAdapter,
    Position, Symbol, Verification, VerificationSource, VerificationStatus,
};
use agentstatedeveloper_python::PythonAdapter;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Directory (or file) to index. Recursively walks for `*.py`.
    pub path: PathBuf,
}

pub fn run(cfg: &Config, args: IndexArgs) -> Result<()> {
    let mut engine = Engine::open_sqlite(&cfg.db_path)?;
    let adapter = Arc::new(PythonAdapter::new());
    let adapter_dyn: Arc<dyn agentstatedeveloper_core::LanguageAdapter> = adapter.clone();
    engine.register_adapter(adapter_dyn);

    let files = collect_py_files(&args.path)?;
    let index_root = if args.path.is_dir() {
        args.path.clone()
    } else {
        args.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    };

    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };

    let mut symbol_count: usize = 0;
    let mut effect_count: usize = 0;
    let mut all_symbol_ids: Vec<String> = Vec::new();
    let mut all_edges: Vec<CallEdge> = Vec::new();

    for file in &files {
        let source = std::fs::read_to_string(file)
            .with_context(|| format!("read {}", file.display()))?;
        let rel = file.strip_prefix(&index_root).unwrap_or(file);
        let file_str = rel.to_string_lossy().replace('\\', "/");

        let parsed = adapter.parse_symbols(&file_str, &source)?;

        for p in &parsed {
            let symbol_id = canonical_symbol_id(&p.qname, p.kind, &file_str);
            let symbol_fp = symbol_fingerprint(&p.body);
            let symbol = Symbol {
                symbol_id: symbol_id.clone(),
                symbol_fp,
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
            };

            index_store.put_symbol(&engine.ref_name, &symbol, &cfg.agent_id)?;
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
            effect_store.put_effects(&engine.ref_name, &symbol_id, &decl, &cfg.agent_id)?;
            effect_count += 1;
        }

        let edges = adapter.extract_call_edges(&file_str, &source, &parsed);
        all_edges.extend(edges);
    }

    // Resolve qname -> symbol_id for both ends of each edge and aggregate
    // into per-symbol callees / callers maps. Edges where either side can't
    // be resolved (cross-module, unknown name, etc.) are silently dropped.
    let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut callers_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_edge_count: usize = 0;
    for edge in &all_edges {
        let caller_sym = match index_store
            .get_symbol_by_qname(&engine.ref_name, &edge.caller_qname)?
        {
            Some(s) => s.symbol_id,
            None => continue,
        };
        let callee_sym = match index_store
            .get_symbol_by_qname(&engine.ref_name, &edge.callee_qname)?
        {
            Some(s) => s.symbol_id,
            None => continue,
        };
        let cs = callees_of.entry(caller_sym.clone()).or_default();
        if !cs.contains(&callee_sym) {
            cs.push(callee_sym.clone());
        }
        let rs = callers_of.entry(callee_sym).or_default();
        if !rs.contains(&caller_sym) {
            rs.push(caller_sym);
        }
        resolved_edge_count += 1;
    }

    // Sort each list for deterministic on-disk content (and friendlier diffs).
    for v in callees_of.values_mut() {
        v.sort();
    }
    for v in callers_of.values_mut() {
        v.sort();
    }

    for (sym_id, callees) in &callees_of {
        let path = paths::callees_path(sym_id);
        let value = json!({ "callees": callees });
        let opts = CommitOptions::new(
            &cfg.agent_id,
            IntentCategory::Refine,
            format!("write callees for {sym_id}"),
        );
        engine
            .repo
            .set_json(&engine.ref_name, &path, &value, opts)?;
    }
    for (sym_id, callers) in &callers_of {
        let path = paths::callers_path(sym_id);
        let value = json!({ "callers": callers });
        let opts = CommitOptions::new(
            &cfg.agent_id,
            IntentCategory::Refine,
            format!("write callers for {sym_id}"),
        );
        engine
            .repo
            .set_json(&engine.ref_name, &path, &value, opts)?;
    }

    // Now that callees/callers are persisted, the transitive pass can walk
    // the graph via AsgIndexStore::get_callees and surface real effects.
    let transitive_updates =
        propagate_transitive(&index_store, &effect_store, &engine.ref_name, &all_symbol_ids)?;

    let summary = json!({
        "files": files.len(),
        "symbols": symbol_count,
        "effects": effect_count,
        "edges": resolved_edge_count,
        "transitive_updates": transitive_updates,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// Recursively collect `*.py` files under `root`. If `root` is itself a
/// `.py` file, return just that.
fn collect_py_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if root.is_file() {
        if is_py(root) {
            out.push(root.to_path_buf());
        }
        return Ok(out);
    }
    walk(root, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let rd = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            // Skip common noise dirs.
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                name,
                ".git" | ".venv" | "venv" | "__pycache__" | "node_modules" | ".tox" | ".mypy_cache"
            ) {
                continue;
            }
            walk(&path, out)?;
        } else if ft.is_file() && is_py(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_py(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()) == Some("py")
}
