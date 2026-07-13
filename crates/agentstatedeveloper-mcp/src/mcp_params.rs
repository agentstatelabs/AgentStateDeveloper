//! MCP tool parameter types (Plan M t-002, 1.0.92).
//!
//! Extracted from `mcp_server.rs` to make the handler file scannable.
//! Each struct is a typed shape for one MCP tool's arguments —
//! `Deserialize` so rmcp can parse the incoming JSON, `JsonSchema` so
//! rmcp can serve the tool's parameter schema to the agent.
//!
//! Naming-convention notes preserved at the bottom of this file
//! (was at the same position in mcp_server.rs).
//!
//! No behavior change in this extraction; pure relocation.

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
pub struct CodeQueryParams {
    /// Substring match on qualified name.
    pub name_contains: Option<String>,
    /// Filter by symbol kind: module, function, method, class, variable.
    pub kind: Option<String>,
    /// Filter by language (e.g. "python").
    pub language: Option<String>,
    /// Max results to return (default: 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct CodeSearchParams {
    /// Concept or keyword(s) to search for. BM25-ranked via FTS5.
    /// Supports inline exclusion syntax: "drift playhead -sample -waveform".
    pub query: String,
    /// Filter by symbol kind: module, function, method, class, variable.
    pub kind: Option<String>,
    /// Filter by language (e.g. "swift", "python", "typescript", "rust").
    pub language: Option<String>,
    /// Max results to return (default: 20).
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    /// Include test-file symbols in results (default: false — tests excluded so
    /// production entry points rank first).
    #[serde(default)]
    pub include_tests: bool,
    /// Restrict to test symbols only. Overrides include_tests when true.
    /// Use when classifying test coverage or auditing test layout.
    #[serde(default)]
    pub tests_only: bool,
    /// Comma-separated terms to exclude (e.g. "sample editor,waveform").
    pub exclude: Option<String>,
    /// Comma-separated glob patterns to restrict to specific paths (e.g. "App/**/DriftPad*").
    pub paths: Option<String>,
    /// Named scope alias from .asd/scopes.toml (e.g. "drift-pad").
    pub scope: Option<String>,
}

fn default_search_limit() -> u32 {
    20
}

#[derive(Deserialize, JsonSchema)]
pub struct ReferencesParams {
    /// Symbol name to find references for. Pass a qname (e.g. `pkg.mod.Type`)
    /// for exact definition lookup, or a bare identifier (e.g. `MasterBusParams`)
    /// to match any symbol whose qname ends with `.<name>` plus all literal
    /// text occurrences in the worktree.
    pub name: String,
    /// Project root for the rg scan. Defaults to the current directory.
    pub path: Option<String>,
    /// Cap the number of occurrences returned (default: 500, 0 = unlimited).
    #[serde(default = "default_references_limit")]
    pub limit: u32,
    /// Optional rg `--glob` filter, e.g. "**/*.swift". Comma-separated for multiple.
    pub globs: Option<String>,
}

fn default_references_limit() -> u32 {
    500
}

