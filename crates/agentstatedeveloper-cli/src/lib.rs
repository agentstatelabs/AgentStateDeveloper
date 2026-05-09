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

    /// Assemble agent query context for one or more symbols.
    /// Returns signature, callers/callees, effects, invariants, hazards, and ledger.
    #[command(name = "context-for")]
    ContextFor(commands::context_for::ContextForArgs),

    /// Scan the ASG for integrity issues (orphaned refs, malformed blobs, stale
    /// call graph edges) and optionally apply safe auto-corrections.
    /// By default runs read-only (dry-run); pass `--fix` to apply corrections.
    Repair(commands::repair::RepairArgs),

    /// Working notes scoped to a symbol or investigation, with a
    /// promote-to-ledger path. Local-only; not synced by `asd sync`.
    #[command(subcommand)]
    Scratch(commands::scratch::ScratchCmd),

    /// Ranked concept search over indexed symbols.
    Search(commands::search::SearchArgs),

    /// Broad feature archaeology: search → expand call chains, invariants,
    /// hazards, and effects for the top matching entry points in one pass.
    Investigate(commands::investigate::InvestigateArgs),

    /// Show index health: age, symbol count, and optionally dirty source files.
    Status(commands::status::StatusArgs),

    /// Blast-radius analysis before editing a symbol: transitive callers,
    /// aggregated effects, invariants/hazards, affected tests, and recent git touches.
    Impact(commands::impact::ImpactArgs),

    /// Structured pre-edit checklist: files to inspect, invariants to preserve,
    /// tests to run, known hazards, and effects to verify. Markdown or JSON output.
    Checklist(commands::checklist::ChecklistArgs),

    /// Record, list, and remove invariants attached to symbols.
    /// Shortcut for `asd ledger {append,list,withdraw} --kind invariant`.
    #[command(subcommand)]
    Invariant(commands::invariant::InvariantCmd),

    /// One-call agent-ready context package for a planned change: design invariants,
    /// layer-grouped entry points, likely edit files, affected tests, effects, and
    /// recent git touches — all composed in a single JSON response.
    #[command(name = "prepare-change")]
    PrepareChange(commands::prepare_change::PrepareChangeArgs),

    /// Symbols in files changed since a commit + combined blast radius.
    /// PR-review workflow: pass the base SHA to get full impact without knowing
    /// any symbol names upfront. Supports --agent, --intent, --depth.
    Since(commands::since::SinceArgs),

    /// Record and list search-quality feedback verdicts for (query, symbol) pairs.
    #[command(subcommand)]
    Feedback(commands::feedback::FeedbackCmd),

    /// Derive ledger annotations from a git commit and optionally write them.
    /// Reads changed files and the commit message, resolves touched symbols,
    /// and suggests (or records) decisions, invariants, proofs, and hazards.
    #[command(name = "annotate-commit")]
    AnnotateCommit(commands::annotate_commit::AnnotateCommitArgs),

    /// Close an active task: write proof and optional validation entries to the
    /// ledger for all symbols affected by HEAD, tagged with CTX plan/task provenance.
    #[command(name = "task-close")]
    TaskClose(commands::task_close::TaskCloseArgs),

    /// Benchmark scorecard across the five ASD dimensions:
    /// truth, feedback, change, uncertainty, and workflow.
    Scorecard(commands::scorecard::ScorecardArgs),

    /// Golden benchmark harness: run structural assertions against ASD
    /// command output to catch ranking/classification regressions.
    Probe(commands::probe::ProbeCmd),
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
        Command::ContextFor(args) => context_for::run(cfg, args),
        Command::Repair(args) => repair::run(cfg, args),
        Command::Scratch(cmd) => scratch::run(cfg, cmd),
        Command::Search(args) => search::run(cfg, args),
        Command::Investigate(args) => investigate::run(cfg, args),
        Command::Status(args) => status::run(cfg, args),
        Command::Impact(args) => impact::run(cfg, args),
        Command::Checklist(args) => checklist::run(cfg, args),
        Command::Invariant(sub) => invariant::run(cfg, sub),
        Command::PrepareChange(args) => prepare_change::run(cfg, args),
        Command::Since(args) => since::run(cfg, args),
        Command::Feedback(sub) => feedback::run(cfg, sub),
        Command::AnnotateCommit(args) => annotate_commit::run(cfg, args),
        Command::TaskClose(args) => task_close::run(cfg, args),
        Command::Scorecard(args) => scorecard::run(cfg, args),
        Command::Probe(cmd) => probe::run(cfg, cmd),
    }
}
