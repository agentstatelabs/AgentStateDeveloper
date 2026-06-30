//! `asd onboard` — one-shot setup for a freshly-cloned project (Plan K t-005).
//!
//! Runs `init → index → conclusions import` in the right order, idempotent.
//! A new developer (or their agent) doesn't need to know the sequence —
//! they run `asd onboard` and get:
//!
//!   1. A live SQLite ASG (`asd init` — installs hooks, updates .gitignore)
//!   2. A fresh semantic index from source (`asd index .`)
//!   3. Inherited ledger entries from the committed sidecar
//!      (`asd conclusions import` — pulls `.asd/conclusions/*.jsonl` in)
//!
//! Steps that have already completed are no-ops. Steps that have nothing
//! to do (e.g. no `.asd/conclusions/` directory yet, fresh-from-scratch
//! project) skip cleanly with a one-line note instead of failing.
//!
//! Composing existing commands rather than re-implementing keeps the
//! onboard surface a thin orchestrator — it inherits future
//! init/index/conclusions improvements for free.

use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

use crate::commands::{
    conclusions::{self, ConclusionsCmd, ImportArgs},
    index::{self, IndexArgs},
    init::{self, InitArgs},
};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct OnboardArgs {
    /// Directory to index. Defaults to the current working directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Skip git hook installation in the init step. Mirrors the
    /// `asd init --no-hooks` flag.
    #[arg(long, default_value_t = false)]
    pub no_hooks: bool,

    /// Tee the full index log to stderr in real time. Mirrors the
    /// `asd index --verbose` flag.
    #[arg(short, long)]
    pub verbose: bool,
}

pub fn run(cfg: &Config, args: OnboardArgs) -> Result<()> {
    eprintln!("== asd onboard ==\n");

    // -- Step 1: init -----------------------------------------------------
    eprintln!("[1/3] asd init …");
    let init_args = InitArgs {
        no_hooks: args.no_hooks,
    };
    init::run(cfg, init_args)?;

    // -- Step 2: index ---------------------------------------------------
    eprintln!("\n[2/3] asd index {} …", args.path.display());
    let index_args = IndexArgs {
        path: args.path.clone(),
        verbose: args.verbose,
    };
    index::run(cfg, index_args)?;

    // -- Step 3: conclusions import -------------------------------------
    // Skip cleanly when there's nothing to import (fresh project, no
    // committed sidecar yet). conclusions::import resolves its default
    // input dir relative to the database parent; we mirror that here
    // for the existence check so the skip message references the same
    // directory the import would have read from.
    let conclusions_dir = cfg
        .db_path
        .parent()
        .map(|p| p.join(".asd").join("conclusions"))
        .unwrap_or_else(|| PathBuf::from(".asd/conclusions"));
    if conclusions_dir.is_dir() {
        eprintln!("\n[3/3] asd conclusions import …");
        let import_args = ImportArgs {
            in_dir: Some(conclusions_dir),
        };
        conclusions::run(cfg, ConclusionsCmd::Import(import_args))?;
    } else {
        eprintln!(
            "\n[3/3] asd conclusions import — skipped (.asd/conclusions/ doesn't exist; fresh project)"
        );
    }

    eprintln!("\n== onboard complete ==");
    eprintln!(
        "Your ASD project is ready. Try:\n  asd status\n  asd search <query>\n  asd think bootstrap"
    );
    Ok(())
}
