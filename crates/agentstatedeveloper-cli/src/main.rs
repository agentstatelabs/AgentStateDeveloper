//! `asd` — AgentStateDeveloper CLI.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod config;
mod commands;

use commands::{init, index, ledger, read, verify_effects};

/// AgentStateDeveloper — code-level context and audit overlay.
#[derive(Debug, Parser)]
#[command(name = "asd", version, about = "AgentStateDeveloper CLI")]
struct Cli {
    /// Path to the ASD SQLite database. Overrides `ASD_DB` env var.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize an ASD repository.
    Init(init::InitArgs),

    /// Index Python source files under a directory.
    Index(index::IndexArgs),

    /// Read a symbol, its effects, and recent ledger entries.
    Read(read::ReadArgs),

    /// Ledger operations.
    #[command(subcommand)]
    Ledger(ledger::LedgerCmd),

    /// Verify declared effects for a symbol (M1: prints declared as unverified).
    VerifyEffects(verify_effects::VerifyEffectsArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::resolve(cli.db.clone());

    match cli.cmd {
        Command::Init(args) => init::run(&cfg, args),
        Command::Index(args) => index::run(&cfg, args),
        Command::Read(args) => read::run(&cfg, args),
        Command::Ledger(sub) => ledger::run(&cfg, sub),
        Command::VerifyEffects(args) => verify_effects::run(&cfg, args),
    }
}
