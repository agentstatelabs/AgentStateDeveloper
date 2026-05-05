//! agentstatedeveloper-cli — library surface.
//!
//! The `asd` binary is a thin wrapper over this crate. `asd-pro`
//! (commercial) imports this as a library and extends it: it reuses
//! [`run_oss_command`] for OSS subcommands and provides its own
//! handlers for the ratify / audit-verify commands that OSS stubs out.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use clap::{Parser, Subcommand};

use agentstatedeveloper_core::{AuditSink, RatifyOps};

pub mod config;
pub mod commands;

pub use config::Config;

/// Process-wide audit sink override. `asd-pro` sets this at startup
/// to swap in `JsonlFileSink` when `--audit-log` / `ASD_AUDIT_LOG` is
/// configured. OSS `asd` leaves it unset, so subcommands fall back to
/// the default `NullSink` (with a warning if a log path was configured).
static AUDIT_SINK_OVERRIDE: OnceLock<Arc<dyn AuditSink>> = OnceLock::new();

pub fn set_audit_sink_override(sink: Arc<dyn AuditSink>) {
    let _ = AUDIT_SINK_OVERRIDE.set(sink);
}

pub(crate) fn audit_sink_override() -> Option<Arc<dyn AuditSink>> {
    AUDIT_SINK_OVERRIDE.get().cloned()
}

/// Process-wide ratify ops override. `asd-pro` installs `RatifyOpsImpl`
/// here before dispatch. `open_engine` wires it into every Engine instance.
static RATIFY_OVERRIDE: OnceLock<Arc<dyn RatifyOps>> = OnceLock::new();

pub fn set_ratify_ops_override(ratify: Arc<dyn RatifyOps>) {
    let _ = RATIFY_OVERRIDE.set(ratify);
}

pub(crate) fn ratify_ops_override() -> Option<Arc<dyn RatifyOps>> {
    RATIFY_OVERRIDE.get().cloned()
}

/// AgentStateDeveloper — code-level context and audit overlay.
#[derive(Debug, Parser)]
#[command(name = "asd", version, about = "AgentStateDeveloper CLI")]
pub struct Cli {
    /// Path to the ASD SQLite database. Overrides `ASD_DB` env var.
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Path to a JSON policy file evaluated against ledger/effect writes.
    /// Overrides `ASD_POLICY` env var. When absent, solo-dev default is
    /// permissive (everything Allow).
    #[arg(long, global = true)]
    pub policy: Option<PathBuf>,

    /// Path to an append-only JSONL audit log. Every ledger/effect
    /// mutation emits one event. Overrides `ASD_AUDIT_LOG` env var.
    /// When absent, the audit sink discards events.
    #[arg(long, global = true)]
    pub audit_log: Option<PathBuf>,

    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize an ASD repository and install git hooks.
    Init(commands::init::InitArgs),

    /// Show installed ASD git hooks and their current status.
    Hooks(commands::hooks::HooksArgs),

    /// Index Python source files under a directory.
    Index(commands::index::IndexArgs),

    /// Read a symbol, its effects, and recent ledger entries.
    Read(commands::read::ReadArgs),

    /// Ledger operations.
    #[command(subcommand)]
    Ledger(commands::ledger::LedgerCmd),

    /// Policy introspection (requires --policy / ASD_POLICY).
    #[command(subcommand)]
    Policy(commands::policy::PolicyCmd),

    /// Verify declared effects for a symbol (M1: prints declared as unverified).
    VerifyEffects(commands::verify_effects::VerifyEffectsArgs),

    /// Run a program under the ASD runtime tracer and ingest observed effects
    /// (Python only — uses sys.settrace via tools/asd_tracer.py).
    Trace(commands::trace::TraceArgs),

    /// Mirror ASG state into the `.asd/v1/` on-disk sidecar so it can
    /// travel with `git commit`.
    Sync(commands::sync::SyncArgs),

    /// Read the `.asd/v1/` sidecar back into ASG. Inverse of `sync`.
    /// Used to restore state after a fresh `git clone`.
    Hydrate(commands::hydrate::HydrateArgs),

    /// Read back audit events (ledger mutations, policy evaluations,
    /// effect declarations) emitted to the JSONL audit log.
    #[command(subcommand)]
    Audit(commands::audit::AuditCmd),

    /// Install, uninstall, or check asd-mcp registration in agent tools.
    #[command(subcommand)]
    Mcp(commands::mcp::McpCmd),

    /// List indexed symbols, effects, or ledger entries.
    List(commands::list::ListArgs),

    /// Show symbols that call the given symbol (direct or transitive).
    Callers(commands::graph::CallersArgs),

    /// Show symbols called by the given symbol (direct or transitive).
    Callees(commands::graph::CalleesArgs),
}

/// Resolve [`Config`] from the parsed CLI flags.
pub fn config_from_cli(cli: &Cli) -> Config {
    Config::resolve(cli.db.clone(), cli.policy.clone(), cli.audit_log.clone())
}

/// Dispatch the OSS command set. `asd-pro` can call this for any
/// subcommand that does not require a commercial override.
pub fn run(cli: Cli) -> Result<()> {
    let cfg = config_from_cli(&cli);
    run_with_config(&cfg, cli.cmd)
}

/// Same as [`run`] but uses an already-resolved [`Config`]. `asd-pro`
/// uses this after customizing the audit sink / ledger store.
pub fn run_with_config(cfg: &Config, cmd: Command) -> Result<()> {
    use commands::*;
    match cmd {
        Command::Init(args) => init::run(cfg, args),
        Command::Hooks(args) => hooks::run(cfg, args),
        Command::Index(args) => index::run(cfg, args),
        Command::Read(args) => read::run(cfg, args),
        Command::Ledger(sub) => ledger::run(cfg, sub),
        Command::Policy(sub) => policy::run(cfg, sub),
        Command::VerifyEffects(args) => verify_effects::run(cfg, args),
        Command::Trace(args) => trace::run(cfg, args),
        Command::Sync(args) => sync::run(cfg, args),
        Command::Hydrate(args) => hydrate::run(cfg, args),
        Command::Audit(sub) => audit::run(cfg, sub),
        Command::Mcp(sub) => mcp::run(cfg, sub),
        Command::List(args) => list::run(cfg, args),
        Command::Callers(args) => graph::run_callers(cfg, args),
        Command::Callees(args) => graph::run_callees(cfg, args),
    }
}
