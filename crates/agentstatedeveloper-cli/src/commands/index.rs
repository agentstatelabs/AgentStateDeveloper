//! `asd index <path>` — walk a directory for source files we have
//! adapters for, parse them, and write Symbol + EffectDecl records into
//! the ASG.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{run_index, Engine, LanguageAdapter};
use agentstatedeveloper_python::PythonAdapter;
use agentstatedeveloper_typescript::TypeScriptAdapter;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Directory (or file) to index. Recursively walks for known source
    /// extensions (`.py`, `.ts`, `.tsx`, `.mts`, `.cts`).
    pub path: PathBuf,
}

pub fn run(cfg: &Config, args: IndexArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let adapters: Vec<Arc<dyn LanguageAdapter>> = vec![
        Arc::new(PythonAdapter::new()),
        Arc::new(TypeScriptAdapter::new()),
    ];

    let summary = run_index(&engine.repo, &engine.ref_name, &args.path, &cfg.agent_id, &adapters, Some(engine.audit.as_ref()))?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "files": summary.files,
            "symbols": summary.symbols,
            "effects": summary.effects,
            "edges": summary.edges,
            "intra_module_edges": summary.intra_module_edges,
            "cross_module_edges": summary.cross_module_edges,
            "transitive_updates": summary.transitive_updates,
            "orphaned_tagged": summary.orphaned_tagged,
        }))?
    );
    Ok(())
}
