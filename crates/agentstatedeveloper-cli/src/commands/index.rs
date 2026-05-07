//! `asd index <path>` — walk a directory for source files we have
//! adapters for, parse them, and write Symbol + EffectDecl records into
//! the ASG.
//!
//! A full debug log is always written to `.asd/index.log` in the current
//! directory. `--verbose` tees that same output to stderr in real time.
//! Skipped files are capped at 100 lines on stderr but fully recorded in
//! the log file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_adapters::default_adapters;
use agentstatedeveloper_core::{collect_source_files, run_index, Engine};

use crate::config::Config;

const SKIPPED_DISPLAY_LIMIT: usize = 100;

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Directory (or file) to index. Recursively walks for known source
    /// extensions (`.py`, `.ts`, `.tsx`, `.rs`, `.go`, `.java`, `.cs`, `.rb`, `.kt`, `.swift`).
    pub path: PathBuf,

    /// Tee the full index log to stderr in real time.
    #[arg(short, long)]
    pub verbose: bool,
}

/// Always-on log that writes every line to `.asd/index.log` and
/// optionally mirrors it to stderr when verbose is set.
struct IndexLog {
    file: std::fs::File,
    verbose: bool,
}

impl IndexLog {
    fn open(verbose: bool) -> Option<Self> {
        let dir = PathBuf::from(".asd");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::File::create(dir.join("index.log")).ok().map(|f| Self { file: f, verbose })
    }

    fn line(&mut self, msg: &str) {
        let _ = writeln!(self.file, "{}", msg);
        if self.verbose {
            eprintln!("{}", msg);
        }
    }
}

pub fn run(cfg: &Config, args: IndexArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let adapters = default_adapters();

    let mut log = IndexLog::open(args.verbose);
    let log_path = PathBuf::from(".asd/index.log");

    // Pre-scan so we know total counts before processing starts.
    let collected = collect_source_files(&args.path, &adapters)?;
    let total = collected.recognized.len();
    let skipped = collected.skipped;

    let header = format!(
        "Indexing {} file{} under {} …",
        total,
        if total == 1 { "" } else { "s" },
        args.path.display()
    );

    // Always print the header to stderr regardless of verbose.
    eprintln!("{}", header);
    if let Some(l) = &mut log { l.line(&header); }

    if total == 0 {
        let msg = format!(
            "asd index: no recognized source files found under {}",
            args.path.display()
        );
        eprintln!("{}", msg);
        if let Some(l) = &mut log { l.line(&msg); }
        log_skipped(&skipped, &mut log, true);
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

    // Wrap log in Arc<Mutex> so the progress closure can borrow it.
    let log = Arc::new(Mutex::new(log));
    let log_clone = Arc::clone(&log);
    let width = total.to_string().len();

    let log_clone2 = Arc::clone(&log);

    let progress: &dyn Fn(&Path, usize, usize) = &|file: &Path, idx: usize, total: usize| {
        let msg = format!("  [{idx:>width$}/{total}] {}", file.display());
        if let Ok(mut guard) = log_clone.lock() {
            if let Some(l) = guard.as_mut() {
                l.line(&msg);
            }
        }
    };

    // Phase messages always go to stderr (not just verbose) — they mark the
    // boundary between file parsing and post-processing so the user knows
    // the tool is still working on a large repo.
    let on_phase: &dyn Fn(&str) = &|msg: &str| {
        eprintln!("{}", msg);
        if let Ok(mut guard) = log_clone2.lock() {
            if let Some(l) = guard.as_mut() {
                l.line(msg);
            }
        }
    };

    let summary = run_index(
        &engine.repo,
        &engine.ref_name,
        &args.path,
        &cfg.agent_id,
        &adapters,
        Some(engine.audit.as_ref()),
        Some(progress),
        Some(on_phase),
        Some(&cfg.db_path),
    )?;

    // Unwrap Arc — run_index is done, no other holders.
    let mut log = Arc::try_unwrap(log).ok().and_then(|m| m.into_inner().ok()).flatten();

    // Log all skipped files (no cap in log; capped on stderr).
    log_skipped(&skipped, &mut log, args.verbose);

    // Done summary — always to stderr.
    let skipped_hint = if !skipped.is_empty() && !args.verbose {
        format!(
            " ({} file{} skipped — see {})",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            log_path.display()
        )
    } else {
        String::new()
    };
    let doc_hint = if summary.doc_files > 0 {
        format!(", {} doc chunk{} from {} file{}", summary.docs_indexed, if summary.docs_indexed == 1 { "" } else { "s" }, summary.doc_files, if summary.doc_files == 1 { "" } else { "s" })
    } else { String::new() };
    let done_msg = format!(
        "Done. {} symbol{}, {} effect{}{}.{}",
        summary.symbols, if summary.symbols == 1 { "" } else { "s" },
        summary.effects, if summary.effects == 1 { "" } else { "s" },
        doc_hint,
        skipped_hint,
    );
    eprintln!("{}", done_msg);
    if let Some(l) = &mut log { l.line(&done_msg); }

    let log_note = format!("Full log: {}", log_path.display());
    if let Some(l) = &mut log { l.line(&log_note); }
    // Only print log path hint to stderr when not verbose (verbose already showed everything).
    if !args.verbose {
        eprintln!("{}", log_note);
    }

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
            "disambiguated": summary.disambiguated,
            "doc_files": summary.doc_files,
            "docs_indexed": summary.docs_indexed,
            "cross_file_collisions": summary.top_collisions.iter().map(|(q, f1, f2)| {
                serde_json::json!({ "qname": q, "first": f1, "second": f2 })
            }).collect::<Vec<_>>(),
        }))?
    );
    Ok(())
}

/// Write skipped files to the log and conditionally to stderr.
/// Log receives all files; stderr is capped at SKIPPED_DISPLAY_LIMIT.
fn log_skipped(skipped: &[PathBuf], log: &mut Option<IndexLog>, show_on_stderr: bool) {
    if skipped.is_empty() {
        return;
    }

    let header = format!(
        "  {} file{} skipped (no adapter):",
        skipped.len(),
        if skipped.len() == 1 { "" } else { "s" }
    );

    // Log file always gets the header and all entries.
    if let Some(l) = log.as_mut() {
        l.line(&header);
        for f in skipped {
            l.line(&format!("  [skip] {}", f.display()));
        }
    }

    if !show_on_stderr {
        return;
    }

    // stderr: header + up to SKIPPED_DISPLAY_LIMIT entries.
    eprintln!("{}", header);
    let display = skipped.len().min(SKIPPED_DISPLAY_LIMIT);
    for f in &skipped[..display] {
        eprintln!("  [skip] {}", f.display());
    }
    if skipped.len() > SKIPPED_DISPLAY_LIMIT {
        eprintln!(
            "  … and {} more — see .asd/index.log for the full list",
            skipped.len() - SKIPPED_DISPLAY_LIMIT
        );
    }
}