#[derive(Deserialize, JsonSchema)]
pub struct ConclusionsListParams {
    /// Restrict to one conclusion class: decisions | classifications |
    /// mappings | hazards | recipes | followups. Omit to list all six.
    pub class: Option<String>,
    /// Restrict to one symbol qname. Omit to list across all symbols.
    pub symbol: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ConclusionsExportParams {
    /// Output directory for the JSONL files. Defaults to `.asd/conclusions/`
    /// relative to the database parent directory.
    pub out: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ConclusionsImportParams {
    /// Input directory containing `*.jsonl` files. Defaults to
    /// `.asd/conclusions/` relative to the database parent directory.
    #[serde(rename = "in")]
    pub in_dir: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RecipeClassifyTestMigrationParams {
    /// Search query — finds candidate test symbols.
    pub query: String,
    /// Max candidates to classify (default: 50).
    #[serde(default = "default_recipe_limit")]
    pub limit: u32,
}

fn default_recipe_limit() -> u32 {
    50
}

// -- Plan G t-003: agent-thinking write surface ------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ThinkSpeculateParams {
    pub qname: String,
    /// Confidence in [0.0, 1.0]. Below 0.3 is excluded from prior_thinking
    /// auto-surface by default.
    pub confidence: f64,
    pub summary: String,
    pub body: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ThinkModelParams {
    pub name: String,
    /// Comma-separated qnames the model spans (first is anchor).
    pub symbols: String,
    pub summary: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ThinkFailedParams {
    pub qname: String,
    pub tried: String,
    pub because: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ThinkQuestionParams {
    pub qname: String,
    pub question: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ThinkListParams {
    /// Filter to one thinking kind: hypothesis | mental_model |
    /// failed_attempt | open_question.
    pub kind: Option<String>,
    pub symbol: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct InvestigateParams {
    /// Natural-language or keyword query.
    /// Supports inline exclusion syntax: "drift playhead -sample -waveform".
    pub query: String,
    /// Number of top entry-point symbols to fully expand (default: 5).
    #[serde(default = "default_investigate_depth")]
    pub depth: u32,
    /// Filter by symbol kind: module, function, method, class, variable.
    pub kind: Option<String>,
    /// Filter by language (e.g. "swift", "python", "typescript", "rust").
    pub language: Option<String>,
    /// Include test-file symbols as entry-point candidates (default: false).
    #[serde(default)]
    pub include_tests: bool,
    /// Adjust output ordering and guidance for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    pub intent: Option<String>,
    /// Comma-separated terms to exclude (e.g. "sample editor,waveform").
    pub exclude: Option<String>,
    /// Comma-separated glob patterns to restrict to specific paths.
    pub paths: Option<String>,
    /// Named scope alias from .asd/scopes.toml.
    pub scope: Option<String>,
}

fn default_investigate_depth() -> u32 {
    10
}
fn default_impact_depth() -> u32 {
    3
}
fn default_git_depth() -> u32 {
    20
}
fn default_checklist_depth() -> u32 {
    10
}
fn default_test_depth() -> u32 {
    2
}
fn default_prepare_depth() -> u32 {
    10
}
fn default_prepare_git_depth() -> u32 {
    10
}

#[derive(Deserialize, JsonSchema)]
pub struct PrepareChangeParams {
    /// Free-form description of the intended change (treated as a search query).
    /// Supports inline exclusion syntax: "drift playhead -sample -waveform".
    pub description: String,
    /// Number of top entry-point symbols to expand (default: 7).
    #[serde(default = "default_prepare_depth")]
    pub depth: u32,
    /// Filter by symbol kind.
    pub kind: Option<String>,
    /// Filter by language.
    pub language: Option<String>,
    /// Include test-file symbols as entry-point candidates (default: false).
    #[serde(default)]
    pub include_tests: bool,
    /// Adjust output for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    pub intent: Option<String>,
    /// BFS depth for finding affected tests from the top entry point (default: 2).
    #[serde(default = "default_test_depth")]
    pub test_depth: u32,
    /// Number of recent git commits to scan per file (default: 10).
    #[serde(default = "default_prepare_git_depth")]
    pub git_depth: u32,
    /// Comma-separated terms to exclude (e.g. "sample editor,waveform").
    pub exclude: Option<String>,
    /// Comma-separated glob patterns to restrict to specific paths.
    pub paths: Option<String>,
    /// Named scope alias from .asd/scopes.toml.
    pub scope: Option<String>,
    /// Active task context to enrich the query (e.g. a CTX task description).
    /// Tokens are appended to the description before candidate scoring so the
    /// result set is biased toward symbols relevant to the current task.
    pub task_context: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ChecklistParams {
    /// Natural-language or keyword query.
    /// Supports inline exclusion syntax: "drift playhead -sample -waveform".
    pub query: String,
    /// Number of top entry-point symbols to analyse (default: 5).
    #[serde(default = "default_checklist_depth")]
    pub depth: u32,
    /// Filter by symbol kind.
    pub kind: Option<String>,
    /// Filter by language.
    pub language: Option<String>,
    /// Include test-file symbols as entry-point candidates (default: false).
    #[serde(default)]
    pub include_tests: bool,
    /// Adjust checklist framing for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    pub intent: Option<String>,
    /// Caller BFS depth for finding affected tests (default: 2).
    #[serde(default = "default_test_depth")]
    pub test_depth: u32,
    /// Comma-separated terms to exclude (e.g. "sample editor,waveform").
    pub exclude: Option<String>,
    /// Comma-separated glob patterns to restrict to specific paths.
    pub paths: Option<String>,
    /// Named scope alias from .asd/scopes.toml.
    pub scope: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImpactParams {
    /// Fully-qualified symbol name to analyse.
    pub qname: String,
    /// Caller-graph traversal depth (default: 3).
    #[serde(default = "default_impact_depth")]
    pub depth: u32,
    /// Number of recent git commits to look back per touched file (default: 20).
    #[serde(default = "default_git_depth")]
    pub git_depth: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct ArchitectureParams {
    /// How many packages, hotspots, and clusters to list (default: 12).
    #[serde(default = "default_arch_top")]
    pub top: usize,
}

fn default_arch_top() -> usize {
    12
}

#[derive(Deserialize, JsonSchema)]
pub struct DeadCodeParams {
    /// Max candidates to list; the total count is always reported (default: 50).
    #[serde(default = "default_dead_limit")]
    pub limit: usize,
    /// Include test functions (excluded by default).
    #[serde(default)]
    pub include_tests: bool,
}

fn default_dead_limit() -> usize {
    50
}

#[derive(Deserialize, JsonSchema)]
pub struct SinceParams {
    /// Base commit SHA (or branch/tag) to diff against HEAD.
    pub sha: String,
    /// Caller-graph BFS depth for blast radius (default: 3).
    #[serde(default = "default_impact_depth")]
    pub depth: u32,
    /// Number of recent git commits to scan per changed file (default: 10).
    #[serde(default = "default_since_git_depth")]
    pub git_depth: u32,
}

fn default_since_git_depth() -> u32 {
    10
}

#[derive(Deserialize, JsonSchema)]
pub struct InvariantAddParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// One-line invariant summary.
    pub summary: String,
    /// Author identifier (default: "asd-mcp-agent").
    #[serde(default = "default_author_id")]
    pub author_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct InvariantListParams {
    /// Filter to a single symbol's invariants. Omit to list all.
    pub qname: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FeedbackMarkParams {
    /// The search query that produced this result.
    pub query: String,
    /// Fully-qualified symbol name being rated.
    pub qname: String,
    /// Verdict: "useful", "noisy", "missing", "wrong_layer",
    /// "already_covered" (Plan C t-005), or "diagnostic_only" (Plan C t-005).
    pub verdict: String,
    /// Optional free-text note explaining the verdict.
    pub note: Option<String>,
    /// Agent/author identifier (default: "asd-mcp-agent").
    #[serde(default = "default_author_id")]
    pub author_id: String,
    /// Plan E t-009: when verdict is "already_covered", the qname of
    /// the symbol whose behavior covers this one. Auto-writes a paired
    /// Mapping ledger entry alongside the FeedbackEntry.
    pub covered_by: Option<String>,
    /// Plan J t-014: optional expiry in days from now. After `now + N
    /// days` the entry no longer influences ranking. Useful for
    /// false-positive marks that should auto-decay.
    pub ttl_days: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FeedbackListParams {
    /// Filter to feedback for a specific symbol qname. Omit to list all.
    pub qname: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FeedbackPromoteParams {
    /// Fully-qualified symbol name to promote.
    pub qname: String,
    /// The domain concept this symbol is the source-of-truth for (e.g. "Drift Pad playhead").
    pub concept: String,
    /// Agent/author identifier (default: "asd-mcp-agent").
    #[serde(default = "default_author_id")]
    pub author_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CodeReadParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct EffectsOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CallersOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CalleesOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerGetParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// Include entries that have been superseded (default: false).
    #[serde(default)]
    pub include_superseded: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerFindParams {
    /// Filter by ledger kind (decision, assumption, constraint, rationale, hazard, tradeoff).
    pub kind: Option<String>,
    /// Filter by tag (must be present on entry).
    pub tag: Option<String>,
    /// Filter by author id.
    pub author_id: Option<String>,
    /// Max results to return (default: 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerAppendParams {
    /// Fully-qualified symbol name this entry attaches to.
    pub qname: String,
    /// Ledger entry kind.
    pub kind: String,
    /// One-line summary.
    pub summary: String,
    /// Optional free-form body (markdown ok).
    pub body: Option<String>,
    /// Optional tags.
    pub tags: Option<Vec<String>>,
    /// Author kind: "agent" or "human" (default: "agent").
    #[serde(default = "default_author_kind")]
    pub author_kind: String,
    /// Author id (default: "asd-mcp").
    #[serde(default = "default_author_id")]
    pub author_id: String,
    /// Plan B t-002: optional classification role/intent tag
    /// (e.g. "diagnostic-test", "fast-test", "fixture-path").
    pub role: Option<String>,
    /// Plan B t-002: optional canonical reproduction or validation command
    /// (e.g. "swift test --filter SongPlayersTests").
    pub command: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerApproveParams {
    /// Entry id (returned by a prior `ledger_append` call).
    pub entry_id: String,
    /// Approver identifier — recorded on the entry as `approved-by:<id>`.
    pub approver: String,
    /// Approver kind. Must match an `approver:*` tag on the original
    /// entry (e.g., "human", "senior_agent") unless `approver` itself
    /// matches directly.
    #[serde(default = "default_approver_kind_mcp")]
    pub approver_kind: String,
    /// Optional approver rationale — appended to the entry body as an
    /// "Approver note" section.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerRejectParams {
    /// Entry id to reject.
    pub entry_id: String,
    /// Reviewer id — recorded as `rejected-by:<id>`.
    pub reviewer: String,
    /// Reviewer kind. Same approver-match rule as approve.
    #[serde(default = "default_approver_kind_mcp")]
    pub reviewer_kind: String,
    /// Rejection reason (required). Appended to the entry body.
    pub reason: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerWithdrawParams {
    /// Entry id to withdraw.
    pub entry_id: String,
    /// Author id — must match the original `entry.author.id`.
    pub author_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerSupersedeParams {
    /// Qname the new entry attaches to.
    pub qname: String,
    /// Entry ids superseded by the new entry.
    pub supersedes: Vec<String>,
    /// Ledger kind for the new entry (decision, rationale, hazard, …).
    pub kind: String,
    /// One-line summary.
    pub summary: String,
    /// Optional body.
    #[serde(default)]
    pub body: Option<String>,
    /// Author kind (default: "agent").
    #[serde(default = "default_author_kind")]
    pub author_kind: String,
    /// Author id (default: "asd-mcp").
    #[serde(default = "default_author_id")]
    pub author_id: String,
}

fn default_approver_kind_mcp() -> String {
    "human".to_string()
}

#[derive(Deserialize, JsonSchema)]
pub struct TracesOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// Maximum number of trace records to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReindexParams {
    /// Absolute or relative path to a source file or directory to reindex.
    /// Relative paths are resolved from the process working directory.
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SyncParams {
    /// Project root to sync into (`.asd/v1/` is appended). Defaults to the
    /// directory of the active db.
    #[serde(default)]
    pub dir: Option<String>,
    /// Also remove orphaned sidecar files for symbols that no longer exist.
    #[serde(default)]
    pub prune: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct TestSummaryParams {
    /// Raw test-runner output to summarize (cargo/pytest auto-detected; others
    /// via a generic scan).
    pub output: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct MapParams {
    /// Return the summary without writing any Ownership ledger entries.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerRebindParams {
    /// symbol_id of the old symbol whose ledger entries should be re-parented.
    pub from_symbol_id: String,
    /// Fully-qualified name of the new symbol to resolve and bind to.
    pub to_qname: String,
    /// Agent or user performing the rebind.
    #[serde(default = "default_author_id")]
    pub agent_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct AuditTailParams {
    /// Substring match on event_type (e.g., `ledger.approve`,
    /// `ledger.` for all ledger events).
    #[serde(default)]
    pub event_type: Option<String>,
    /// Return only events AFTER this `event_id` (exclusive). Use for
    /// incremental polling.
    #[serde(default)]
    pub since: Option<String>,
    /// Exact match on actor_id.
    #[serde(default)]
    pub actor: Option<String>,
    /// Exact match on outcome.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Max events to return (default 200, max 1000).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct EffectDeclareParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// List of declared effects. Each element is a JSON object matching the
    /// `Effect` schema: `{ "effect": "<category>", "qualifiers": ..., "note": ... }`.
    pub declared: Vec<serde_json::Value>,
    /// Author id (default: "asd-mcp"). Surfaced to the policy gate so rules
    /// can scope by agent identity.
    #[serde(default = "default_author_id")]
    pub author_id: String,
}

fn default_limit() -> u32 {
    50
}
fn default_author_kind() -> String {
    "agent".to_string()
}
fn default_author_id() -> String {
    "asd-mcp".to_string()
}

// -- Scratch parameter types ------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct ScratchWriteParams {
    /// Working notes content (markdown OK).
    pub content: String,
    /// Optional: qualified name of the symbol to attach this note to.
    /// In planning mode, the symbol does not need to exist yet.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Optional: named investigation context (e.g. "tracing-sync-bug").
    #[serde(default)]
    pub workflow: Option<String>,
    /// Optional: time-to-live in hours. When not set, no expiry.
    #[serde(default)]
    pub ttl_hours: Option<i64>,
    /// Optional: freeform tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// When true, marks this entry as pre-implementation planning notes.
    /// Adds the "planning" tag automatically. Unresolved symbol names are
    /// stored as-is rather than causing an error — useful for naming planned
    /// symbols that do not exist in the index yet.
    #[serde(default)]
    pub planning: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScratchListParams {
    /// Filter by symbol qualified name.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Filter by workflow name.
    #[serde(default)]
    pub workflow: Option<String>,
    /// Filter by session/agent_id.
    #[serde(default)]
    pub session: Option<String>,
    /// Filter by status: "draft", "promoted", "discarded". Defaults to "draft".
    #[serde(default)]
    pub status: Option<String>,
    /// Max results to return (default: 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScratchReadParams {
    /// Scratch entry ID (`scr_…`).
    pub scratch_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScratchUpdateParams {
    /// Scratch entry ID to update.
    pub scratch_id: String,
    /// Replacement content (replaces previous content entirely).
    pub content: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScratchDiscardParams {
    /// Scratch entry ID to discard.
    pub scratch_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScratchPromoteParams {
    /// Scratch entry ID to promote.
    pub scratch_id: String,
    /// Ledger kind: decision, assumption, constraint, rationale, hazard, tradeoff, invariant, ownership, proof, validation_scenario, known_bug, concept.
    pub kind: String,
    /// Symbol qualified name. Required if the scratch entry has no symbol attached.
    #[serde(default)]
    pub qname: Option<String>,
    /// One-line summary. Defaults to the first non-empty line of the scratch content.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScratchCleanParams {
    /// Delete entries older than this many hours. Required.
    pub older_than_hours: u32,
    /// Comma-separated statuses to clean: "discarded,promoted" (default).
    #[serde(default = "default_scratch_clean_statuses")]
    pub statuses: String,
    /// When true, report what would be deleted without removing anything.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_scratch_clean_statuses() -> String {
    "discarded,promoted".to_string()
}

// -- New tool parameter types -----------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Concept or keyword(s) to search for.
    pub query: String,
    /// Filter by symbol kind.
    pub kind: Option<String>,
    /// Filter by language.
    pub language: Option<String>,
    /// Max results (default: 20).
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    /// Include test-file symbols (default: false).
    #[serde(default)]
    pub include_tests: bool,
    /// Adjust guidance for a specific intent (bugfix, feature, refactor, test, architecture, ui).
    pub intent: Option<String>,
    /// Named scope alias from .asd/scopes.toml.
    pub scope: Option<String>,
    /// Comma-separated glob patterns to restrict to specific paths.
    pub paths: Option<String>,
    /// Comma-separated exclusion terms.
    pub exclude: Option<String>,
    /// Skip document index; return symbol hits only (default: false).
    #[serde(default)]
    pub symbols_only: bool,
    /// Agent token budget (default: 8000).
    #[serde(default = "default_agent_budget")]
    pub agent_budget: u32,
}

fn default_agent_budget() -> u32 {
    8000
}

#[derive(Deserialize, JsonSchema)]
pub struct ContextForParams {
    /// Comma-separated fully-qualified symbol names.
    pub qnames: String,
    /// Token budget for the output.
    pub budget_tokens: Option<u32>,
    /// Include full source body (default: false).
    #[serde(default)]
    pub include_body: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskCloseParams {
    /// Free-text proof of completion (default: "task completed").
    pub proof: Option<String>,
    /// Comma-separated qnames to annotate. If omitted, resolved from git HEAD changed files.
    pub symbols: Option<String>,
    /// Mark the task as validated.
    #[serde(default)]
    pub validated: bool,
    /// Validation note (used when validated = true).
    pub validation_note: Option<String>,
    /// Reference to validation evidence (file, URL, or test name).
    pub evidence: Option<String>,
    /// CTX plan ID.
    pub plan: Option<String>,
    /// CTX task ID.
    pub task: Option<String>,
    /// Author id (default: "asd-task-close").
    #[serde(default = "default_task_close_author")]
    pub author: String,
}

fn default_task_close_author() -> String {
    "asd-task-close".to_string()
}

#[derive(Deserialize, JsonSchema)]
pub struct VerifyEffectsParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// Write verification result back to the store (default: false).
    #[serde(default)]
    pub write: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Show source files modified since last commit (default: false).
    #[serde(default)]
    pub show_dirty: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScorecardParams {
    /// Named scope alias.
    pub scope: Option<String>,
    /// Comma-separated glob patterns.
    pub paths: Option<String>,
    /// Drill-down dimension: "truth", "feedback", "change", "uncertainty", "workflow".
    pub drill_down: Option<String>,
    /// Max symbols shown in drill-down (default: 10).
    #[serde(default = "default_scorecard_limit")]
    pub limit: u32,
}

fn default_scorecard_limit() -> u32 {
    10
}

#[derive(Deserialize, JsonSchema)]
pub struct AnnotateCommitParams {
    /// Git commit SHA (default: "HEAD").
    pub sha: Option<String>,
    /// Actually write entries (default: false — dry-run).
    #[serde(default)]
    pub write: bool,
    /// Author id (defaults to git user.name).
    pub author: Option<String>,
    /// Additional task context appended to commit body for annotation extraction.
    pub task_description: Option<String>,
    /// CTX task ID — written as ctx:task:<id> provenance tag.
    pub ctx_task: Option<String>,
    /// CTX plan ID — written as ctx:plan:<id> provenance tag.
    pub ctx_plan: Option<String>,
}

// -- Tool implementations ---------------------------------------------------
//
// MCP tool naming conventions (see CLI parity audit):
// - Tools that mirror a CLI verb 1:1 use the same name (search, callers, callees,
//   impact, since, investigate, prepare_change, ...).
// - `code_*` prefix is used for tools whose bare CLI name would collide in the
//   flat MCP namespace shared with other servers: `code_search`, `code_read`,
//   `code_query` — CLI equivalents are `asd search` and `asd read`.
// - Subcommand-style CLI verbs are flattened with an underscore in MCP because
//   MCP has no nested-command concept: MCP `ledger_append` = CLI `asd ledger append`,
//   MCP `scratch_write` = CLI `asd scratch write`, and so on for feedback_*,
//   invariant_*, audit_*.
// - No `_of` suffix: use bare names (callers, callees, effects, traces).
