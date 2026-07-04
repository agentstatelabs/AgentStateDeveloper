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

pub mod commands;
pub mod config;

pub use config::Config;

/// Process-wide audit sink override. `asd-pro` sets this at startup
/// to swap in `JsonlFileSink` when `--audit-log` / `ASD_AUDIT_LOG` is
/// configured. OSS `asd` leaves it unset, so subcommands fall back to
/// the default `NullSink` (with a warning if a log path was configured).
static AUDIT_SINK_OVERRIDE: OnceLock<Arc<dyn AuditSink>> = OnceLock::new();

pub fn set_audit_sink_override(sink: Arc<dyn AuditSink>) {
    // First-write-wins: OnceLock::set returns Err if already initialized.
    // `asd-pro` installs once at startup; later calls (test re-init, etc.)
    // are intentional no-ops — the first sink remains authoritative.
    let _ = AUDIT_SINK_OVERRIDE.set(sink);
}

pub(crate) fn audit_sink_override() -> Option<Arc<dyn AuditSink>> {
    AUDIT_SINK_OVERRIDE.get().cloned()
}

/// Process-wide ratify ops override. `asd-pro` installs `RatifyOpsImpl`
/// here before dispatch. `open_engine` wires it into every Engine instance.
static RATIFY_OVERRIDE: OnceLock<Arc<dyn RatifyOps>> = OnceLock::new();

pub fn set_ratify_ops_override(ratify: Arc<dyn RatifyOps>) {
    // First-write-wins (see set_audit_sink_override): the OSS->pro
    // installer runs once; double-set is a no-op by design.
    let _ = RATIFY_OVERRIDE.set(ratify);
}

pub(crate) fn ratify_ops_override() -> Option<Arc<dyn RatifyOps>> {
    RATIFY_OVERRIDE.get().cloned()
}

/// AgentStateDeveloper — code-level context and audit overlay.
#[derive(Debug, Parser)]
#[command(
    name = "asd",
    version,
    about = "AgentStateDeveloper CLI",
    long_about = "AgentStateDeveloper CLI — code-level context and audit overlay.

Bootstrap a NEW repo (no sidecar yet) — manual sequence:
  asd init                  # install git hooks, create .asd/ directory
  asd index <path>          # walk source files; build FTS + ASG state
  asd sync                  # write .asd/v1/ sidecar (commit this)
  asd status                # verify (expect state: fresh, sidecar: hydrated)

Bootstrap a FRESH CLONE (sidecar already in repo) — manual sequence:
  asd init                  # install git hooks
  asd hydrate --verify      # restore ASG from sidecar; --verify catches drift
  asd status                # verify (index_consistency.consistent == true)

One-shot alternative (recommended): `asd onboard` runs the right sequence
automatically for either case above. Idempotent — safe to re-run.

Daily loop:
  asd prepare-change \"<task>\"   # one-call context package (use BEFORE editing)
  asd search \"<query>\" --agent   # ranked semantic search
  asd impact <symbol>             # blast radius for a planned change
  asd ledger append ...           # record a decision / invariant / hazard

