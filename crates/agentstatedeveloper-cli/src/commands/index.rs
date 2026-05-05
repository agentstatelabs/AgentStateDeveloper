//! `asd index <path>` — walk a directory for source files we have
//! adapters for, parse them, and write Symbol + EffectDecl records into
//! the ASG.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_adapters::default_adapters;
use agentstatedeveloper_core::{collect_source_files, run_index, Engine};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Directory (or file) to index. Recursively walks for known source
    /// extensions (`.py`, `.ts`, `.tsx`, `.rs`, `.go`, `.java`, `.cs`, `.rb`, `.kt`, `.swift`).
    pub path: PathBuf,

    /// Print each file as it is indexed, and list skipped files.
    #[arg(short, long)]
    pub verbose: bool,
}

pub fn run(cfg: &Config, args: IndexArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let adapters = default_adapters();

    // Pre-scan so we can report counts and skipped files before processing starts.
    let collected = collect_source_files(&args.path, &adapters)?;
    let total = collected.recognized.len();
    let skipped = collected.skipped;

    if total == 0 {
        eprintln!("asd index: no recognized source files found under {}", args.path.display());
        if args.verbose && !skipped.is_empty() {
            eprintln!("  {} file{} skipped (no adapter):", skipped.len(), if skipped.len() == 1 { "" } else { "s" });
            for f in &skipped {
                eprintln!("  [skip] {}", f.display());
            }
        }
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "files": 0,
            "skipped": skipped.len(),
            "symbols": 0,
            "effects": 0,
            "edges": 0,
            "intra_module_edges": 0,
            "cross_module_edges": 0,
            "transitive_updates": 0,
            "orphaned_tagged": 0,
        }))?);
        return Ok(());
    }

    eprintln!("Indexing {} file{} under {} …",
        total,
        if total == 1 { "" } else { "s" },
        args.path.display()
    );

    let verbose = args.verbose;
    let progress: Option<&dyn Fn(&std::path::Path, usize, usize)> = if verbose {
        Some(&|file: &std::path::Path, idx: usize, total: usize| {
            eprintln!("  [{idx:>width$}/{total}] {}", file.display(), width = total.to_string().len());
        })
    } else {
        None
    };

    let summary = run_index(
        &engine.repo,
        &engine.ref_name,
        &args.path,
        &cfg.agent_id,
        &adapters,
        Some(engine.audit.as_ref()),
        progress,
    )?;

    if args.verbose && !skipped.is_empty() {
        eprintln!("  {} file{} skipped (no adapter):", skipped.len(), if skipped.len() == 1 { "" } else { "s" });
        for f in &skipped {
            eprintln!("  [skip] {}", f.display());
        }
    }

    eprintln!("Done. {} symbol{}, {} effect{}.{}",
        summary.symbols, if summary.symbols == 1 { "" } else { "s" },
        summary.effects, if summary.effects == 1 { "" } else { "s" },
        if summary.skipped > 0 && !args.verbose {
            format!(" ({} file{} skipped — run with -v to list)", summary.skipped, if summary.skipped == 1 { "" } else { "s" })
        } else {
            String::new()
        },
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "files": summary.files,
            "skipped": summary.skipped,
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
