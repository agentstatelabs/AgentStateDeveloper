//! `asd onboard` — one-shot setup for a freshly-cloned project (Plan K t-005).
//!
//! Runs `init → index → conclusions import → mcp install` in the right order,
//! idempotent. A new developer (or their agent) doesn't need to know the
//! sequence — they run `asd onboard` and get:
//!
//!   1. A live SQLite ASG (`asd init` — installs hooks, updates .gitignore)
//!   2. A fresh semantic index from source (`asd index .`)
//!   3. Inherited ledger entries from the committed sidecar
//!      (`asd conclusions import` — pulls `.asd/conclusions/*.jsonl` in)
//!   4. A project-scoped MCP registration so the user's agent can reach asd
//!      (`asd mcp install --project` — writes `.mcp.json` in the repo). This
//!      mirrors `ctx init`'s one-command ergonomics; pass `--no-mcp` to skip.
//!
//! Steps that have already completed are no-ops. Steps that have nothing
//! to do (e.g. no `.asd/conclusions/` directory yet, fresh-from-scratch
//! project) skip cleanly with a one-line note instead of failing. The MCP
//! step is best-effort: if the `asd-mcp` binary isn't installed it warns and
//! continues, since the repo is already usable without it.
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
    mcp::{self, InstallArgs, McpCmd},
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

    /// Skip the MCP-registration step. By default `onboard` finishes by
    /// registering a project-scoped asd-mcp server (`.mcp.json` in the repo)
    /// so the user's agent can reach asd in one command.
    #[arg(long, default_value_t = false)]
    pub no_mcp: bool,
}

pub fn run(cfg: &Config, args: OnboardArgs) -> Result<()> {
    eprintln!("== asd onboard ==\n");

    // -- Step 1: init -----------------------------------------------------
    eprintln!("[1/4] asd init …");
    let init_args = InitArgs {
        no_hooks: args.no_hooks,
    };
    init::run(cfg, init_args)?;

    // -- Step 2: index ---------------------------------------------------
    eprintln!("\n[2/4] asd index {} …", args.path.display());
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
        eprintln!("\n[3/4] asd conclusions import …");
        let import_args = ImportArgs {
            in_dir: Some(conclusions_dir),
        };
        conclusions::run(cfg, ConclusionsCmd::Import(import_args))?;
    } else {
        eprintln!(
            "\n[3/4] asd conclusions import — skipped (.asd/conclusions/ doesn't exist; fresh project)"
        );
    }

    // -- Step 4: mcp install (project-scoped) ----------------------------
    // Best-effort: registration is what makes asd reachable from the user's
    // agent in one command (matching `ctx init`). Project scope writes a
    // `.mcp.json` in the repo rather than editing global tool configs, so it's
    // safe to run repeatedly and never clobbers a cooperating tool's global
    // registration. If the `asd-mcp` binary isn't installed we warn and keep
    // going — the repo is already usable; only the agent wiring is missing.
    if args.no_mcp {
        eprintln!("\n[4/4] asd mcp install — skipped (--no-mcp)");
    } else {
        eprintln!("\n[4/4] asd mcp install --project …");
        let install_args = InstallArgs {
            db: None,
            tool: None,
            follow_active: false,
            project: true,
        };
        if let Err(e) = mcp::run(cfg, McpCmd::Install(install_args)) {
            eprintln!(
                "  warning: MCP registration skipped — {e}\n  \
                 (repo is set up; run `asd mcp install` once asd-mcp is available)"
            );
        }
    }

    eprintln!("\n== onboard complete ==");
    eprintln!(
        "Your ASD project is ready. Try:\n  asd status\n  asd search <query>\n  asd think bootstrap"
    );
    Ok(())
}