When source changes outside ASD's commit hooks, re-run `asd index <path>`
to refresh the FTS cache. `asd status` will flag staleness if the index
is more than an hour behind."
)]
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

    /// Plan D t-001: emit compact output projecting each command down to
    /// load-bearing fields (qname, file:line, signature, first doc line).
    /// 60-80% token reduction vs default. Also honors `ASD_FORMAT=brief`.
    /// `--json` (default) keeps the structured payload.
    #[arg(long, global = true)]
    pub brief: bool,

    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize an ASD repository and install git hooks.
    Init(commands::init::InitArgs),

    /// Plan K t-005: one-shot post-clone setup. Runs `init → index →
    /// conclusions import` in the right order so a new developer (or
    /// their agent) gets a fully usable ASD project in one command.
    /// Idempotent — re-runs are safe.
    Onboard(commands::onboard::OnboardArgs),

    /// Show installed ASD git hooks and their current status.
    Hooks(commands::hooks::HooksArgs),

    /// Walk source files under a directory and build the FTS + ASG
    /// index. Re-runnable; idempotent. (Aliased as `reindex` to
    /// match the MCP `mcp__asd__reindex` tool name.)
    #[command(alias = "reindex")]
    Index(commands::index::IndexArgs),

    /// Read a symbol, its effects, and recent ledger entries.
    /// Plan D t-003: also accepts `code_read` (MCP-era alias).
    #[command(alias = "code_read")]
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

    /// Install ASD's agent Skill (SKILL.md) into detected agent skill dirs.
    Skill(commands::skill::SkillArgs),

    /// Print a paste-into-your-agent block that installs + connects ASD (+ CTX).
    Bootstrap(commands::bootstrap::BootstrapArgs),

    /// List indexed symbols, effects, or ledger entries.
    List(commands::list::ListArgs),

    /// Show symbols that call the given symbol (direct or transitive).
    /// Plan D t-003: also accepts `callers_of` (MCP-era alias).
    #[command(alias = "callers_of")]
    Callers(commands::graph::CallersArgs),

    /// Show symbols called by the given symbol (direct or transitive).
    /// Plan D t-003: also accepts `callees_of` (MCP-era alias).
    #[command(alias = "callees_of")]
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
    /// Plan D t-003: also accepts `code_search` and `code_query` (MCP-era aliases).
    #[command(aliases = ["code_search", "code_query"])]
    Search(commands::search::SearchArgs),

    /// Exact-symbol references via literal text scan + index definition lookup.
    /// Use this when you want rg-style completeness on a concrete identifier
    /// (no tokenization, no BM25). Requires `rg` on PATH.
    References(commands::references::ReferencesArgs),

    /// List named scope aliases defined in `.asd/scopes.toml`. Discoverability
    /// for the `--scope` and `--paths` flags supported by search and friends.
    #[command(subcommand)]
    Scopes(commands::scopes::ScopesCmd),

    /// View ledger entries bucketed by the six Plan B conclusion classes
    /// (decisions, classifications, mappings, hazards, recipes, followups).
    #[command(subcommand)]
    Conclusions(commands::conclusions::ConclusionsCmd),

    /// Plan C t-004: structured change-intent recipes. Returns per-file
    /// action plans (Delete / Gate / Run / KeepAsCovered / Review) for
    /// known task families.
    #[command(subcommand)]
    Recipe(commands::recipe::RecipeCmd),

    /// Plan C t-007: initial-read project summary. Walks the indexed
    /// project, identifies package boundaries, and tags test files
    /// (fast-test / diagnostic-test). Writes Ownership ledger entries
    /// with role tags so the next session inherits the mental model.
    Map(commands::map::MapArgs),

    /// Plan G t-003: capture agent thinking. Subcommands: speculate
    /// (Hypothesis), model (MentalModel), failed (FailedAttempt),
    /// question (OpenQuestion), list. See docs/initial-read-prompt.md.
    #[command(subcommand)]
    Think(commands::think::ThinkCmd),

    /// Sidecar utilities. `migrate` flips a repo from the legacy `.asd/v1/`
    /// layout (Plan A) to the compact `.asd/conclusions/` layout (Plan B).
    #[command(subcommand)]
    Sidecar(commands::sidecar::SidecarCmd),

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

    /// List cross-service endpoints (HTTP routes/clients, pub-sub) detected in
    /// this repo, show in-repo matched edges, and `--export` a service manifest.
    Endpoints(commands::endpoints::EndpointsArgs),

    /// One-call "orient me" overview: languages, packages, layers, routes, and
    /// call-graph hotspots for a cold agent. Supports --agent.
    Architecture(commands::architecture::ArchitectureArgs),

    /// Functions/methods with no inbound call edges (candidate dead code).
    /// Excludes route handlers, tests, and main/dunder methods. Supports --agent.
    DeadCode(commands::dead_code::DeadCodeArgs),

    /// Read test-runner output on stdin; emit a compact failures-only summary
    /// (cargo/pytest parsed precisely, others via generic scan). Supports --agent.
    TestSummary(commands::test_summary::TestSummaryArgs),

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

    /// State Trust Score: machine-readable rollup of index freshness, sidecar
    /// status, ledger density, dirty files, and concept gaps. Answers: "can I
    /// rely on ASD for the current task?" in a single call.
    Trust(commands::trust::TrustArgs),

    /// Task workflow session history: evidence quality, workflow type, and
    /// missing steps across recent `asd task-close` invocations.
    Workflow(commands::workflow::WorkflowArgs),

    /// Manage the shared ASD repo registry at ~/.config/asd/repos.toml.
    /// Subcommands: add, list, use, rm, show.
    #[command(subcommand)]
    Repo(commands::repo::RepoCmd),
}

/// Resolve [`Config`] from the parsed CLI flags.
pub fn config_from_cli(cli: &Cli) -> Config {
    Config::resolve_with_brief(
        cli.db.clone(),
        cli.policy.clone(),
        cli.audit_log.clone(),
        cli.brief,
    )
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
        Command::Onboard(args) => onboard::run(cfg, args),
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
        Command::Skill(args) => skill::run(args),
        Command::Bootstrap(args) => bootstrap::run(args),
        Command::List(args) => list::run(cfg, args),
        Command::Callers(args) => graph::run_callers(cfg, args),
        Command::Callees(args) => graph::run_callees(cfg, args),
        Command::ContextFor(args) => context_for::run(cfg, args),
        Command::Repair(args) => repair::run(cfg, args),
        Command::Scratch(cmd) => scratch::run(cfg, cmd),
        Command::Search(args) => search::run(cfg, args),
        Command::References(args) => references::run(cfg, args),
        Command::Scopes(cmd) => scopes::run(cfg, cmd),
        Command::Conclusions(cmd) => conclusions::run(cfg, cmd),
        Command::Recipe(cmd) => recipe::run(cfg, cmd),
        Command::Map(args) => map::run(cfg, args),
        Command::Think(cmd) => think::run(cfg, cmd),
        Command::Sidecar(cmd) => sidecar::run(cfg, cmd),
        Command::Investigate(args) => investigate::run(cfg, args),
        Command::Status(args) => status::run(cfg, args),
        Command::Impact(args) => impact::run(cfg, args),
        Command::Checklist(args) => checklist::run(cfg, args),
        Command::Invariant(sub) => invariant::run(cfg, sub),
        Command::PrepareChange(args) => prepare_change::run(cfg, args),
        Command::Since(args) => since::run(cfg, args),
        Command::Endpoints(args) => endpoints::run(cfg, args),
        Command::Architecture(args) => architecture::run(cfg, args),
        Command::DeadCode(args) => dead_code::run(cfg, args),
        Command::TestSummary(args) => test_summary::run(cfg, args),
        Command::Feedback(sub) => feedback::run(cfg, sub),
        Command::AnnotateCommit(args) => annotate_commit::run(cfg, args),
        Command::TaskClose(args) => task_close::run(cfg, args),
        Command::Scorecard(args) => scorecard::run(cfg, args),
        Command::Probe(cmd) => probe::run(cfg, cmd),
        Command::Trust(args) => trust::run(cfg, args),
        Command::Workflow(args) => workflow::run(cfg, args),
        Command::Repo(cmd) => repo::run(cfg, cmd),
    }
}
