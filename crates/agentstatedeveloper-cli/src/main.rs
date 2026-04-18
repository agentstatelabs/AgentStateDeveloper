//! `asd` — AgentStateDeveloper CLI.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod config;
mod commands;

use commands::{hydrate, index, init, ledger, policy, read, sync, trace, verify_effects};

/// AgentStateDeveloper — code-level context and audit overlay.
#[derive(Debug, Parser)]
#[command(name = "asd", version, about = "AgentStateDeveloper CLI")]
struct Cli {
    /// Path to the ASD SQLite database. Overrides `ASD_DB` env var.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Path to a JSON policy file evaluated against ledger/effect writes.
    /// Overrides `ASD_POLICY` env var. When absent, solo-dev default is
    /// permissive (everything Allow).
    #[arg(long, global = true)]
    policy: Option<PathBuf>,

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

    /// Policy introspection (requires --policy / ASD_POLICY).
    #[command(subcommand)]
    Policy(policy::PolicyCmd),

    /// Verify declared effects for a symbol (M1: prints declared as unverified).
    VerifyEffects(verify_effects::VerifyEffectsArgs),

    /// Run a Python program under the ASD runtime tracer and ingest the
    /// observed effects into ASG.
    Trace(trace::TraceArgs),

    /// Mirror ASG state into the `.asd/v1/` on-disk sidecar so it can
    /// travel with `git commit`.
    Sync(sync::SyncArgs),

    /// Read the `.asd/v1/` sidecar back into ASG. Inverse of `sync`.
    /// Used to restore state after a fresh `git clone`.
    Hydrate(hydrate::HydrateArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::Config::resolve(cli.db.clone(), cli.policy.clone());

    match cli.cmd {
        Command::Init(args) => init::run(&cfg, args),
        Command::Index(args) => index::run(&cfg, args),
        Command::Read(args) => read::run(&cfg, args),
        Command::Ledger(sub) => ledger::run(&cfg, sub),
        Command::Policy(sub) => policy::run(&cfg, sub),
        Command::VerifyEffects(args) => verify_effects::run(&cfg, args),
        Command::Trace(args) => trace::run(&cfg, args),
        Command::Sync(args) => sync::run(&cfg, args),
        Command::Hydrate(args) => hydrate::run(&cfg, args),
    }
}
