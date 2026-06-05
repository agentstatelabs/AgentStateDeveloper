//! AgentStateDeveloper MCP stdio server — exposes ASD read/write operations
//! as MCP tools for coding agents.
//!
//! Patterns mirror `agentstategraph-mcp::server` (same `rmcp` version).

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Command as Proc;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use agentstatedeveloper_adapters::default_adapters;
use agentstatedeveloper_core::{
    ASD_PATH_PREFIX, AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore,
    AsgScratchStore, AuditEvent, Author, AuthorKind, CleanFilter, ConclusionClass, Decision,
    Effect, EffectCategory, EffectDecl, EffectStore, Engine, FeedbackEntry, FeedbackStore,
    FeedbackVerdict, FtsFilters, IndexStore, LedgerEntry, LedgerKind, LedgerStore, Mismatch,
    ParsedSymbol, Rebind, ScratchEntry, ScratchFilter, ScratchStatus, ScratchStore, SearchDocsDb,
    SearchFtsDb, SidecarState, Situation, Symbol, Verification, VerificationSource,
    VerificationStatus, WorkflowSummary, actions, append_workflow_session,
    apply_feedback_adjustments, apply_file_scope_feedback, brief,
    build_feedback_state_from_entries, classify_layer_sym, compute_trust_score,
    compute_uncertainty, conclusions_export, confidence_reason, confidence_scores,
    derive_cold_hints, detect_ambiguous_tokens, detect_confidence_warnings, detect_possible_misses,
    detect_workflow, discover_symbol_ownership, effect_detail_reason, emit_audit, estimate_tokens,
    event_types, explain_feedback_impacts, explain_match, extract_summary, find_candidates,
    find_covering_tests, gather_recency, git_dirty_files, glob_match, hybrid_boost, intent_focus,
    intent_layer_order, kind_str, load_layer_overrides, load_scope_aliases, parse_intent,
    parse_query, paths, propose_test_path, recipes, resolve_scope, result_bucket,
    score_evidence_quality, sidecar_lifecycle_state, stale_warning, suggest_better_queries,
    suggest_scoped_queries, symbol_tier, thinking, trim_for_agent,
};

/// The AgentStateDeveloper MCP server.
///
/// `db_path` is stored inside an `Arc<RwLock<PathBuf>>` so the registry
/// watcher (see [`AsdMcpServer::with_registry_tracking`]) can swap it in
/// place when the user runs `asd repo use <other>`.  Reads are sync and
/// fast — we never hold the guard across `.await`.
#[derive(Clone)]
pub struct AsdMcpServer {
    engine: Arc<Mutex<Engine>>,
    db_path: Arc<std::sync::RwLock<PathBuf>>,
    audit_log_path: Option<PathBuf>,
    tool_router: ToolRouter<Self>,
}

// -- Parameter types --------------------------------------------------------

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

#[tool_router]
impl AsdMcpServer {
    pub fn new(engine: Arc<Mutex<Engine>>, db_path: PathBuf) -> Self {
        Self::with_audit_log(engine, db_path, None)
    }

    pub fn with_audit_log(
        engine: Arc<Mutex<Engine>>,
        db_path: PathBuf,
        audit_log_path: Option<PathBuf>,
    ) -> Self {
        Self {
            engine,
            db_path: Arc::new(std::sync::RwLock::new(db_path)),
            audit_log_path,
            tool_router: Self::tool_router(),
        }
    }

    /// Build the server and spawn a background watcher on the shared registry
    /// at `~/.config/asd/repos.toml`. When the active repo changes, the
    /// watcher opens the new db and atomically swaps both the underlying
    /// `Engine` and `db_path` so subsequent tool calls hit the new repo
    /// without restarting the process.
    ///
    /// Pass `false` for `track_registry` when the caller fixed the db
    /// explicitly (e.g. `ASD_DB=...`) — in that case the user opted out of
    /// follow-the-active-repo behavior and we leave the engine alone.
    pub fn with_registry_tracking(
        engine: Arc<Mutex<Engine>>,
        db_path: PathBuf,
        audit_log_path: Option<PathBuf>,
        track_registry: bool,
    ) -> Self {
        let s = Self::with_audit_log(engine, db_path, audit_log_path);
        if track_registry {
            spawn_registry_watcher(s.engine.clone(), s.db_path.clone());
        }
        s
    }

    /// Clone of the currently-open db path. Cheap (one std::RwLock read + clone).
    pub fn db_path(&self) -> PathBuf {
        self.db_path
            .read()
            .expect("db_path lock poisoned")
            .clone()
    }

    // -- Read tools --

    #[tool(
        description = "Health check: reports MCP server status, ASG db path, indexed symbol count, and total artifact counts (symbols + ledger entries + effects)."
    )]
    async fn health(&self) -> String {
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let symbol_count = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.len(),
            _ => 0,
        };
        let raw_db_path = self.db_path();
        let db_path = raw_db_path
            .canonicalize()
            .unwrap_or(raw_db_path)
            .to_string_lossy()
            .to_string();
        let ledger_prefix = format!("{}/ledger", ASD_PATH_PREFIX);
        // Plan L t-010: walk the ledger tree once, derive BOTH the
        // orphan count and the total entry count from the same pass.
        // Orphan = ledger entries keyed by a symbol_id that isn't in
        // the qname index. Total entries = sum across all symbol
        // subtrees (one inner object per symbol; each value-of-key
        // is one entry).
        let (orphan_count, ledger_entry_count) =
            match engine.repo.get_tree(&ref_name, &ledger_prefix) {
                Ok(serde_json::Value::Object(by_symbol)) => {
                    let indexed_prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
                    let indexed: std::collections::HashSet<String> =
                        match engine.repo.get_tree(&ref_name, &indexed_prefix) {
                            Ok(serde_json::Value::Object(m)) => m
                                .values()
                                .filter_map(|v| {
                                    v.get("symbol_id")?.as_str().map(|s| s.to_string())
                                })
                                .collect(),
                            _ => std::collections::HashSet::new(),
                        };
                    let mut orphans = 0usize;
                    let mut entries = 0usize;
                    for (sym_id, subtree) in &by_symbol {
                        if let serde_json::Value::Object(es) = subtree {
                            entries += es.len();
                        }
                        if !indexed.contains(sym_id) {
                            orphans += 1;
                        }
                    }
                    (orphans, entries)
                }
                _ => (0, 0),
            };
        // Plan L t-010: count symbols with at least one declared
        // effect. Top-level keys under /asd/v1/effects are symbol_ids.
        let effects_prefix = format!("{}/effects", ASD_PATH_PREFIX);
        let effects_count = match engine.repo.get_tree(&ref_name, &effects_prefix) {
            Ok(serde_json::Value::Object(map)) => map.len(),
            _ => 0,
        };
        let stale = stale_warning(&self.db_path(), 3600);
        // Plan J t-005: compute FTS-side symbol count so the
        // response can flag any divergence from the ASG-side count.
        // Field reports (M21) had agents seeing different numbers
        // from `asd status` (FTS) and `asd health` (ASG) on the
        // same repo with no explanation. Now both surfaces report
        // BOTH counts plus a consistency advisory string when they
        // diverge.
        let fts_symbol_count = SearchFtsDb::open(&self.db_path())
            .ok()
            .map(|f| f.symbol_count() as usize)
            .unwrap_or(0);
        let index_consistency =
            agentstatedeveloper_core::compute_index_consistency(symbol_count, fts_symbol_count);
        let payload = serde_json::json!({
            "status": "ok",
            "db_path": db_path,
            // Bare `symbol_count` kept for backward compatibility —
            // older callers (Plan L t-010 acceptance: "backward
            // compat: bare symbol_count keeps working") depend on it.
            "symbol_count": symbol_count,
            "orphaned_symbol_count": orphan_count,
            // Plan L t-010: total artifact rollup. Previously the
            // bare symbol_count was being misread as "total things in
            // the index" — it's only the qname count. This breakdown
            // makes the distinction explicit.
            "artifact_count": {
                "symbols": symbol_count,
                "ledger_entries": ledger_entry_count,
                "effects": effects_count,
            },
            "index_consistency": index_consistency,
            "stale": stale,
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Query indexed symbols. Filters (all optional, AND-combined): name_contains, kind, language. Returns up to `limit` symbol summaries. (CLI: `asd search` covers the search variant.)"
    )]
    async fn code_query(&self, params: Parameters<CodeQueryParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);

        let qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => return "[]".to_string(),
        };

        let index = AsgIndexStore::from_engine(&engine);
        let mut symbols = Vec::new();
        let kind_filter = p.kind.as_deref().map(|k| k.to_lowercase());
        let name_filter = p.name_contains.as_deref();
        let lang_filter = p.language.as_deref();
        let limit = p.limit.max(1) as usize;

        for qname in qnames {
            if let Some(needle) = name_filter
                && !qname.contains(needle)
            {
                continue;
            }
            let sym = match index.get_symbol_by_qname(&ref_name, &qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            if let Some(lang) = lang_filter
                && sym.language != lang
            {
                continue;
            }
            if let Some(ref k) = kind_filter {
                let sym_kind = match sym.kind {
                    agentstatedeveloper_core::SymbolKind::Module => "module",
                    agentstatedeveloper_core::SymbolKind::Function => "function",
                    agentstatedeveloper_core::SymbolKind::Method => "method",
                    agentstatedeveloper_core::SymbolKind::Class => "class",
                    agentstatedeveloper_core::SymbolKind::Variable => "variable",
                };
                if sym_kind != k {
                    continue;
                }
            }
            symbols.push(sym);
            if symbols.len() >= limit {
                break;
            }
        }

        symbols.sort_by(|a, b| a.qname.cmp(&b.qname));
        // Plan E t-007: brief = compact per-symbol projection.
        if brief::brief_from_env() {
            let projected: Vec<_> = symbols.iter().map(brief::brief_symbol).collect();
            return serde_json::to_string(&projected).unwrap_or_else(|_| "[]".to_string());
        }
        serde_json::to_string(&symbols).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Ranked concept search over indexed symbols using FTS5/BM25. Returns symbols sorted by relevance. Use this when you need to discover entry points for a feature or concept — 'playhead over clips', 'auth flow', 'export pipeline', etc. (CLI: `asd search`.)"
    )]
    async fn code_search(&self, params: Parameters<CodeSearchParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (tokens, mut exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
            {
                exclusions.push(term);
            }
        }

        if tokens.is_empty() {
            return "[]".to_string();
        }

        let limit = p.limit.max(1) as usize;
        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref scope) = p.scope {
            paths_filter.extend(resolve_scope(scope, &db_path));
        }
        if let Some(ref paths) = p.paths {
            paths_filter.extend(
                paths
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            tests_only: p.tests_only,
            exclude_terms: exclusions.clone(),
            paths_filter,
            exclude_paths: Vec::new(),
            exclude_languages: Vec::new(),        };

        // --- FTS path ---
        let fts_result = SearchFtsDb::open(&db_path)
            .ok()
            .filter(|fts| fts.has_data())
            .and_then(|fts| fts.search(&p.query, &filters, limit * 4).ok());

        if let Some(hits) = fts_result {
            let ledger_store = AsgLedgerStore::from_engine(&engine);
            let mut scored: Vec<(f64, _)> = hits
                .into_iter()
                .map(|hit| {
                    let boost = hybrid_boost(&hit, &tokens);
                    let entries = ledger_store
                        .list_entries(&ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let text = entries
                        .iter()
                        .map(|e| e.summary.to_lowercase())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let ledger_boost = if text.is_empty() {
                        0.0
                    } else {
                        tokens.iter().filter(|t| text.contains(t.as_str())).count() as f64
                    };
                    (hit.bm25_score + boost + ledger_boost, hit)
                })
                .collect();
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.qname.cmp(&b.1.qname))
            });
            if !exclusions.is_empty() {
                scored.retain(|(_, hit)| {
                    let qn = hit.qname.to_lowercase();
                    let fl = hit.file.to_lowercase();
                    let doc = hit.doc.as_deref().unwrap_or("").to_lowercase();
                    let sig = hit.signature.as_deref().unwrap_or("").to_lowercase();
                    !exclusions.iter().any(|e| {
                        qn.contains(e.as_str())
                            || fl.contains(e.as_str())
                            || doc.contains(e.as_str())
                            || sig.contains(e.as_str())
                    })
                });
            }
            scored.truncate(limit);

            let recency = gather_recency(200, 14.0);
            let index_store = AsgIndexStore::from_engine(&engine);
            let raw_scores: Vec<f64> = scored.iter().map(|(s, _)| *s).collect();
            let confidences = confidence_scores(&raw_scores);
            let mut layers_present: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let results: Vec<serde_json::Value> = scored
                .iter()
                .zip(confidences.iter())
                .map(|((score, hit), conf)| {
                    let tier = hit.tier;
                    let layer = classify_layer_sym(&hit.file, &hit.qname, tier, &layer_overrides);
                    layers_present.insert(layer.to_string());
                    let summary = extract_summary(hit.doc.as_deref(), hit.signature.as_deref());
                    let rec = recency.get(&hit.file);
                    let is_hot = rec.map(|r| r.hot).unwrap_or(false);
                    let entries = ledger_store
                        .list_entries(&ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let has_ledger = !entries.is_empty();
                    let match_reasons = index_store
                        .get_symbol_by_qname(&ref_name, &hit.qname)
                        .ok()
                        .flatten()
                        .map(|sym| explain_match(&sym, &tokens, &entries, is_hot))
                        .unwrap_or_default();
                    let bucket = result_bucket(&hit.file, &match_reasons, has_ledger, is_hot);
                    serde_json::json!({
                        "score": score,
                        "confidence": conf,
                        "bucket": bucket,
                        "qname": hit.qname,
                        "kind": hit.kind,
                        "language": hit.language,
                        "file": hit.file,
                        "line": hit.line,
                        "tier": tier,
                        "layer": layer,
                        "summary": summary,
                        "signature": hit.signature,
                        "doc": hit.doc,
                        "last_touched_days": rec.and_then(|r| r.last_touched_days),
                        "hot": is_hot,
                        "match_reasons": match_reasons,
                    })
                })
                .collect();
            let layers_ref: std::collections::HashSet<&str> =
                layers_present.iter().map(|s| s.as_str()).collect();
            let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
            let possible_misses = detect_possible_misses(&p.query, &layers_ref, results.len());
            // Document hits from the broad corpus (markdown, config, manifests, etc.)
            let doc_hits: Vec<serde_json::Value> = SearchDocsDb::open(&db_path)
                .ok()
                .filter(|db| !db.is_empty())
                .and_then(|db| db.search(&tokens, limit, None).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "source": "document",
                        "score": h.bm25_score,
                        "kind": h.kind,
                        "path": h.path,
                        "line": h.span_start,
                        "title": h.title,
                        "preview": h.preview,
                        "owner_symbol_id": h.owner_symbol_id,
                    })
                })
                .collect();
            let stale = stale_warning(&db_path, 3600);
            // Plan D t-007: brief mode projects each FTS hit down to
            // {qname, file:line, signature, doc, score} and drops the
            // ambiguous_terms / possible_misses / confidence / document_hits
            // arrays that don't add per-hit information.
            if brief::brief_from_env() {
                let out = serde_json::json!({
                    "query": p.query,
                    "results": brief::brief_search_results(&results),
                    "stale": stale,
                    "query_id": brief::query_id("code_search", &[&p.query]),
                });
                return serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string());
            }
            let out = serde_json::json!({
                "query": p.query,
                "ambiguous_terms": ambiguous_terms,
                "possible_misses": possible_misses,
                "results": results,
                "document_hits": doc_hits,
                "stale": stale,
                "confidence": {
                    "strong": "concept and cross-layer queries that span multiple terms (e.g. 'master volume strategy', 'export pipeline')",
                    "weak": "exact-identifier lookups and 'all references to X' — use `references` for those; broad single-word queries may surface unrelated files",
                },
            });
            return serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string());
        }

        // --- Fallback: in-memory O(N) scoring ---
        let kind_filter = filters.kind;
        let lang_filter = filters.language;
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => return "[]".to_string(),
        };
        let index = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let mut scored: Vec<(u32, agentstatedeveloper_core::Symbol)> = Vec::new();
        for qname in &qnames {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            let sk = format!("{:?}", sym.kind).to_lowercase();
            if let Some(ref k) = kind_filter {
                if &sk != k {
                    continue;
                }
            }
            if let Some(ref lang) = lang_filter {
                if &sym.language != lang {
                    continue;
                }
            }
            let qn = sym.qname.to_lowercase();
            let sig = sym.signature.as_deref().unwrap_or("").to_lowercase();
            let doc = sym.doc.as_deref().unwrap_or("").to_lowercase();
            let file = sym.file.to_lowercase();
            let ledger_text: String = ledger_store
                .list_entries(&ref_name, &sym.symbol_id)
                .unwrap_or_default()
                .iter()
                .map(|e| e.summary.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            let mut score: u32 = 0;
            for token in &tokens {
                if qn.contains(token.as_str()) {
                    score += 4;
                }
                if !sig.is_empty() && sig.contains(token.as_str()) {
                    score += 3;
                }
                if !doc.is_empty() && doc.contains(token.as_str()) {
                    score += 3;
                }
                if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) {
                    score += 2;
                }
                if file.contains(token.as_str()) {
                    score += 1;
                }
            }
            if score > 0 {
                scored.push((score, sym));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.qname.cmp(&b.1.qname)));
        scored.truncate(limit);
        let recency = gather_recency(200, 14.0);
        let results: Vec<serde_json::Value> = scored
            .iter()
            .map(|(score, sym)| {
                let tier = symbol_tier(&sym.file);
                let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
                let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
                let rec = recency.get(&sym.file);
                serde_json::json!({
                    "score": score,
                    "qname": sym.qname,
                    "kind": format!("{:?}", sym.kind).to_lowercase(),
                    "language": sym.language,
                    "file": sym.file,
                    "line": sym.start.line,
                    "tier": tier,
                    "layer": layer,
                    "summary": summary,
                    "signature": sym.signature,
                    "doc": sym.doc,
                    "last_touched_days": rec.and_then(|r| r.last_touched_days),
                    "hot": rec.map(|r| r.hot).unwrap_or(false),
                })
            })
            .collect();
        serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List named scope aliases from `.asd/scopes.toml`. Use this first when broad searches return noise — narrow with `--scope <name>` or `--paths <glob>` on search, prepare_change, investigate, impact, checklist, since. (CLI: `asd scopes list`.)"
    )]
    async fn scopes_list(&self) -> String {
        let engine = self.engine.lock().await;
        let db_path = self.db_path();
        drop(engine);
        let aliases = load_scope_aliases(&db_path);
        let scopes_file = db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("scopes.toml")
            .display()
            .to_string();
        let hint = if aliases.is_empty() {
            "no scopes defined; create .asd/scopes.toml with entries like `audio-engine = [\"Packages/AudioEngine/**\"]`, or pass `--paths <glob>` directly to any scoped command"
        } else {
            "pass `--scope <name>` to search/prepare_change/investigate/impact/checklist/since"
        };
        serde_json::to_string(&serde_json::json!({
            "scopes": aliases,
            "count": aliases.len(),
            "scopes_file": scopes_file,
            "usage_hint": hint,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "List ledger entries bucketed by the six Plan B conclusion classes (decisions, classifications, mappings, hazards, recipes, followups). Optional `class` filters to one bucket; optional `symbol` filters to one qname. Use to audit what conclusions exist before exporting to .asd/conclusions/*.jsonl. (CLI: `asd conclusions list`.)"
    )]
    async fn conclusions_list(&self, params: Parameters<ConclusionsListParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let ledger = AsgLedgerStore::from_engine(&engine);

        // Parse optional class filter.
        let target_class: Option<ConclusionClass> = p.class.as_deref().and_then(|s| match s {
            "decisions" => Some(ConclusionClass::Decisions),
            "classifications" => Some(ConclusionClass::Classifications),
            "mappings" => Some(ConclusionClass::Mappings),
            "hazards" => Some(ConclusionClass::Hazards),
            "recipes" => Some(ConclusionClass::Recipes),
            "followups" => Some(ConclusionClass::FollowUps),
            _ => None,
        });
        if p.class.is_some() && target_class.is_none() {
            return err_json(&format!(
                "unknown conclusion class: {}. valid: decisions, classifications, mappings, hazards, recipes, followups",
                p.class.as_deref().unwrap_or("")
            ));
        }

        // Resolve target symbols.
        let symbol_ids: Vec<(String, String)> = if let Some(qname) = p.symbol.as_deref() {
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => vec![(sym.symbol_id, sym.qname)],
                _ => Vec::new(),
            }
        } else {
            let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
            let tree = engine
                .repo
                .get_tree(&ref_name, &prefix)
                .unwrap_or(serde_json::Value::Null);
            let qnames: Vec<String> = match tree {
                serde_json::Value::Object(m) => m.keys().cloned().collect(),
                _ => Vec::new(),
            };
            let mut out = Vec::new();
            for qn in qnames {
                if let Ok(Some(sym)) = index.get_symbol_by_qname(&ref_name, &qn) {
                    out.push((sym.symbol_id, sym.qname));
                }
            }
            out
        };

        use std::collections::BTreeMap;
        let mut buckets: BTreeMap<&'static str, Vec<serde_json::Value>> = BTreeMap::new();
        for class in ConclusionClass::all() {
            if target_class.is_none() || target_class == Some(*class) {
                buckets.insert(class.filename_stem(), Vec::new());
            }
        }

        for (sym_id, qname) in &symbol_ids {
            let entries = ledger.list_entries(&ref_name, sym_id).unwrap_or_default();
            for entry in entries {
                let class = entry.kind.conclusion_class();
                if let Some(filter) = target_class {
                    if class != filter {
                        continue;
                    }
                }
                if let Some(bucket) = buckets.get_mut(class.filename_stem()) {
                    bucket.push(serde_json::json!({
                        "entry_id": entry.entry_id,
                        "kind": entry.kind.as_str(),
                        "qname": qname,
                        "symbol_id": entry.symbol_id,
                        "summary": entry.summary,
                        "role": entry.role,
                        "command": entry.command,
                        "tags": entry.tags,
                        "created_at": entry.created_at,
                    }));
                }
            }
        }

        let total: usize = buckets.values().map(|v| v.len()).sum();
        let full = serde_json::json!({
            "class": target_class.map(|c| c.filename_stem()),
            "symbol": p.symbol,
            "total": total,
            "buckets": buckets,
        });
        // Plan F t-006: brief mode drops symbol_id + null role/command/tags.
        let out = if brief::brief_from_env() {
            brief::brief_conclusions_list(&full)
        } else {
            full
        };
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Write all ledger conclusions to compact JSONL files (one per class) under `.asd/conclusions/`. Byte-stable when no new entries — safe to run from a pre-commit hook. Returns per-class entry + byte counts. (CLI: `asd conclusions export`.)"
    )]
    async fn conclusions_export(&self, params: Parameters<ConclusionsExportParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;

        let out_dir: std::path::PathBuf =
            p.out.map(std::path::PathBuf::from).unwrap_or_else(|| {
                db_path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join(".asd")
                    .join("conclusions")
            });

        match conclusions_export::export_all(&engine, &out_dir) {
            Ok(counts) => {
                let total_entries: usize = counts.iter().map(|(_, n, _)| n).sum();
                let total_bytes: u64 = counts.iter().map(|(_, _, b)| b).sum();
                serde_json::to_string(&serde_json::json!({
                    "out_dir": out_dir.display().to_string(),
                    "files": counts.iter().map(|(stem, n, b)| serde_json::json!({
                        "class": stem,
                        "file": format!("{stem}.jsonl"),
                        "entries": n,
                        "bytes": b,
                    })).collect::<Vec<_>>(),
                    "total_entries": total_entries,
                    "total_bytes": total_bytes,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => err_json(&format!("conclusions export failed: {e}")),
        }
    }

    #[tool(
        description = "Read `.asd/conclusions/*.jsonl` back into the local ledger. Idempotent (entries keyed by entry_id). Run after `git pull` or on a fresh clone to populate ASG with the committed conclusions. (CLI: `asd conclusions import`.)"
    )]
    async fn conclusions_import(&self, params: Parameters<ConclusionsImportParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;

        let in_dir: std::path::PathBuf =
            p.in_dir.map(std::path::PathBuf::from).unwrap_or_else(|| {
                db_path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join(".asd")
                    .join("conclusions")
            });

        match conclusions_export::import_all(&engine, &in_dir, "asd-mcp") {
            Ok(results) => {
                let total_imported: usize = results.iter().map(|r| r.imported).sum();
                let total_unknown: usize = results.iter().map(|r| r.skipped_unknown_qname).sum();
                let total_parse: usize = results.iter().map(|r| r.skipped_parse_error).sum();
                serde_json::to_string(&serde_json::json!({
                    "in_dir": in_dir.display().to_string(),
                    "files": results.iter().map(|r| serde_json::json!({
                        "class": r.class,
                        "file": r.file,
                        "imported": r.imported,
                        "skipped_unknown_qname": r.skipped_unknown_qname,
                        "skipped_parse_error": r.skipped_parse_error,
                    })).collect::<Vec<_>>(),
                    "total_imported": total_imported,
                    "total_skipped_unknown_qname": total_unknown,
                    "total_skipped_parse_error": total_parse,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => err_json(&format!("conclusions import failed: {e}")),
        }
    }

    #[tool(
        description = "Plan C t-004: classify test-tier symbols matching a query into migration actions (Delete / Gate / Run / KeepAsCovered / Review) based on their role-tagged ledger entries. Replaces a raw symbol list with a structured action plan. (CLI: `asd recipe classify-test-migration`.)"
    )]
    async fn recipe_classify_test_migration(
        &self,
        params: Parameters<RecipeClassifyTestMigrationParams>,
    ) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;
        let index = AsgIndexStore::from_engine(&engine);

        // Resolve candidate test-tier symbols via FTS.
        let fts = SearchFtsDb::open(&db_path).ok();
        let candidate_qnames: Vec<String> = if let Some(fts) = fts {
            let filters = FtsFilters {
                kind: None,
                language: None,
                include_tests: true,
                tests_only: true,
                exclude_terms: vec![],
                paths_filter: vec![],
                exclude_paths: vec![],
                exclude_languages: vec![],            };
            fts.search(&p.query, &filters, p.limit as usize)
                .unwrap_or_default()
                .into_iter()
                .map(|h| h.qname)
                .collect()
        } else {
            Vec::new()
        };

        let recipe = recipes::classify_test_migration(&engine, &index, &candidate_qnames, &p.query);
        serde_json::to_string(&recipe).unwrap_or_else(|_| "{}".to_string())
    }

    // -- Plan G t-003: agent-thinking handlers -----------------------------

    #[tool(
        description = "Plan G t-003: record a Hypothesis (speculation with confidence in [0.0, 1.0]). Below 0.3 is excluded from prepare-change/context-for prior_thinking by default. Idempotent — same (qname, summary) re-records over the previous entry. (CLI: `asd think speculate`.)"
    )]
    async fn think_speculate(&self, params: Parameters<ThinkSpeculateParams>) -> String {
        let p = params.0;
        if !(0.0..=1.0).contains(&p.confidence) {
            return err_json(&format!("confidence must be in [0.0, 1.0]; got {}", p.confidence));
        }
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let sym = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ledger = AsgLedgerStore::from_engine(&engine);
        let mut entry = LedgerEntry::new(
            &sym.symbol_id,
            LedgerKind::Hypothesis,
            &p.summary,
            Author { kind: AuthorKind::Agent, id: "asd-mcp".into() },
        );
        entry.entry_id = think_det_id("hypothesis", &p.qname, &p.summary);
        entry.confidence = Some(p.confidence);
        entry.body = p.body;
        think_push_provenance_tags(&self.db_path(), &mut entry.tags);
        match ledger.append_entry(&ref_name, &entry, "asd-mcp") {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true, "kind": "hypothesis", "qname": p.qname,
                "confidence": p.confidence, "entry_id": entry.entry_id,
            })).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Plan G t-003: record a MentalModel (multi-symbol structural understanding). Anchored on the FIRST symbol in `symbols`. Body carries the full symbols[] list. Idempotent by (name, summary). (CLI: `asd think model`.)"
    )]
    async fn think_model(&self, params: Parameters<ThinkModelParams>) -> String {
        let p = params.0;
        let symbols: Vec<String> = p
            .symbols
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if symbols.is_empty() {
            return err_json("symbols must list at least one qname (comma-separated)");
        }
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let sym = match index.get_symbol_by_qname(&ref_name, &symbols[0]) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", symbols[0])),
            Err(e) => return err_json(&e.to_string()),
        };
        let ledger = AsgLedgerStore::from_engine(&engine);
        let body = serde_json::json!({ "symbols": &symbols, "name": &p.name }).to_string();
        let mut entry = LedgerEntry::new(
            &sym.symbol_id,
            LedgerKind::MentalModel,
            format!("{}: {}", p.name, p.summary),
            Author { kind: AuthorKind::Agent, id: "asd-mcp".into() },
        );
        entry.entry_id = think_det_id("model", &p.name, &p.summary);
        entry.body = Some(body);
        think_push_provenance_tags(&self.db_path(), &mut entry.tags);
        match ledger.append_entry(&ref_name, &entry, "asd-mcp") {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true, "kind": "mental_model", "name": p.name,
                "symbols": symbols, "entry_id": entry.entry_id,
            })).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Plan G t-003: record a FailedAttempt (negative evidence — what was tried + why it didn't work). Saves the next session from re-treading. Idempotent by (qname, tried). (CLI: `asd think failed`.)"
    )]
    async fn think_failed(&self, params: Parameters<ThinkFailedParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let sym = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ledger = AsgLedgerStore::from_engine(&engine);
        let body = serde_json::json!({ "tried": &p.tried, "because": &p.because }).to_string();
        let mut entry = LedgerEntry::new(
            &sym.symbol_id,
            LedgerKind::FailedAttempt,
            format!("failed: {} — because {}", p.tried, p.because),
            Author { kind: AuthorKind::Agent, id: "asd-mcp".into() },
        );
        entry.entry_id = think_det_id("failed", &p.qname, &p.tried);
        entry.body = Some(body);
        think_push_provenance_tags(&self.db_path(), &mut entry.tags);
        match ledger.append_entry(&ref_name, &entry, "asd-mcp") {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true, "kind": "failed_attempt", "qname": p.qname,
                "entry_id": entry.entry_id,
            })).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Plan G t-003: record an OpenQuestion (known unknown blocking confident action). Be generous — every question recorded is one the next session doesn't have to re-ask. Idempotent by (qname, question). (CLI: `asd think question`.)"
    )]
    async fn think_question(&self, params: Parameters<ThinkQuestionParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let sym = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ledger = AsgLedgerStore::from_engine(&engine);
        let mut entry = LedgerEntry::new(
            &sym.symbol_id,
            LedgerKind::OpenQuestion,
            &p.question,
            Author { kind: AuthorKind::Agent, id: "asd-mcp".into() },
        );
        entry.entry_id = think_det_id("question", &p.qname, &p.question);
        think_push_provenance_tags(&self.db_path(), &mut entry.tags);
        match ledger.append_entry(&ref_name, &entry, "asd-mcp") {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true, "kind": "open_question", "qname": p.qname,
                "entry_id": entry.entry_id,
            })).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Plan G t-003: list captured thinking entries (Hypothesis/MentalModel/FailedAttempt/OpenQuestion). Optional kind filter: hypothesis | mental_model | failed_attempt | open_question. (CLI: `asd think list`.)"
    )]
    async fn think_list(&self, params: Parameters<ThinkListParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let ledger = AsgLedgerStore::from_engine(&engine);

        let kind_filter: Option<LedgerKind> = p.kind.as_deref().and_then(|s| match s {
            "hypothesis" => Some(LedgerKind::Hypothesis),
            "mental_model" => Some(LedgerKind::MentalModel),
            "failed_attempt" => Some(LedgerKind::FailedAttempt),
            "open_question" => Some(LedgerKind::OpenQuestion),
            _ => None,
        });
        if p.kind.is_some() && kind_filter.is_none() {
            return err_json(
                "unknown kind; valid: hypothesis, mental_model, failed_attempt, open_question",
            );
        }

        let symbol_ids: Vec<(String, String)> = if let Some(qname) = p.symbol.as_deref() {
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => vec![(s.symbol_id, s.qname)],
                _ => return err_json(&format!("symbol not found: {qname}")),
            }
        } else {
            let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
            let tree = engine
                .repo
                .get_tree(&ref_name, &prefix)
                .unwrap_or(serde_json::Value::Null);
            let qnames: Vec<String> = match tree {
                serde_json::Value::Object(m) => m.keys().cloned().collect(),
                _ => Vec::new(),
            };
            let mut out = Vec::new();
            for qn in qnames {
                if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, &qn) {
                    out.push((s.symbol_id, s.qname));
                }
            }
            out
        };

        let mut entries = Vec::new();
        for (sid, qname) in &symbol_ids {
            let les = ledger.list_entries(&ref_name, sid).unwrap_or_default();
            for entry in les {
                if entry.kind.conclusion_class() != ConclusionClass::Thinking {
                    continue;
                }
                if let Some(filter) = kind_filter {
                    if entry.kind != filter {
                        continue;
                    }
                }
                entries.push(serde_json::json!({
                    "entry_id": entry.entry_id,
                    "kind": entry.kind.as_str(),
                    "qname": qname,
                    "summary": entry.summary,
                    "confidence": entry.confidence,
                    "body": entry.body,
                    "tags": entry.tags,
                    "created_at": entry.created_at,
                }));
            }
        }

        serde_json::to_string(&serde_json::json!({
            "total": entries.len(),
            "kind_filter": p.kind,
            "symbol_filter": p.symbol,
            "entries": entries,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Plan F t-002: build a migration plan for stale test files. Returns the same shape as recipe_classify_test_migration but adds a Move action when a Mapping ledger entry carries a `move_to` path in its body. Otherwise falls back to the classify decision tree. (CLI: `asd recipe migrate-stale-tests`.)"
    )]
    async fn recipe_migrate_stale_tests(
        &self,
        params: Parameters<RecipeClassifyTestMigrationParams>,
    ) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;
        let index = AsgIndexStore::from_engine(&engine);

        let fts = SearchFtsDb::open(&db_path).ok();
        let candidate_qnames: Vec<String> = if let Some(fts) = fts {
            let filters = FtsFilters {
                kind: None,
                language: None,
                include_tests: true,
                tests_only: true,
                exclude_terms: vec![],
                paths_filter: vec![],
                exclude_paths: vec![],
                exclude_languages: vec![],            };
            fts.search(&p.query, &filters, p.limit as usize)
                .unwrap_or_default()
                .into_iter()
                .map(|h| h.qname)
                .collect()
        } else {
            Vec::new()
        };

        let recipe = recipes::migrate_stale_tests(&engine, &index, &candidate_qnames, &p.query);
        serde_json::to_string(&recipe).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Exact-symbol references with rg parity. Returns the canonical definition(s) from the ASD index plus every literal text occurrence in the worktree via `rg --fixed-strings --word-regexp`. Use this when you want complete, predictable matches for a concrete identifier (no tokenization, no BM25). Requires `rg` on PATH. (CLI: `asd references`.)"
    )]
    async fn references(&self, params: Parameters<ReferencesParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);

        // Definition lookup — qname-exact if `.` present, else basename-suffix match.
        let definitions: Vec<Symbol> = if p.name.contains('.') {
            match index.get_symbol_by_qname(&ref_name, &p.name) {
                Ok(Some(s)) => vec![s],
                Ok(None) => Vec::new(),
                Err(e) => return err_json(&e.to_string()),
            }
        } else {
            let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
            let qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
                Ok(serde_json::Value::Object(m)) => m.keys().cloned().collect(),
                _ => Vec::new(),
            };
            let needle_dot = format!(".{}", p.name);
            let mut out = Vec::new();
            for qn in qnames {
                if qn == p.name || qn.ends_with(&needle_dot) {
                    if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, &qn) {
                        out.push(s);
                    }
                }
            }
            out.sort_by(|a, b| a.qname.cmp(&b.qname));
            out
        };

        // Drop engine lock before shelling out — rg can take a moment on large trees.
        drop(engine);

        let root = p
            .path
            .clone()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let globs: Vec<String> = p
            .globs
            .as_deref()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let limit = p.limit as usize;

        let (occurrences, scan_status) = match rg_scan(&root, &p.name, &globs, limit) {
            Ok(occ) => (occ, "ok".to_string()),
            Err(e) => (Vec::new(), format!("error: {e}")),
        };

        let stale = stale_warning(&self.db_path(), 3600);
        // Plan D t-007: brief mode drops the confidence + scan metadata
        // and projects definitions through brief_symbol so each is the
        // compact 4-field shape.
        if brief::brief_from_env() {
            let brief_defs: Vec<serde_json::Value> =
                definitions.iter().map(brief::brief_symbol).collect();
            let payload = serde_json::json!({
                "name": p.name,
                "definitions": brief_defs,
                "occurrences": occurrences,
                "occurrence_count": occurrences.len(),
                "stale": stale,
                "query_id": brief::query_id("references", &[&p.name]),
            });
            return serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        }
        let payload = serde_json::json!({
            "name": p.name,
            "search_root": root.display().to_string(),
            "definitions": definitions,
            "occurrences": occurrences,
            "occurrence_count": occurrences.len(),
            "limit": p.limit,
            "scan": { "tool": "rg", "status": scan_status, "flags": ["--fixed-strings", "--word-regexp"] },
            "stale": stale,
            "confidence": {
                "strong": "exact-literal references for concrete identifiers (MasterBusParams, KSPatch.userFacingPresets); matches rg by construction",
                "weak": "conceptual or cross-layer queries (e.g. 'master volume strategy') — use `code_search` or `investigate` for those",
            },
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Feature archaeology in one pass: FTS5 search for entry points, then expand each with call chains, effects, invariants, and hazards. Use this at the start of any broad investigation — 'playhead over clips', 'auth flow', 'export pipeline', etc."
    )]
    async fn investigate(&self, params: Parameters<InvestigateParams>) -> String {
        let p = params.0;
        let intent = p.intent.as_deref().and_then(parse_intent).unwrap_or("");
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (tokens, mut exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
            {
                exclusions.push(term);
            }
        }

        if tokens.is_empty() {
            return serde_json::json!({ "query": p.query, "entry_points": [] }).to_string();
        }

        let depth = p.depth.max(1) as usize;
        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref scope) = p.scope {
            paths_filter.extend(resolve_scope(scope, &db_path));
        }
        if let Some(ref paths) = p.paths {
            paths_filter.extend(
                paths
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            tests_only: false,
            exclude_terms: exclusions,
            paths_filter,
            exclude_paths: Vec::new(),
            exclude_languages: Vec::new(),        };

        let index = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);

        let mut top_qnames = find_candidates(
            &engine,
            &p.query,
            &tokens,
            &filters,
            &ledger_store,
            &index,
            depth,
        );

        // Apply durable feedback adjustments.
        {
            use agentstatedeveloper_core::{FeedbackStore, apply_feedback_adjustments};
            let fb_store = AsgFeedbackStore::from_engine(&engine);
            let fb = fb_store.flat_verdicts(&ref_name).unwrap_or_default();
            apply_feedback_adjustments(&engine, &index, &p.query, &mut top_qnames, &fb);
        }

        // Build id_map for call graph resolution.
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let all_qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => vec![],
        };
        let mut id_map: std::collections::HashMap<String, agentstatedeveloper_core::Symbol> =
            std::collections::HashMap::new();
        for qn in &all_qnames {
            if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, qn) {
                id_map.insert(s.symbol_id.clone(), s);
            }
        }

        let recency = gather_recency(200, 14.0);

        let resolve_ids = |ids: Vec<String>| -> Vec<serde_json::Value> {
            ids.iter().map(|id| {
                if let Some(s) = id_map.get(id) {
                    serde_json::json!({ "qname": s.qname, "file": s.file, "line": s.start.line })
                } else {
                    serde_json::json!({ "symbol_id": id })
                }
            }).collect()
        };

        let mut entry_points: Vec<serde_json::Value> = Vec::new();
        for (score, qname) in &top_qnames {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            let callee_ids = index
                .get_callees(&ref_name, &sym.symbol_id)
                .unwrap_or_default();
            let caller_ids = index
                .get_callers(&ref_name, &sym.symbol_id)
                .unwrap_or_default();
            let effects = effect_store
                .get_effects(&ref_name, &sym.symbol_id)
                .unwrap_or(None);
            let ledger = ledger_store
                .list_entries(&ref_name, &sym.symbol_id)
                .unwrap_or_default();

            let mut invariants: Vec<serde_json::Value> = Vec::new();
            let mut hazards: Vec<serde_json::Value> = Vec::new();
            let mut ownership: Vec<serde_json::Value> = Vec::new();
            let mut validation_scenarios: Vec<serde_json::Value> = Vec::new();
            let mut known_bugs: Vec<serde_json::Value> = Vec::new();
            let mut concepts: Vec<serde_json::Value> = Vec::new();
            let mut other_ledger: Vec<serde_json::Value> = Vec::new();
            for entry in &ledger {
                let v = serde_json::to_value(entry).unwrap_or_default();
                match entry.kind {
                    LedgerKind::Invariant => invariants.push(v),
                    LedgerKind::Hazard => hazards.push(v),
                    LedgerKind::Ownership => ownership.push(v),
                    LedgerKind::ValidationScenario => validation_scenarios.push(v),
                    LedgerKind::KnownBug => known_bugs.push(v),
                    LedgerKind::Concept => concepts.push(v),
                    _ => other_ledger.push(v),
                }
            }

            let tier = symbol_tier(&sym.file);
            let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
            let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
            let rec = recency.get(&sym.file);
            entry_points.push(serde_json::json!({
                "score": score,
                "layer": layer,
                "summary": summary,
                "last_touched_days": rec.and_then(|r| r.last_touched_days),
                "hot": rec.map(|r| r.hot).unwrap_or(false),
                "qname": sym.qname,
                "kind": format!("{:?}", sym.kind).to_lowercase(),
                "language": sym.language,
                "file": sym.file,
                "line": sym.start.line,
                "signature": sym.signature,
                "invariants": invariants,
                "hazards": hazards,
                "known_bugs": known_bugs,
                "concepts": concepts,
                "ownership": ownership,
                "validation_scenarios": validation_scenarios,
                "callers": resolve_ids(caller_ids),
                "callees": resolve_ids(callee_ids),
                "effects": effects,
                "notes": other_ledger,
            }));
        }

        // Aggregate invariants/hazards across all entry points.
        let mut all_invariants: Vec<serde_json::Value> = Vec::new();
        let mut all_hazards: Vec<serde_json::Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for ep in &entry_points {
            let qname = ep.get("qname").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(invs) = ep.get("invariants").and_then(|v| v.as_array()) {
                for inv in invs {
                    let key = inv
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !key.is_empty() && seen.insert(key) {
                        let mut v = inv.clone();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "source_qname".to_string(),
                                serde_json::Value::String(qname.to_string()),
                            );
                        }
                        all_invariants.push(v);
                    }
                }
            }
            if let Some(hzs) = ep.get("hazards").and_then(|v| v.as_array()) {
                for hz in hzs {
                    let mut v = hz.clone();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "source_qname".to_string(),
                            serde_json::Value::String(qname.to_string()),
                        );
                    }
                    all_hazards.push(v);
                }
            }
        }

        // Group by layer (intent-aware ordering).
        let layer_order = intent_layer_order(intent);
        let mut by_layer = serde_json::Map::new();
        for lk in layer_order {
            let members: Vec<&serde_json::Value> = entry_points
                .iter()
                .filter(|ep| ep.get("layer").and_then(|v| v.as_str()) == Some(*lk))
                .collect();
            if !members.is_empty() {
                by_layer.insert(
                    lk.to_string(),
                    serde_json::Value::Array(members.into_iter().cloned().collect()),
                );
            }
        }

        let focus = intent_focus(intent);
        let layers_present: std::collections::HashSet<&str> = entry_points
            .iter()
            .filter_map(|ep| ep.get("layer").and_then(serde_json::Value::as_str))
            .collect();
        let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
        let possible_misses = detect_possible_misses(&p.query, &layers_present, entry_points.len());
        // Token economy (1.0.80): drop `query` input echo. Other
        // fields use `intent`/`focus` from input but those are
        // resolved-and-canonicalized derivatives — keep them.
        let full = serde_json::json!({
            "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
            "focus": if focus.is_empty() { serde_json::Value::Null } else { serde_json::json!(focus) },
            "tokens": tokens,
            "ambiguous_terms": ambiguous_terms,
            "possible_misses": possible_misses,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "by_layer": by_layer,
        });
        // Plan F t-006: brief flattens by_layer to a compact entry_points list.
        let out = if brief::brief_from_env() {
            brief::brief_investigate(&full)
        } else {
            full
        };
        let out = agentstatedeveloper_core::drop_empty_top_level(out);
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Read a symbol by qname. Returns { symbol, effects, ledger } — full context needed to reason about the code unit. (CLI: `asd read`.)"
    )]
    async fn code_read(&self, params: Parameters<CodeReadParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore::from_engine(&engine);
        let effects = match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let ledger = match ledger_store.list_entries(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        // Plan D t-007: honor ASD_FORMAT=brief at the per-call site.
        if brief::brief_from_env() {
            let effects_json = effects.as_ref().and_then(|e| serde_json::to_value(e).ok());
            let mut out = brief::brief_read(
                &symbol,
                &[], // code_read doesn't compute callers/callees inline
                &[],
                effects_json.as_ref(),
                ledger.len(),
            );
            if let serde_json::Value::Object(ref mut m) = out {
                m.insert(
                    "query_id".into(),
                    serde_json::Value::String(brief::query_id("code_read", &[&p.qname])),
                );
            }
            return serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string());
        }

        let payload = serde_json::json!({
            "symbol": symbol,
            "effects": effects,
            "ledger": ledger,
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(description = "Return declared + transitive effects for a symbol (resolved via qname).")]
    async fn effects(&self, params: Parameters<EffectsOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore::from_engine(&engine);
        match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(Some(decl)) => serde_json::to_string(&decl).unwrap_or_else(|_| "null".to_string()),
            Ok(None) => "null".to_string(),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List symbols that call the given symbol (inbound call edges, intra-module)."
    )]
    async fn callers(&self, params: Parameters<CallersOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let target = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ids = match index.get_callers(&ref_name, &target.symbol_id) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        let syms = match resolve_symbols_by_ids(&engine, &ids) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        // Plan E t-007: brief = compact per-symbol projection.
        if brief::brief_from_env() {
            let projected: Vec<_> = syms.iter().map(brief::brief_symbol).collect();
            return serde_json::to_string(&projected).unwrap_or_else(|_| "[]".to_string());
        }
        serde_json::to_string(&syms).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List symbols called by the given symbol (outbound call edges, intra-module)."
    )]
    async fn callees(&self, params: Parameters<CalleesOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let target = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ids = match index.get_callees(&ref_name, &target.symbol_id) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        let syms = match resolve_symbols_by_ids(&engine, &ids) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        // Plan E t-007: brief = compact per-symbol projection.
        if brief::brief_from_env() {
            let projected: Vec<_> = syms.iter().map(brief::brief_symbol).collect();
            return serde_json::to_string(&projected).unwrap_or_else(|_| "[]".to_string());
        }
        serde_json::to_string(&syms).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List ledger entries for a symbol, newest first. By default, entries superseded by later entries are omitted; set include_superseded=true to include them."
    )]
    async fn ledger_get(&self, params: Parameters<LedgerGetParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let entries = match ledger_store.list_entries(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let filtered: Vec<&LedgerEntry> = if p.include_superseded {
            entries.iter().collect()
        } else {
            // Collect all superseded ids, then exclude entries whose id appears in any supersedes list.
            let mut superseded: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for e in &entries {
                for sid in &e.supersedes {
                    superseded.insert(sid.clone());
                }
            }
            entries
                .iter()
                .filter(|e| !superseded.contains(&e.entry_id))
                .collect()
        };

        serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Search ledger entries across all symbols. Filters (all optional): kind, tag, author_id. O(n) scan — v1 simplicity."
    )]
    async fn ledger_find(&self, params: Parameters<LedgerFindParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/ledger", ASD_PATH_PREFIX);
        let limit = p.limit.max(1) as usize;

        let kind_filter = match p.kind.as_deref() {
            Some(s) => match parse_ledger_kind(s) {
                Ok(k) => Some(k),
                Err(e) => return err_json(&e),
            },
            None => None,
        };

        let tree = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(v) => v,
            Err(_) => return "[]".to_string(),
        };

        let mut matches: Vec<LedgerEntry> = Vec::new();
        if let serde_json::Value::Object(by_symbol) = tree {
            for (_sym_id, per_symbol) in by_symbol {
                let entries_map = match per_symbol {
                    serde_json::Value::Object(m) => m,
                    _ => continue,
                };
                for (_entry_id, v) in entries_map {
                    if let Ok(entry) = serde_json::from_value::<LedgerEntry>(v) {
                        if let Some(k) = kind_filter
                            && entry.kind != k
                        {
                            continue;
                        }
                        if let Some(ref t) = p.tag
                            && !entry.tags.iter().any(|x| x == t)
                        {
                            continue;
                        }
                        if let Some(ref a) = p.author_id
                            && &entry.author.id != a
                        {
                            continue;
                        }
                        matches.push(entry);
                    }
                }
            }
        }

        matches.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        matches.truncate(limit);
        serde_json::to_string(&matches).unwrap_or_else(|_| "[]".to_string())
    }

    // -- Write tools --

    #[tool(
        description = "Append a ledger entry to a symbol (resolved via qname). Routes through the configured policy gate — may deny, allow, or flag the entry as awaiting-approval. Returns { entry_id, matched_policy, status }."
    )]
    async fn ledger_append(&self, params: Parameters<LedgerAppendParams>) -> String {
        let p = params.0;
        let kind = match parse_ledger_kind(&p.kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };
        let author_kind = match parse_author_kind(&p.author_kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };
        let author_kind_label = match author_kind {
            AuthorKind::Agent => "agent",
            AuthorKind::Human => "human",
        };

        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_APPEND,
                    &p.author_id,
                    author_kind_label,
                    "error",
                )
                .with_reason(format!("symbol not found: {}", p.qname))
                .with_payload(serde_json::json!({
                    "qname": &p.qname,
                    "kind": kind.as_str(),
                }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&format!("symbol not found: {}", p.qname));
            }
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_APPEND,
                    &p.author_id,
                    author_kind_label,
                    "error",
                )
                .with_reason(e.to_string())
                .with_payload(serde_json::json!({
                    "qname": &p.qname,
                    "kind": kind.as_str(),
                }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&e.to_string());
            }
        };

        // Evaluate policy before doing any write.
        let action = actions::ledger_append_action(kind.as_str());
        let situation = Situation {
            description: format!("ledger.append for {}", p.qname),
            qualifiers: serde_json::json!({
                "qname": &p.qname,
                "kind": kind.as_str(),
                "symbol_id": &symbol.symbol_id,
                "file": &symbol.file,
                "language": &symbol.language,
            }),
        };
        let decision = match engine.policy.evaluate(&situation, &action, &p.author_id) {
            Ok(d) => d,
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_APPEND,
                    &p.author_id,
                    author_kind_label,
                    "error",
                )
                .with_secondary(&symbol.symbol_id)
                .with_reason(format!("policy evaluation failed: {}", e))
                .with_payload(serde_json::json!({
                    "qname": &p.qname,
                    "kind": kind.as_str(),
                }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&format!("policy evaluation failed: {}", e));
            }
        };

        if let Decision::Deny {
            matched_policy,
            reason,
        } = &decision
        {
            let event = AuditEvent::new(
                event_types::LEDGER_APPEND,
                &p.author_id,
                author_kind_label,
                "denied",
            )
            .with_secondary(&symbol.symbol_id)
            .with_matched_policy(Some(matched_policy.clone()))
            .with_reason(reason.clone())
            .with_payload(serde_json::json!({
                "qname": &p.qname,
                "kind": kind.as_str(),
            }));
            emit_audit(engine.audit.as_ref(), event);
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }

        let author = Author {
            kind: author_kind,
            id: p.author_id.clone(),
        };
        let mut entry = LedgerEntry::new(symbol.symbol_id.clone(), kind, p.summary, author);
        entry.body = p.body;
        if let Some(tags) = p.tags {
            entry.tags = tags;
        }
        entry.matched_policy = decision.matched_policy();
        // Plan C t-002: warn on unknown role (stderr — visible to the
        // MCP host's logs). Unknown tags are still stored; CLI/MCP just
        // signal the typo.
        if let Some(ref r) = p.role {
            if agentstatedeveloper_core::RoleTag::from_str(r).is_none() {
                let valid: Vec<&str> = agentstatedeveloper_core::RoleTag::all()
                    .iter()
                    .map(|t| t.as_str())
                    .collect();
                eprintln!(
                    "asd-mcp: warning: role={:?} is not a canonical RoleTag. Valid: {}",
                    r,
                    valid.join(", ")
                );
            }
        }
        entry.role = p.role;
        entry.command = p.command;

        // RequireApproval: tag the entry so downstream reviewers see it.
        if let Decision::RequireApproval {
            approvers, reason, ..
        } = &decision
        {
            entry.tags.push("awaiting-approval".to_string());
            for a in approvers {
                entry.tags.push(format!("approver:{}", a));
            }
            if let Some(r) = reason {
                if entry.body.is_none() {
                    entry.body = Some(format!("Approval reason: {}", r));
                }
            }
        }

        let ledger_store = AsgLedgerStore::from_engine(&engine);
        if let Err(e) = ledger_store.append_entry(&ref_name, &entry, &p.author_id) {
            let event = AuditEvent::new(
                event_types::LEDGER_APPEND,
                &p.author_id,
                author_kind_label,
                "error",
            )
            .with_subject(&entry.entry_id)
            .with_secondary(&symbol.symbol_id)
            .with_matched_policy(entry.matched_policy.clone())
            .with_reason(e.to_string())
            .with_payload(serde_json::json!({
                "qname": &p.qname,
                "kind": kind.as_str(),
            }));
            emit_audit(engine.audit.as_ref(), event);
            return err_json(&e.to_string());
        }

        let status = match &decision {
            Decision::Allow { .. } => "allowed",
            Decision::RequireApproval { .. } => "awaiting-approval",
            Decision::Deny { .. } => "denied",
            Decision::NoPolicyMatch => "no-policy-match",
        };

        let event = AuditEvent::new(
            event_types::LEDGER_APPEND,
            &p.author_id,
            author_kind_label,
            status,
        )
        .with_subject(&entry.entry_id)
        .with_secondary(&entry.symbol_id)
        .with_matched_policy(entry.matched_policy.clone())
        .with_payload(serde_json::json!({
            "qname": &p.qname,
            "kind": kind.as_str(),
            "tags": &entry.tags,
        }));
        emit_audit(engine.audit.as_ref(), event);

        serde_json::to_string(&serde_json::json!({
            "entry_id": entry.entry_id,
            "symbol_id": symbol.symbol_id,
            "matched_policy": entry.matched_policy,
            "status": status,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Approve a ledger entry currently tagged `awaiting-approval`. Flips tags to `approved` + records `approved-by:<approver>` and `approved-at:<timestamp>`. Scans ledger by entry_id — no qname needed. Enforces that the approver kind/id matches one of the original entry's `approver:*` tags."
    )]
    async fn ledger_approve(&self, params: Parameters<LedgerApproveParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let situation = Situation {
            description: format!("ledger.approve {}", p.entry_id),
            qualifiers: serde_json::json!({ "entry_id": &p.entry_id }),
        };
        if let Ok(Decision::Deny {
            matched_policy,
            reason,
        }) = engine
            .policy
            .evaluate(&situation, actions::LEDGER_APPROVE, &p.approver)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        match ledger_store.approve_entry(
            &ref_name,
            &p.entry_id,
            &p.approver,
            &p.approver_kind,
            p.message.as_deref(),
            "asd-mcp",
        ) {
            Ok(outcome) => {
                let status = if outcome.already_approved {
                    "already-approved"
                } else {
                    "approved"
                };
                let event = AuditEvent::new(
                    event_types::LEDGER_APPROVE,
                    &p.approver,
                    &p.approver_kind,
                    status,
                )
                .with_subject(&outcome.entry.entry_id)
                .with_secondary(&outcome.entry.symbol_id)
                .with_matched_policy(outcome.entry.matched_policy.clone())
                .with_payload(serde_json::json!({ "tags": outcome.entry.tags }));
                emit_audit(engine.audit.as_ref(), event);

                serde_json::to_string(&serde_json::json!({
                    "status": status,
                    "entry": outcome.entry,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_APPROVE,
                    &p.approver,
                    &p.approver_kind,
                    "error",
                )
                .with_subject(&p.entry_id)
                .with_reason(e.to_string());
                emit_audit(engine.audit.as_ref(), event);
                err_json(&e.to_string())
            }
        }
    }

    #[tool(
        description = "Reject an awaiting-approval ledger entry. Flips tags to `rejected` + records `rejected-by:<reviewer>` and `rejected-at:<timestamp>`. `reason` is required and appended to the entry body. Same approver-match rule as `ledger_approve`."
    )]
    async fn ledger_reject(&self, params: Parameters<LedgerRejectParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let situation = Situation {
            description: format!("ledger.reject {}", p.entry_id),
            qualifiers: serde_json::json!({ "entry_id": &p.entry_id }),
        };
        if let Ok(Decision::Deny {
            matched_policy,
            reason,
        }) = engine
            .policy
            .evaluate(&situation, actions::LEDGER_REJECT, &p.reviewer)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        match ledger_store.reject_entry(
            &ref_name,
            &p.entry_id,
            &p.reviewer,
            &p.reviewer_kind,
            &p.reason,
            "asd-mcp",
        ) {
            Ok(outcome) => {
                let status = if outcome.already_resolved {
                    "already-rejected"
                } else {
                    "rejected"
                };
                let event = AuditEvent::new(
                    event_types::LEDGER_REJECT,
                    &p.reviewer,
                    &p.reviewer_kind,
                    status,
                )
                .with_subject(&outcome.entry.entry_id)
                .with_secondary(&outcome.entry.symbol_id)
                .with_matched_policy(outcome.entry.matched_policy.clone())
                .with_reason(&p.reason)
                .with_payload(serde_json::json!({ "tags": outcome.entry.tags }));
                emit_audit(engine.audit.as_ref(), event);

                serde_json::to_string(&serde_json::json!({
                    "status": status,
                    "entry": outcome.entry,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_REJECT,
                    &p.reviewer,
                    &p.reviewer_kind,
                    "error",
                )
                .with_subject(&p.entry_id)
                .with_reason(e.to_string());
                emit_audit(engine.audit.as_ref(), event);
                err_json(&e.to_string())
            }
        }
    }

    #[tool(
        description = "Withdraw an awaiting-approval entry. Must be called by the original author (author_id matching the entry's author.id). Flips `awaiting-approval` → `withdrawn`."
    )]
    async fn ledger_withdraw(&self, params: Parameters<LedgerWithdrawParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let situation = Situation {
            description: format!("ledger.withdraw {}", p.entry_id),
            qualifiers: serde_json::json!({ "entry_id": &p.entry_id }),
        };
        if let Ok(Decision::Deny {
            matched_policy,
            reason,
        }) = engine
            .policy
            .evaluate(&situation, actions::LEDGER_WITHDRAW, &p.author_id)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        match ledger_store.withdraw_entry(&ref_name, &p.entry_id, &p.author_id, "asd-mcp") {
            Ok(outcome) => {
                let status = if outcome.already_resolved {
                    "already-withdrawn"
                } else {
                    "withdrawn"
                };
                let event =
                    AuditEvent::new(event_types::LEDGER_WITHDRAW, &p.author_id, "agent", status)
                        .with_subject(&outcome.entry.entry_id)
                        .with_secondary(&outcome.entry.symbol_id)
                        .with_payload(serde_json::json!({ "tags": outcome.entry.tags }));
                emit_audit(engine.audit.as_ref(), event);

                serde_json::to_string(&serde_json::json!({
                    "status": status,
                    "entry": outcome.entry,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let event =
                    AuditEvent::new(event_types::LEDGER_WITHDRAW, &p.author_id, "agent", "error")
                        .with_subject(&p.entry_id)
                        .with_reason(e.to_string());
                emit_audit(engine.audit.as_ref(), event);
                err_json(&e.to_string())
            }
        }
    }

    #[tool(
        description = "Append a new ledger entry that supersedes one or more existing entries for the given symbol. Non-superseded entries remain; superseded ones are filtered out of default `ledger_get` results."
    )]
    async fn ledger_supersede(&self, params: Parameters<LedgerSupersedeParams>) -> String {
        let p = params.0;
        let kind = match parse_ledger_kind(&p.kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };
        let author_kind = match parse_author_kind(&p.author_kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };
        let author_kind_label = match author_kind {
            AuthorKind::Agent => "agent",
            AuthorKind::Human => "human",
        };

        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_SUPERSEDE,
                    &p.author_id,
                    author_kind_label,
                    "error",
                )
                .with_reason(format!("symbol not found: {}", p.qname))
                .with_payload(serde_json::json!({
                    "qname": &p.qname,
                    "supersedes": &p.supersedes,
                }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&format!("symbol not found: {}", p.qname));
            }
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_SUPERSEDE,
                    &p.author_id,
                    author_kind_label,
                    "error",
                )
                .with_reason(e.to_string())
                .with_payload(serde_json::json!({ "qname": &p.qname }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&e.to_string());
            }
        };

        let situation = Situation {
            description: format!("ledger.supersede for {}", p.qname),
            qualifiers: serde_json::json!({
                "qname": &p.qname,
                "symbol_id": &symbol.symbol_id,
                "file": &symbol.file,
                "language": &symbol.language,
            }),
        };
        if let Ok(Decision::Deny {
            matched_policy,
            reason,
        }) = engine
            .policy
            .evaluate(&situation, actions::LEDGER_SUPERSEDE, &p.author_id)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }

        let author = Author {
            kind: author_kind,
            id: p.author_id.clone(),
        };
        let mut entry = LedgerEntry::new(&symbol.symbol_id, kind, p.summary, author);
        entry.body = p.body;
        entry.supersedes = p.supersedes.clone();
        entry.tags.push("supersedes".to_string());

        let ledger_store = AsgLedgerStore::from_engine(&engine);
        match ledger_store.append_entry(&ref_name, &entry, "asd-mcp") {
            Ok(()) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_SUPERSEDE,
                    &p.author_id,
                    author_kind_label,
                    "success",
                )
                .with_subject(&entry.entry_id)
                .with_secondary(&entry.symbol_id)
                .with_payload(serde_json::json!({
                    "supersedes": entry.supersedes,
                    "qname": &p.qname,
                }));
                emit_audit(engine.audit.as_ref(), event);

                serde_json::to_string(&serde_json::json!({
                    "status": "superseded",
                    "entry_id": entry.entry_id,
                    "symbol_id": entry.symbol_id,
                    "supersedes": entry.supersedes,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::LEDGER_SUPERSEDE,
                    &p.author_id,
                    author_kind_label,
                    "error",
                )
                .with_secondary(&symbol.symbol_id)
                .with_reason(e.to_string())
                .with_payload(serde_json::json!({
                    "supersedes": &p.supersedes,
                    "qname": &p.qname,
                }));
                emit_audit(engine.audit.as_ref(), event);
                err_json(&e.to_string())
            }
        }
    }

    #[tool(
        description = "Overwrite the `declared` effects list for a symbol. Routes through the configured policy gate. Uses `asd.effect.declare.broadens` as the action when the new list introduces effect categories not already present; otherwise `asd.effect.declare`. Returns the updated EffectDecl plus a `status` string."
    )]
    async fn effect_declare(&self, params: Parameters<EffectDeclareParams>) -> String {
        let p = params.0;

        // Deserialize each declared element into an Effect.
        let mut declared: Vec<Effect> = Vec::with_capacity(p.declared.len());
        for (i, v) in p.declared.into_iter().enumerate() {
            match serde_json::from_value::<Effect>(v) {
                Ok(e) => declared.push(e),
                Err(e) => return err_json(&format!("declared[{}]: {}", i, e)),
            }
        }

        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let event =
                    AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", "error")
                        .with_reason(format!("symbol not found: {}", p.qname))
                        .with_payload(serde_json::json!({ "qname": &p.qname }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&format!("symbol not found: {}", p.qname));
            }
            Err(e) => {
                let event =
                    AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", "error")
                        .with_reason(e.to_string())
                        .with_payload(serde_json::json!({ "qname": &p.qname }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&e.to_string());
            }
        };

        let effects_store = AsgEffectStore::from_engine(&engine);
        let existing = match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => {
                let event =
                    AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", "error")
                        .with_subject(&symbol.symbol_id)
                        .with_reason(e.to_string())
                        .with_payload(serde_json::json!({ "qname": &p.qname }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&e.to_string());
            }
        };

        // Broadening check: if any new effect category is not already present
        // in the existing declared list, this call is broadening.
        let existing_set: std::collections::HashSet<EffectCategory> = existing
            .as_ref()
            .map(|d| d.declared.iter().map(|e| e.effect.clone()).collect())
            .unwrap_or_default();
        let new_categories: Vec<String> = declared
            .iter()
            .map(|e| e.effect.as_str().to_string())
            .collect();
        let broadens = declared.iter().any(|e| !existing_set.contains(&e.effect));
        let action = if broadens {
            actions::EFFECT_DECLARE_BROADENS
        } else {
            actions::EFFECT_DECLARE
        };

        let situation = Situation {
            description: format!("effect.declare for {}", p.qname),
            qualifiers: serde_json::json!({
                "qname": &p.qname,
                "declared": new_categories,
                "broadens": broadens,
                "symbol_id": &symbol.symbol_id,
                "file": &symbol.file,
                "language": &symbol.language,
            }),
        };
        let decision = match engine.policy.evaluate(&situation, action, &p.author_id) {
            Ok(d) => d,
            Err(e) => {
                let event =
                    AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", "error")
                        .with_subject(&symbol.symbol_id)
                        .with_reason(format!("policy evaluation failed: {}", e))
                        .with_payload(serde_json::json!({
                            "qname": &p.qname,
                            "declared": &new_categories,
                            "broadens": broadens,
                            "action": action,
                        }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&format!("policy evaluation failed: {}", e));
            }
        };

        if let Decision::Deny {
            matched_policy,
            reason,
        } = &decision
        {
            let event =
                AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", "denied")
                    .with_subject(&symbol.symbol_id)
                    .with_matched_policy(Some(matched_policy.clone()))
                    .with_reason(reason.clone())
                    .with_payload(serde_json::json!({
                        "qname": &p.qname,
                        "declared": &new_categories,
                        "broadens": broadens,
                        "action": action,
                    }));
            emit_audit(engine.audit.as_ref(), event);
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }

        let matched_policy = decision.matched_policy();

        let updated = EffectDecl {
            symbol_id: symbol.symbol_id.clone(),
            declared,
            transitive: existing
                .as_ref()
                .map(|d| d.transitive.clone())
                .unwrap_or_default(),
            verification: existing.as_ref().and_then(|d| d.verification.clone()),
            confidence: existing.as_ref().and_then(|d| d.confidence),
            matched_policy: matched_policy.clone(),
        };

        if let Err(e) =
            effects_store.put_effects(&ref_name, &symbol.symbol_id, &updated, &p.author_id)
        {
            let event =
                AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", "error")
                    .with_subject(&symbol.symbol_id)
                    .with_matched_policy(matched_policy.clone())
                    .with_reason(e.to_string())
                    .with_payload(serde_json::json!({
                        "qname": &p.qname,
                        "declared": &new_categories,
                        "broadens": broadens,
                        "action": action,
                    }));
            emit_audit(engine.audit.as_ref(), event);
            return err_json(&e.to_string());
        }

        let status = match &decision {
            Decision::Allow { .. } => "allowed",
            Decision::RequireApproval { .. } => "awaiting-approval",
            Decision::Deny { .. } => "denied",
            Decision::NoPolicyMatch => "no-policy-match",
        };

        let event = AuditEvent::new(event_types::EFFECT_DECLARE, &p.author_id, "agent", status)
            .with_subject(&symbol.symbol_id)
            .with_matched_policy(matched_policy.clone())
            .with_payload(serde_json::json!({
                "qname": &p.qname,
                "declared": &new_categories,
                "broadens": broadens,
                "action": action,
            }));
        emit_audit(engine.audit.as_ref(), event);

        serde_json::to_string(&serde_json::json!({
            "effect_decl": updated,
            "matched_policy": matched_policy,
            "status": status,
            "action": action,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Return execution trace records stored for a symbol (written by `asd trace`). Returns newest-first, up to `limit` (default 20)."
    )]
    async fn traces(&self, params: Parameters<TracesOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore::from_engine(&engine);
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let prefix = format!("{}/traces/{}", paths::ASD_ROOT, symbol.symbol_id);
        let limit = p.limit.unwrap_or(20).min(200);
        let leaf_paths = match engine.repo.list_paths(&ref_name, &prefix, None) {
            Ok(v) => v,
            Err(_) => Vec::new(),
        };
        let mut traces: Vec<serde_json::Value> = Vec::new();
        for path in leaf_paths.iter().take(limit * 4) {
            if let Ok(v) = engine.repo.get_json(&ref_name, path) {
                if traces.len() < limit {
                    traces.push(v);
                }
            }
        }
        serde_json::to_string(&serde_json::json!({
            "symbol_id": symbol.symbol_id,
            "qname": p.qname,
            "count": traces.len(),
            "traces": traces,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Re-parse a source file or directory and refresh the ASD symbol index, effects, and call graph. Accepts absolute or relative paths. Equivalent to running `asd index <path>` from the CLI. After indexing, run `asd sync --prune` (or the pre-commit hook does it automatically) to flush the updated state into the .asd/v1/ sidecar so it travels with the next git commit."
    )]
    async fn reindex(&self, params: Parameters<ReindexParams>) -> String {
        let p = params.0;
        let root = std::path::PathBuf::from(&p.path);
        if !root.exists() {
            return err_json(&format!("path does not exist: {}", p.path));
        }
        let adapters = default_adapters();
        let engine = self.engine.lock().await;
        match agentstatedeveloper_core::run_index(
            &engine.repo,
            &engine.ref_name,
            &root,
            "asd-mcp",
            &adapters,
            Some(engine.audit.as_ref()),
            None,
            None,
            Some(&self.db_path()),
        ) {
            Ok(s) => serde_json::json!({
                "path": p.path,
                "files": s.files,
                "skipped": s.skipped,
                "symbols": s.symbols,
                "edges": s.edges,
                "intra_module_edges": s.intra_module_edges,
                "cross_module_edges": s.cross_module_edges,
                "transitive_updates": s.transitive_updates,
                "orphaned_tagged": s.orphaned_tagged,
            })
            .to_string(),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Record that a symbol was renamed or moved. Writes a rebind record so the old symbol_id maps to the new one, then re-parents all ledger entries from the old symbol_id to the new one. Use this whenever an agent or human renames a function, class, or method so its ledger history follows the rename."
    )]
    async fn ledger_rebind(&self, params: Parameters<LedgerRebindParams>) -> String {
        use agentstategraph::CommitOptions;
        use agentstategraph_core::IntentCategory;
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = &engine.ref_name;

        // Policy gate — evaluate before any writes
        let situation = Situation::new("rebind symbol")
            .with_qualifier("from_symbol_id", &p.from_symbol_id)
            .with_qualifier("to_qname", &p.to_qname);
        match engine
            .policy
            .evaluate(&situation, actions::LEDGER_REBIND, &p.agent_id)
        {
            Ok(Decision::Deny {
                matched_policy,
                reason,
            }) => {
                return serde_json::json!({
                    "policy_denied": true,
                    "matched_policy": matched_policy,
                    "reason": reason,
                })
                .to_string();
            }
            Ok(_) => {}
            Err(e) => return serde_json::json!({ "error": e.to_string() }).to_string(),
        }

        // Resolve new qname → Symbol via index
        let index_store = AsgIndexStore::from_engine(&engine);
        let new_symbol = match index_store.get_symbol_by_qname(ref_name, &p.to_qname) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return serde_json::json!({
                    "error": format!("symbol not found for qname '{}'", p.to_qname)
                })
                .to_string();
            }
            Err(e) => return serde_json::json!({ "error": e.to_string() }).to_string(),
        };

        // Write rebind record
        let rebind = Rebind {
            from_symbol_id: p.from_symbol_id.clone(),
            to_symbol_id: new_symbol.symbol_id.clone(),
            to_qname: new_symbol.qname.clone(),
            at: chrono::Utc::now(),
            by: p.agent_id.clone(),
        };
        let rebind_path = paths::rebind_path(&p.from_symbol_id);
        let rebind_val = match serde_json::to_value(&rebind) {
            Ok(v) => v,
            Err(e) => return serde_json::json!({ "error": e.to_string() }).to_string(),
        };
        if let Err(e) = engine.repo.set_json(
            ref_name,
            &rebind_path,
            &rebind_val,
            CommitOptions::new(
                &p.agent_id,
                IntentCategory::Refine,
                format!("rebind {} → {}", p.from_symbol_id, new_symbol.symbol_id),
            ),
        ) {
            return serde_json::json!({ "error": e.to_string() }).to_string();
        }

        // Re-parent ledger entries
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let entries = match ledger_store.list_entries_with_superseded(ref_name, &p.from_symbol_id) {
            Ok(v) => v,
            Err(e) => return serde_json::json!({ "error": e.to_string() }).to_string(),
        };
        let mut reparented = 0usize;
        for mut entry in entries {
            entry.symbol_id = new_symbol.symbol_id.clone();
            let new_path = paths::ledger_entry_path(&new_symbol.symbol_id, &entry.entry_id);
            let val = match serde_json::to_value(&entry) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if engine
                .repo
                .set_json(
                    ref_name,
                    &new_path,
                    &val,
                    CommitOptions::new(
                        &p.agent_id,
                        IntentCategory::Refine,
                        format!("reparent ledger entry {} after rebind", entry.entry_id),
                    ),
                )
                .is_ok()
            {
                let old_path = paths::ledger_entry_path(&p.from_symbol_id, &entry.entry_id);
                let _ = engine.repo.delete(
                    ref_name,
                    &old_path,
                    CommitOptions::new(
                        &p.agent_id,
                        IntentCategory::Refine,
                        format!("remove old ledger entry {} after rebind", entry.entry_id),
                    ),
                );
                reparented += 1;
            }
        }

        let audit_event =
            AuditEvent::new(event_types::LEDGER_REBIND, &p.agent_id, "agent", "allow")
                .with_subject(p.from_symbol_id.clone())
                .with_secondary(new_symbol.symbol_id.clone())
                .with_payload(serde_json::json!({
                    "from_symbol_id": p.from_symbol_id,
                    "to_symbol_id": new_symbol.symbol_id,
                    "to_qname": new_symbol.qname,
                    "entries_reparented": reparented,
                }));
        emit_audit(engine.audit.as_ref(), audit_event);

        serde_json::json!({
            "from_symbol_id": p.from_symbol_id,
            "to_symbol_id": new_symbol.symbol_id,
            "to_qname": new_symbol.qname,
            "entries_reparented": reparented,
        })
        .to_string()
    }

    #[tool(
        description = "Read back audit events from the configured JSONL log. Supports filtering by event_type substring, exact actor_id, exact outcome, and a `since` cursor. Returns `configured: false` when ASD_AUDIT_LOG was not set at server startup."
    )]
    async fn audit_tail(&self, params: Parameters<AuditTailParams>) -> String {
        let p = params.0;
        let Some(path) = self.audit_log_path.as_ref() else {
            return serde_json::to_string(&serde_json::json!({
                "configured": false,
                "count": 0,
                "events": [],
            }))
            .unwrap_or_else(|_| "{}".to_string());
        };
        let events = match agentstatedeveloper_core::read_jsonl(path) {
            Ok(v) => v,
            Err(e) => return err_json(&format!("read audit log: {}", e)),
        };

        let start_idx = match p.since {
            Some(ref id) => events
                .iter()
                .position(|e| &e.event_id == id)
                .map(|i| i + 1)
                .unwrap_or(0),
            None => 0,
        };

        let limit = p.limit.unwrap_or(200).min(1000) as usize;
        let filtered: Vec<&agentstatedeveloper_core::AuditEvent> = events[start_idx..]
            .iter()
            .filter(|e| {
                if let Some(ref t) = p.event_type {
                    if !e.event_type.contains(t) {
                        return false;
                    }
                }
                if let Some(ref a) = p.actor {
                    if &e.actor_id != a {
                        return false;
                    }
                }
                if let Some(ref o) = p.outcome {
                    if &e.outcome != o {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect();

        serde_json::to_string(&serde_json::json!({
            "configured": true,
            "path": path.display().to_string(),
            "count": filtered.len(),
            "events": filtered,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Verify the hash-chain integrity of the configured audit log. Commercial feature (Enterprise tier) — requires asd-pro."
    )]
    async fn audit_verify(&self) -> String {
        serde_json::to_string(&serde_json::json!({
            "configured": self.audit_log_path.is_some(),
            "verified": false,
            "error": "audit verify is a commercial feature (Enterprise tier) — install asd-pro",
            "upgrade_url": "https://agentstatedeveloper.dev/pricing",
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    // -- Scratch tools -------------------------------------------------------

    #[tool(
        description = "Write a new draft scratch entry. Scratch entries are local-only working notes scoped to a symbol/workflow/session. No policy gate — write freely. Returns { scratch_id, status }."
    )]
    async fn scratch_write(&self, params: Parameters<ScratchWriteParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let store = AsgScratchStore { repo: &engine.repo };

        let mut entry = ScratchEntry::new(&p.content, "asd-mcp");
        entry.workflow = p.workflow;
        entry.tags = p.tags.unwrap_or_default();

        if let Some(ttl_h) = p.ttl_hours {
            entry.expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(ttl_h));
        }

        // Resolve --symbol to symbol_id.
        if let Some(ref qname) = p.symbol {
            let index = AsgIndexStore::from_engine(&engine);
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => {
                    entry.symbol_id = Some(sym.symbol_id);
                }
                Ok(None) => return err_json(&format!("symbol not found: {qname}")),
                Err(e) => return err_json(&e.to_string()),
            }
        }

        match store.write_entry(&ref_name, &entry, "asd-mcp") {
            Ok(stored) => serde_json::to_string(&serde_json::json!({
                "scratch_id": stored.scratch_id,
                "status": "draft",
            }))
            .unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List scratch entries. Default: draft status, non-expired. Use status=null to see all. Returns { entries: [...], count }."
    )]
    async fn scratch_list(&self, params: Parameters<ScratchListParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let store = AsgScratchStore { repo: &engine.repo };

        let status = match p.status.as_deref() {
            Some("promoted") => Some(ScratchStatus::Promoted),
            Some("discarded") => Some(ScratchStatus::Discarded),
            Some("draft") | None => Some(ScratchStatus::Draft),
            Some(other) => return err_json(&format!("unknown status: {other}")),
        };

        let mut filter = ScratchFilter {
            workflow: p.workflow,
            session: p.session,
            status,
            exclude_expired: true,
            symbol_id: None,
        };

        if let Some(ref qname) = p.symbol {
            let index = AsgIndexStore::from_engine(&engine);
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => {
                    filter.symbol_id = Some(sym.symbol_id);
                }
                Ok(None) => return err_json(&format!("symbol not found: {qname}")),
                Err(e) => return err_json(&e.to_string()),
            }
        }

        match store.list_entries(&ref_name, &filter) {
            Ok(mut entries) => {
                entries.truncate(p.limit.max(1) as usize);
                let count = entries.len();
                serde_json::to_string(&serde_json::json!({
                    "entries": entries,
                    "count": count,
                }))
                .unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Read a single scratch entry by scratch_id. Returns the full ScratchEntry JSON."
    )]
    async fn scratch_read(&self, params: Parameters<ScratchReadParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let store = AsgScratchStore { repo: &engine.repo };
        match store.read_entry(&ref_name, &p.scratch_id) {
            Ok(entry) => serde_json::to_string(&entry).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Replace the content of an existing draft scratch entry. Returns the updated ScratchEntry."
    )]
    async fn scratch_update(&self, params: Parameters<ScratchUpdateParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let store = AsgScratchStore { repo: &engine.repo };
        match store.update_entry(&ref_name, &p.scratch_id, &p.content, "asd-mcp") {
            Ok(entry) => serde_json::to_string(&entry).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Mark a scratch entry as discarded (soft-delete). Use scratch_clean to permanently purge discarded entries. Returns { ok: true }."
    )]
    async fn scratch_discard(&self, params: Parameters<ScratchDiscardParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let store = AsgScratchStore { repo: &engine.repo };
        match store.discard_entry(&ref_name, &p.scratch_id, "asd-mcp") {
            Ok(()) => r#"{"ok":true}"#.to_string(),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Promote a draft scratch entry to a durable ledger entry. Requires `kind` and a symbol (via `qname` or the entry's existing symbol_id). Goes through policy + audit. Returns { scratch_id, promoted_to, entry_id }."
    )]
    async fn scratch_promote(&self, params: Parameters<ScratchPromoteParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let scratch_store = AsgScratchStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore::from_engine(&engine);

        // 1. Read scratch entry.
        let entry = match scratch_store.read_entry(&ref_name, &p.scratch_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        // 2. Resolve symbol_id.
        let symbol_id = if let Some(ref qname) = p.qname {
            let index = AsgIndexStore::from_engine(&engine);
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => sym.symbol_id,
                Ok(None) => return err_json(&format!("symbol not found: {qname}")),
                Err(e) => return err_json(&e.to_string()),
            }
        } else if let Some(ref sid) = entry.symbol_id {
            sid.clone()
        } else {
            return err_json("no symbol attached to scratch entry and qname was not provided");
        };

        // 3. Parse ledger kind.
        let kind = match parse_ledger_kind(&p.kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };

        // 4. Build summary.
        let summary = p.summary.unwrap_or_else(|| {
            entry
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or(&entry.content)
                .chars()
                .take(140)
                .collect()
        });

        // 5. Create LedgerEntry.
        let author = Author {
            kind: AuthorKind::Agent,
            id: "asd-mcp".to_string(),
        };
        let mut ledger_entry = LedgerEntry::new(&symbol_id, kind, &summary, author);
        ledger_entry.body = Some(entry.content.clone());

        if let Err(e) = ledger_store.append_entry(&ref_name, &ledger_entry, "asd-mcp") {
            return err_json(&e.to_string());
        }

        // 6. Mark scratch promoted.
        match scratch_store.mark_promoted(
            &ref_name,
            &entry.scratch_id,
            &ledger_entry.entry_id,
            "asd-mcp",
        ) {
            Ok(promoted) => serde_json::to_string(&serde_json::json!({
                "scratch_id": promoted.scratch_id,
                "promoted_to": ledger_entry.entry_id,
                "entry_id": ledger_entry.entry_id,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Permanently delete scratch entries matching the filter. Returns { deleted: N }. Use dry_run=true to preview without deleting."
    )]
    async fn scratch_clean(&self, params: Parameters<ScratchCleanParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let store = AsgScratchStore { repo: &engine.repo };

        let statuses: Vec<ScratchStatus> = p
            .statuses
            .split(',')
            .filter_map(|t| match t.trim() {
                "draft" => Some(ScratchStatus::Draft),
                "promoted" => Some(ScratchStatus::Promoted),
                "discarded" => Some(ScratchStatus::Discarded),
                _ => None,
            })
            .collect();

        let filter = CleanFilter {
            older_than: Some(chrono::Duration::hours(p.older_than_hours as i64)),
            statuses,
        };

        match store.clean_entries(&ref_name, &filter, p.dry_run) {
            Ok(count) => serde_json::to_string(
                &serde_json::json!({ "deleted": count, "dry_run": p.dry_run }),
            )
            .unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "One-call agent-ready context package for a planned change. Composes investigate + impact + checklist: design_invariants, known_hazards, entry_points by layer, likely_edit_files (with recency), affected_tests, effects_summary, and recently_touched git history. Use this as the first call before any non-trivial code change."
    )]
    async fn prepare_change(&self, params: Parameters<PrepareChangeParams>) -> String {
        let p = params.0;
        let intent = p.intent.as_deref().and_then(parse_intent).unwrap_or("");
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (mut tokens, mut exclusions) = parse_query(&p.description);
        if let Some(ref excl) = p.exclude {
            for term in excl
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
            {
                exclusions.push(term);
            }
        }
        if let Some(ref ctx_text) = p.task_context {
            let (ctx_tokens, _) = parse_query(ctx_text);
            for t in ctx_tokens {
                if !tokens.contains(&t) {
                    tokens.push(t);
                }
            }
        }

        if tokens.is_empty() {
            return serde_json::json!({ "description": p.description, "entry_points": {} })
                .to_string();
        }

        let depth = p.depth.max(1) as usize;
        let test_depth = p.test_depth.max(1) as usize;
        let git_depth = p.git_depth.max(1) as usize;
        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref scope) = p.scope {
            paths_filter.extend(resolve_scope(scope, &db_path));
        }
        if let Some(ref paths) = p.paths {
            paths_filter.extend(
                paths
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            tests_only: false,
            exclude_terms: exclusions,
            paths_filter,
            exclude_paths: Vec::new(),
            exclude_languages: Vec::new(),        };

        let index = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);

        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let all_qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => vec![],
        };
        let mut id_map: std::collections::HashMap<String, agentstatedeveloper_core::Symbol> =
            std::collections::HashMap::new();
        for qn in &all_qnames {
            if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, qn) {
                id_map.insert(s.symbol_id.clone(), s);
            }
        }

        let mut candidates = find_candidates(
            &engine,
            &p.description,
            &tokens,
            &filters,
            &ledger_store,
            &index,
            depth,
        );

        // Apply durable feedback adjustments.
        {
            use agentstatedeveloper_core::{FeedbackStore, apply_feedback_adjustments};
            let fb_store = AsgFeedbackStore::from_engine(&engine);
            let fb = fb_store.flat_verdicts(&ref_name).unwrap_or_default();
            apply_feedback_adjustments(&engine, &index, &p.description, &mut candidates, &fb);
        }

        let recency = gather_recency(200, 14.0);
        let layer_order = intent_layer_order(intent);
        let mut by_layer: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let mut design_invariants: Vec<serde_json::Value> = Vec::new();
        let mut known_hazards: Vec<serde_json::Value> = Vec::new();
        let mut validation_scenarios_ledger: Vec<serde_json::Value> = Vec::new();
        let mut effects_summary: Vec<serde_json::Value> = Vec::new();
        // Plan A t-009: file_scores carries (score, file, layer, days, hot,
        // top_symbol, why) so each suggested file can answer "why this file?".
        // Files below 25% of top score are dropped to reduce broad-query noise.
        //
        // Plan E t-006: shared helpers live in
        // `agentstatedeveloper_core::prepare_change` (FileScoreEntry,
        // file_score_floor(), push_file_score()). The orchestration loop
        // below is still duplicated with the CLI handler; Plan F lifts the
        // full walk. Prefer the core helpers when editing scoring logic.
        let mut file_scores: Vec<(f64, String, String, Option<f64>, bool, String, String)> =
            Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        let mut seen_vs: HashSet<String> = HashSet::new();
        let mut seen_effect: HashSet<String> = HashSet::new();
        let mut top_sym_id: Option<String> = None;
        // Effects floor stays at 25% (broad signal — admit any
        // symbol with even loosely-relevant effects).
        let effect_score_floor = candidates.first().map(|(s, _)| s * 0.25).unwrap_or(0.0);
        // ExampleFlow refinement #2 (1.0.83): file floor bumped to
        // 40% via the core helper (was 0.25, shared with effects).
        // Diverged because the noise-suppression need is asymmetric:
        // surfacing one unrelated file is worse than missing one
        // tangential effect.
        let file_score_floor = agentstatedeveloper_core::file_score_floor(&candidates);

        for (score, qname) in &candidates {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            let tier = symbol_tier(&sym.file);
            let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
            let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
            let rec = recency.get(&sym.file);
            let ltd = rec.and_then(|r| r.last_touched_days);
            let hot = rec.map(|r| r.hot).unwrap_or(false);
            if top_sym_id.is_none() {
                top_sym_id = Some(sym.symbol_id.clone());
            }
            let entries = ledger_store
                .list_entries(&ref_name, &sym.symbol_id)
                .unwrap_or_default();
            if seen_files.insert(sym.file.clone()) && *score >= file_score_floor {
                let reasons = explain_match(&sym, &tokens, &entries, hot);
                let why = reasons
                    .first()
                    .cloned()
                    .unwrap_or_else(|| format!("contains symbol {}", sym.qname));
                file_scores.push((
                    *score,
                    sym.file.clone(),
                    layer.to_string(),
                    ltd,
                    hot,
                    sym.qname.clone(),
                    why,
                ));
            }
            for entry in &entries {
                match entry.kind {
                    LedgerKind::Invariant => {
                        if seen_inv.insert(entry.summary.clone()) {
                            // 1.0.86: include entry_id so downstream
                            // sections (preserve, suggested_test_coverage,
                            // scenario_tests) can ref instead of duplicate.
                            design_invariants.push(serde_json::json!({
                                "entry_id": entry.entry_id,
                                "summary": entry.summary,
                                "source": sym.qname,
                            }));
                        }
                    }
                    LedgerKind::Hazard => {
                        known_hazards.push(
                            serde_json::json!({ "summary": entry.summary, "source": sym.qname }),
                        );
                    }
                    LedgerKind::ValidationScenario => {
                        if seen_vs.insert(entry.summary.clone()) {
                            validation_scenarios_ledger.push(serde_json::json!({ "scenario": entry.summary, "source": sym.qname }));
                        }
                    }
                    _ => {}
                }
            }
            if *score >= effect_score_floor {
                if let Ok(Some(decl)) = effect_store.get_effects(&ref_name, &sym.symbol_id) {
                    let has_high_signal = decl.declared.iter().any(|e| !e.effect.is_low_signal());
                    for eff in &decl.declared {
                        if has_high_signal && eff.effect.is_low_signal() {
                            continue;
                        }
                        let cat = eff.effect.as_str().to_string();
                        let key = format!("{}:{}", cat, sym.qname);
                        if seen_effect.insert(key) {
                            effects_summary
                                .push(serde_json::json!({ "category": cat, "source": sym.qname }));
                        }
                    }
                }
            }
            let ep = serde_json::json!({
                "score": score, "qname": sym.qname, "file": sym.file,
                "line": sym.start.line, "layer": layer, "summary": summary,
                "last_touched_days": ltd, "hot": hot,
            });
            by_layer
                .entry(layer.to_string())
                .or_insert_with(|| serde_json::Value::Array(vec![]))
                .as_array_mut()
                .unwrap()
                .push(ep);
        }

        // Plan J t-001: invariant propagation from direct callers.
        // See the CLI handler in commands/prepare_change.rs for the
        // full rationale; this mirror keeps the MCP `prepare_change`
        // tool surface symmetric.
        let mut candidate_sym_ids_pc: Vec<(String, String)> = Vec::new();
        for (_, qname) in candidates.iter() {
            if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, qname) {
                candidate_sym_ids_pc.push((s.symbol_id, s.qname));
            }
        }
        let mut caller_ids_seen_pc: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut caller_visit_order_pc: Vec<(String, String)> = Vec::new();
        for (cand_sym_id, _) in &candidate_sym_ids_pc {
            let direct_callers = index
                .get_callers(&ref_name, cand_sym_id)
                .unwrap_or_default();
            for caller_id in direct_callers {
                if candidate_sym_ids_pc.iter().any(|(sid, _)| sid == &caller_id) {
                    continue;
                }
                if caller_ids_seen_pc.insert(caller_id.clone()) {
                    caller_visit_order_pc.push((cand_sym_id.clone(), caller_id));
                }
            }
        }
        let caller_id_strs: Vec<&str> = caller_visit_order_pc
            .iter()
            .map(|(_, cid)| cid.as_str())
            .collect();
        let caller_resolved = SearchFtsDb::open(&db_path)
            .ok()
            .map(|fts| fts.resolve_symbol_ids_bulk(&caller_id_strs))
            .unwrap_or_default();
        // Fallback qname lookup when FTS cache is cold (fresh post-
        // import, or test fixtures writing edges directly). Builds
        // the reverse index once via a single qname-tree walk.
        let need_fallback_pc = caller_visit_order_pc
            .iter()
            .any(|(_, cid)| !caller_resolved.contains_key(cid.as_str()));
        let fallback_id_to_qname_pc: std::collections::HashMap<String, String> =
            if need_fallback_pc {
                let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
                match engine.repo.get_tree(&ref_name, &prefix) {
                    Ok(serde_json::Value::Object(m)) => m
                        .into_iter()
                        .filter_map(|(qn, v)| {
                            v.get("symbol_id")
                                .and_then(|v| v.as_str())
                                .map(|sid| (sid.to_string(), qn))
                        })
                        .collect(),
                    _ => std::collections::HashMap::new(),
                }
            } else {
                std::collections::HashMap::new()
            };
        for (_cand_sym_id, caller_id) in &caller_visit_order_pc {
            let caller_qname = caller_resolved
                .get(caller_id.as_str())
                .map(|r| r.qname.as_str())
                .or_else(|| fallback_id_to_qname_pc.get(caller_id).map(String::as_str));
            let Some(caller_qname) = caller_qname else {
                continue;
            };
            let caller_entries = ledger_store
                .list_entries(&ref_name, caller_id)
                .unwrap_or_default();
            for entry in caller_entries {
                if !matches!(entry.kind, LedgerKind::Invariant) {
                    continue;
                }
                if seen_inv.insert(entry.summary.clone()) {
                    // 1.0.86: include entry_id (see other push site).
                    design_invariants.push(serde_json::json!({
                        "entry_id": entry.entry_id,
                        "summary": entry.summary,
                        "source": caller_qname,
                        "from_caller": true,
                    }));
                }
            }
        }

        // Reorder by_layer.
        let mut ordered: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for lk in layer_order {
            if let Some(v) = by_layer.remove(*lk) {
                ordered.insert(lk.to_string(), v);
            }
        }

        file_scores.sort_by(|a, b| {
            b.4.cmp(&a.4)
                .then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
        });
        // 1.0.87: file-level cliff detection (mirrors CLI prepare_change).
        let mut score_only_sorted: Vec<f64> = file_scores.iter().map(|f| f.0).collect();
        score_only_sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let cliff_cut = agentstatedeveloper_core::cliff_cutoff_index(
            score_only_sorted.iter().copied(),
        );
        if cliff_cut < file_scores.len() {
            let cutoff_score = score_only_sorted[cliff_cut - 1];
            file_scores.retain(|f| f.0 >= cutoff_score);
        }
        let dirty_files_pc = git_dirty_files();
        let likely_edit_files: Vec<serde_json::Value> = file_scores
            .iter()
            .map(|(score, file, layer, days, hot, top_symbol, why)| {
                // Plan J t-003: use the unified core classifier so
                // CLI and MCP agree on the role taxonomy (was
                // inline-divergent before — MCP missed fixture /
                // script / generated / view / viewmodel).
                let file_role = agentstatedeveloper_core::classify_file_role(file);
                let conflict_risk = dirty_files_pc.contains(file.as_str());
                serde_json::json!({
                    "file": file, "layer": layer, "score": score,
                    "last_touched_days": days, "hot": hot,
                    "file_role": file_role, "conflict_risk": conflict_risk,
                    "top_symbol": top_symbol, "why": why,
                })
            })
            .collect();

        // Affected tests via BFS from top entry point.
        let mut affected_tests: Vec<serde_json::Value> = Vec::new();
        if let Some(start_id) = top_sym_id {
            let mut visited: HashSet<String> = HashSet::new();
            let mut queue: VecDeque<(String, usize)> = VecDeque::new();
            let mut seen_tnames: HashSet<String> = HashSet::new();
            visited.insert(start_id.clone());
            queue.push_back((start_id, 0));
            while let Some((sid, depth)) = queue.pop_front() {
                if depth >= test_depth {
                    continue;
                }
                let callers = index.get_callers(&ref_name, &sid).unwrap_or_default();
                for cid in callers {
                    if visited.contains(&cid) {
                        continue;
                    }
                    visited.insert(cid.clone());
                    if let Some(s) = id_map.get(&cid) {
                        if symbol_tier(&s.file) == 2 && seen_tnames.insert(s.qname.clone()) {
                            let qname_words: Vec<String> = s
                                .qname
                                .split(|c: char| !c.is_alphabetic())
                                .filter(|t: &&str| t.len() > 2)
                                .map(|t| t.to_lowercase())
                                .collect();
                            let doc_words: Vec<String> = s
                                .doc
                                .as_deref()
                                .unwrap_or("")
                                .split(|c: char| !c.is_alphabetic())
                                .filter(|t: &&str| t.len() > 2)
                                .map(|t| t.to_lowercase())
                                .collect();
                            let test_tokens: Vec<&str> = qname_words
                                .iter()
                                .chain(doc_words.iter())
                                .map(|s| s.as_str())
                                .collect();
                            let covers: Vec<&str> = design_invariants
                                .iter()
                                .filter_map(|inv| {
                                    inv.get("summary").and_then(serde_json::Value::as_str)
                                })
                                .filter(|sum| {
                                    let sl = sum.to_lowercase();
                                    test_tokens.iter().any(|t| sl.contains(*t))
                                })
                                .collect();
                            affected_tests.push(serde_json::json!({
                                "qname": s.qname, "file": s.file, "line": s.start.line,
                                "covers_invariants": covers,
                            }));
                        }
                        if depth + 1 < test_depth {
                            queue.push_back((cid, depth + 1));
                        }
                    }
                }
            }
        }

        // Recent git touches on top 3 files.
        let top_files: Vec<(String, usize)> = file_scores
            .iter()
            .take(3)
            .map(|(_, f, _, _, _, _, _)| (f.clone(), 0))
            .collect();
        let recently_touched = mcp_git_recent_touches(&top_files, git_depth);

        let test_gap = affected_tests.is_empty();
        let proposed_test_path = test_gap
            .then(|| {
                file_scores
                    .first()
                    .map(|(_, f, _, _, _, _, _)| propose_test_path(f))
            })
            .flatten();
        // Plan J t-007: language-aware test stub body — same trigger
        // as proposed_test_path. file_scores tuple = (score, file,
        // layer, days, hot, qname, why); element 5 is the symbol qname
        // used to derive the test name (snake_case for py/rs/rb/ts,
        // PascalCase for go/java/cs/kt/swift).
        let proposed_test_stub: Option<String> = if test_gap {
            file_scores
                .first()
                .map(|(_, file, _, _, _, qname, _)| {
                    agentstatedeveloper_core::propose_test_stub(file, qname)
                })
        } else {
            None
        };
        // 1.0.86: emit refs for invariant-derived hints (dedupes
        // against design_invariants[].summary); keep effects + cold-
        // start as inline hints (genuinely new content).
        let suggested_test_coverage: Vec<serde_json::Value> = if test_gap {
            let mut out: Vec<serde_json::Value> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for inv in &design_invariants {
                if let Some(eid) = inv.get("entry_id").and_then(serde_json::Value::as_str) {
                    if seen.insert(format!("ref:{eid}")) {
                        out.push(serde_json::json!({ "ref": eid }));
                    }
                }
            }
            for eff in &effects_summary {
                if let Some(cat) = eff.get("category").and_then(serde_json::Value::as_str) {
                    let hint = format!("verify {} after change", cat.to_lowercase());
                    if seen.insert(format!("hint:{hint}")) {
                        out.push(serde_json::json!({ "hint": hint }));
                    }
                }
            }
            if design_invariants.is_empty() {
                if let Some((_, qname)) = candidates.first() {
                    if let Ok(Some(sym)) = index.get_symbol_by_qname(&ref_name, qname) {
                        for h in derive_cold_hints(
                            &sym.qname,
                            sym.signature.as_deref(),
                            sym.doc.as_deref(),
                        ) {
                            if seen.insert(format!("hint:{h}")) {
                                out.push(serde_json::json!({ "hint": h }));
                            }
                        }
                    }
                }
            }
            out
        } else {
            vec![]
        };

        const CONSTRAINT_WORDS: &[&str] = &[
            "must",
            "never",
            "shall",
            "always",
            "only",
            "cannot",
            "no ",
            "not ",
            "require",
            "ensure",
            "prevent",
            "guarantee",
            "invariant",
            "forbidden",
        ];
        // 1.0.86: emit refs against design_invariants[].entry_id
        // instead of duplicating summary text.
        let scenario_tests: Vec<serde_json::Value> = design_invariants
            .iter()
            .filter_map(|inv| {
                let summary = inv.get("summary").and_then(serde_json::Value::as_str)?;
                let entry_id = inv.get("entry_id").and_then(serde_json::Value::as_str)?;
                let sl = summary.to_lowercase();
                if CONSTRAINT_WORDS.iter().any(|w| sl.contains(w)) {
                    Some(serde_json::json!({ "ref": entry_id }))
                } else {
                    None
                }
            })
            .collect();

        // T1: safe-change recipe.  T4: manually_validate includes ValidationScenario entries.
        let recipe_inspect: Vec<serde_json::Value> = file_scores.iter()
            .map(|(score, file, layer, days, hot, top_symbol, why)| serde_json::json!({
                "file": file, "layer": layer, "score": score, "last_touched_days": days, "hot": hot,
                "top_symbol": top_symbol, "why": why,
            }))
            .collect();
        // 1.0.86: invariants emit refs; hazards stay inline (no
        // dedupe target in this response).
        let recipe_preserve: Vec<serde_json::Value> = design_invariants
            .iter()
            .filter_map(|inv| {
                inv.get("entry_id")
                    .and_then(serde_json::Value::as_str)
                    .map(|eid| serde_json::json!({ "ref": eid, "kind": "invariant" }))
            })
            .chain(known_hazards.iter().map(|h| {
                serde_json::json!({
                    "constraint": h["summary"],
                    "source": h["source"],
                    "kind": "hazard",
                })
            }))
            .collect();
        let recipe_edit: Vec<serde_json::Value> = likely_edit_files
            .iter()
            .filter(|f| f["file_role"].as_str() == Some("impl"))
            .cloned()
            .chain(
                likely_edit_files
                    .iter()
                    .filter(|f| f["file_role"].as_str() != Some("impl"))
                    .cloned(),
            )
            .collect();
        let recipe_run: Vec<serde_json::Value> = affected_tests.iter()
            .map(|t| serde_json::json!({ "qname": t["qname"], "file": t["file"], "covers_invariants": t["covers_invariants"] }))
            .collect();
        let mut recipe_manually_validate: Vec<serde_json::Value> =
            validation_scenarios_ledger.clone();
        for s in &scenario_tests {
            recipe_manually_validate.push(serde_json::json!({ "scenario": s, "source": "invariant", "kind": "constraint_check" }));
        }
        for eff in &effects_summary {
            let desc = format!(
                "verify {} side-effect still correct after change",
                eff["category"].as_str().unwrap_or("").to_lowercase()
            );
            recipe_manually_validate.push(serde_json::json!({ "scenario": desc, "source": eff["source"], "kind": "effect_check" }));
        }
        // Plan J t-002: when no affected tests exist, prepend a
        // missing_test item to the recipe so an agent reading only
        // the recipe sees the gap (without having to cross-reference
        // top-level `test_gap` field).
        if test_gap {
            let suggestion = proposed_test_path
                .as_deref()
                .unwrap_or("(no proposed path; see proposed_test_path)");
            recipe_manually_validate.push(serde_json::json!({
                "scenario": format!(
                    "No test currently exercises this change set. Add a test \
                     covering the planned edit; suggested target: {suggestion}"
                ),
                "source": "test_gap",
                "kind": "missing_test",
            }));
        }
        // ExampleFlow refinement #1 (1.0.84): recursively drop
        // empty sub-fields. Matches CLI prepare_change handling.
        let safe_change_recipe = agentstatedeveloper_core::drop_empty_recursive(
            serde_json::json!({
                "inspect": recipe_inspect,
                "preserve": recipe_preserve,
                "edit": recipe_edit,
                "run": recipe_run,
                "manually_validate": recipe_manually_validate,
            }),
        );

        let focus = intent_focus(intent);
        let layers_present_pc: std::collections::HashSet<&str> = file_scores
            .iter()
            .map(|(_, _, layer, _, _, _, _)| layer.as_str())
            .collect();
        let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
        let possible_misses =
            detect_possible_misses(&p.description, &layers_present_pc, file_scores.len());
        // Plan G t-006: surface captured thinking on the symbols that
        // matter for this query. Pull top_symbol off each likely_edit_files
        // entry; gather_prior_thinking walks the ledger and projects to the
        // compact `prior_thinking` shape (or Value::Null if nothing to surface).
        let thinking_qnames: Vec<String> = likely_edit_files
            .iter()
            .filter_map(|f| {
                f.get("top_symbol")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
            })
            .collect();
        // ExampleFlow refinement (2026-06-04): gather now returns
        // PriorThinking { entries, summary }. `thinking_summary` always
        // emits (load-bearing signal: tells agents whether thinking
        // exists but was filtered, vs. doesn't exist at all).
        let pt = thinking::gather_prior_thinking(
            &engine,
            &thinking_qnames,
            thinking::DEFAULT_CONFIDENCE_FLOOR,
        );
        let prior_thinking = pt.entries;
        let thinking_summary = pt.summary;

        let full = serde_json::json!({
            "description": p.description,
            "task_context": p.task_context,
            "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
            "focus": if focus.is_empty() { serde_json::Value::Null } else { serde_json::json!(focus) },
            "ambiguous_terms": ambiguous_terms,
            "possible_misses": possible_misses,
            "safe_change_recipe": safe_change_recipe,
            "design_invariants": design_invariants,
            "known_hazards": known_hazards,
            "validation_scenarios": validation_scenarios_ledger,
            "entry_points": { "by_layer": ordered },
            "likely_edit_files": likely_edit_files,
            "affected_tests": affected_tests,
            "test_gap": test_gap,
            "proposed_test_path": proposed_test_path,
            "proposed_test_stub": proposed_test_stub,
            "suggested_test_coverage": suggested_test_coverage,
            "scenario_tests": scenario_tests,
            "effects_summary": effects_summary,
            "recently_touched": recently_touched,
            "prior_thinking": prior_thinking,
            "thinking_summary": thinking_summary,
            // ExampleFlow refinement (1.0.77): 24h soft threshold for
            // prepare-change (matches a typical dev day; index built
            // this morning is fine all afternoon). When age exceeds the
            // threshold but the query DID resolve, downstream UIs can
            // demote via `stale_severity == "soft"`. Loud severity
            // ("critical") fires on empty/broken FTS regardless of age.
            "stale": agentstatedeveloper_core::stale_warning_classified(
                &db_path,
                agentstatedeveloper_core::SOFT_STALE_THRESHOLD_SECS,
            )
                .as_ref()
                .map(|w| serde_json::Value::String(w.message.clone()))
                .unwrap_or(serde_json::Value::Null),
            "stale_severity": agentstatedeveloper_core::stale_warning_classified(
                &db_path,
                agentstatedeveloper_core::SOFT_STALE_THRESHOLD_SECS,
            )
                .as_ref()
                .map(|w| serde_json::to_value(w.severity).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
            "confidence": {
                "strong": "orientation across layers (app/engine/UI/persistence) for a feature-level change description",
                "weak": "narrow bug-fix work — verify each suggested file with `references` or `read` before editing; broad descriptions can surface unrelated files",
            },
        });
        // Plan F t-006: brief drops by_layer / recently_touched / scenario_tests
        // / suggested_test_coverage / effects_summary and trims likely_edit_files
        // to {file, why, top_symbol, layer}.
        let mut out = if brief::brief_from_env() {
            brief::brief_prepare_change(&full)
        } else {
            full
        };
        // Token economy (1.0.79): drop input echoes (the MCP client
        // just sent these) and dedupe stale string vs severity. Then
        // strip top-level null/[]/{} via drop_empty_top_level.
        if let Some(obj) = out.as_object_mut() {
            obj.remove("description");
            obj.remove("task_context");
            obj.remove("stale");
        }
        let out = agentstatedeveloper_core::drop_empty_top_level(out);
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Pre-edit checklist for a query: files to inspect, invariants to preserve, tests to run, known hazards, and effects to verify. Returns structured JSON. Use this before any code edit to get a focused action list."
    )]
    async fn checklist(&self, params: Parameters<ChecklistParams>) -> String {
        let p = params.0;
        let intent = p.intent.as_deref().and_then(parse_intent).unwrap_or("");
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (tokens, mut exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
            {
                exclusions.push(term);
            }
        }

        if tokens.is_empty() {
            return serde_json::json!({ "query": p.query, "files_to_inspect": [] }).to_string();
        }

        let depth = p.depth.max(1) as usize;
        let test_depth = p.test_depth.max(1) as usize;
        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref scope) = p.scope {
            paths_filter.extend(resolve_scope(scope, &db_path));
        }
        if let Some(ref paths) = p.paths {
            paths_filter.extend(
                paths
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            tests_only: false,
            exclude_terms: exclusions,
            paths_filter,
            exclude_paths: Vec::new(),
            exclude_languages: Vec::new(),        };

        let index = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);

        // Build id_map for test BFS.
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let all_qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => vec![],
        };
        let mut id_map: std::collections::HashMap<String, agentstatedeveloper_core::Symbol> =
            std::collections::HashMap::new();
        for qn in &all_qnames {
            if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, qn) {
                id_map.insert(s.symbol_id.clone(), s);
            }
        }

        let mut candidates = find_candidates(
            &engine,
            &p.query,
            &tokens,
            &filters,
            &ledger_store,
            &index,
            depth,
        );

        // Apply durable feedback adjustments.
        {
            use agentstatedeveloper_core::{FeedbackStore, apply_feedback_adjustments};
            let fb_store = AsgFeedbackStore::from_engine(&engine);
            let fb = fb_store.flat_verdicts(&ref_name).unwrap_or_default();
            apply_feedback_adjustments(&engine, &index, &p.query, &mut candidates, &fb);
        }

        let mut files_to_inspect: Vec<serde_json::Value> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut invariants: Vec<serde_json::Value> = Vec::new();
        let mut hazards: Vec<serde_json::Value> = Vec::new();
        let mut effects_list: Vec<serde_json::Value> = Vec::new();
        let mut test_rows: Vec<serde_json::Value> = Vec::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        let mut seen_tests: HashSet<String> = HashSet::new();

        for (_score, qname) in &candidates {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            let tier = symbol_tier(&sym.file);
            let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);

            if seen_files.insert(sym.file.clone()) {
                files_to_inspect.push(serde_json::json!({
                    "file": sym.file, "qname": sym.qname, "layer": layer, "line": sym.start.line,
                }));
            }

            let entries = ledger_store
                .list_entries(&ref_name, &sym.symbol_id)
                .unwrap_or_default();
            for entry in &entries {
                match entry.kind {
                    LedgerKind::Invariant => {
                        if seen_inv.insert(entry.summary.clone()) {
                            invariants.push(serde_json::json!({
                                "summary": entry.summary, "source": sym.qname, "body": entry.body,
                            }));
                        }
                    }
                    LedgerKind::Hazard | LedgerKind::KnownBug => {
                        hazards.push(serde_json::json!({
                            "summary": entry.summary, "source": sym.qname,
                            "kind": entry.kind.as_str(), "body": entry.body,
                        }));
                    }
                    LedgerKind::ValidationScenario => {
                        if seen_inv.insert(entry.summary.clone()) {
                            invariants.push(serde_json::json!({
                                "summary": entry.summary, "source": sym.qname,
                                "kind": "validation_scenario", "body": entry.body,
                            }));
                        }
                    }
                    _ => {}
                }
            }

            if let Ok(Some(decl)) = effect_store.get_effects(&ref_name, &sym.symbol_id) {
                for eff in &decl.declared {
                    effects_list.push(serde_json::json!({
                        "category": format!("{:?}", eff.effect), "source": sym.qname,
                    }));
                }
            }

            // BFS for test callers.
            let mut visited: HashSet<String> = HashSet::new();
            let mut queue: VecDeque<(String, usize)> = VecDeque::new();
            visited.insert(sym.symbol_id.clone());
            queue.push_back((sym.symbol_id.clone(), 0));
            while let Some((sid, depth)) = queue.pop_front() {
                if depth >= test_depth {
                    continue;
                }
                let callers = index.get_callers(&ref_name, &sid).unwrap_or_default();
                for cid in callers {
                    if visited.contains(&cid) {
                        continue;
                    }
                    visited.insert(cid.clone());
                    if let Some(s) = id_map.get(&cid) {
                        if symbol_tier(&s.file) == 2 && seen_tests.insert(s.qname.clone()) {
                            test_rows.push(serde_json::json!({
                                "qname": s.qname, "file": s.file, "line": s.start.line,
                            }));
                        }
                        if depth + 1 < test_depth {
                            queue.push_back((cid, depth + 1));
                        }
                    }
                }
            }
        }

        let test_gap = test_rows.is_empty();
        let proposed_test_path = test_gap
            .then(|| {
                files_to_inspect
                    .first()
                    .and_then(|v| v.get("file").and_then(serde_json::Value::as_str))
                    .map(propose_test_path)
            })
            .flatten();
        let suggested_test_coverage: Vec<String> = if test_gap {
            let mut hints: Vec<String> = invariants
                .iter()
                .filter_map(|inv| inv.get("summary").and_then(serde_json::Value::as_str))
                .map(|s| s.to_string())
                .collect();
            for eff in &effects_list {
                if let Some(cat) = eff.get("category").and_then(serde_json::Value::as_str) {
                    let hint = format!("verify {} after change", cat.to_lowercase());
                    if !hints.contains(&hint) {
                        hints.push(hint);
                    }
                }
            }
            if invariants.is_empty() {
                if let Some((_, qname)) = candidates.first() {
                    if let Ok(Some(sym)) = index.get_symbol_by_qname(&ref_name, qname) {
                        for h in derive_cold_hints(
                            &sym.qname,
                            sym.signature.as_deref(),
                            sym.doc.as_deref(),
                        ) {
                            if !hints.contains(&h) {
                                hints.push(h);
                            }
                        }
                    }
                }
            }
            hints
        } else {
            vec![]
        };

        const CONSTRAINT_WORDS_CL: &[&str] = &[
            "must",
            "never",
            "shall",
            "always",
            "only",
            "cannot",
            "no ",
            "not ",
            "require",
            "ensure",
            "prevent",
            "guarantee",
            "invariant",
            "forbidden",
        ];
        let scenario_tests: Vec<&str> = invariants
            .iter()
            .filter_map(|inv| inv.get("summary").and_then(serde_json::Value::as_str))
            .filter(|s| {
                let sl = s.to_lowercase();
                CONSTRAINT_WORDS_CL.iter().any(|w| sl.contains(w))
            })
            .collect();

        // T-004: task-close proof suggestions.
        let task_close_suggestions: Vec<serde_json::Value> = {
            let mut suggestions = Vec::new();
            for inv in invariants.iter().take(4) {
                let source = inv
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let summary = inv
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !source.is_empty() && !summary.is_empty() {
                    suggestions.push(serde_json::json!({
                        "action": "ledger_append", "kind": "proof", "symbol": source,
                        "suggested_summary": format!("verified that {} holds after change", summary),
                    }));
                }
            }
            for h in hazards.iter().take(2) {
                let source = h
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let summary = h
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !source.is_empty() && !summary.is_empty() {
                    suggestions.push(serde_json::json!({
                        "action": "ledger_append", "kind": "validation_scenario", "symbol": source,
                        "suggested_summary": format!("validate that hazard '{}' was not triggered", summary),
                    }));
                }
            }
            for eff in effects_list.iter().take(2) {
                let source = eff
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let cat = eff
                    .get("category")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !source.is_empty() && !cat.is_empty() {
                    suggestions.push(serde_json::json!({
                        "action": "ledger_append", "kind": "proof", "symbol": source,
                        "suggested_summary": format!("verified {} side-effect is correct after change", cat.to_lowercase()),
                    }));
                }
            }
            suggestions
        };

        let focus = intent_focus(intent);
        let layers_present_cl: std::collections::HashSet<&str> = files_to_inspect
            .iter()
            .filter_map(|f| f.get("layer").and_then(serde_json::Value::as_str))
            .collect();
        let ambiguous_terms_cl = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);
        let possible_misses_cl =
            detect_possible_misses(&p.query, &layers_present_cl, files_to_inspect.len());
        serde_json::to_string(&serde_json::json!({
            "query": p.query,
            "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
            "focus": if focus.is_empty() { serde_json::Value::Null } else { serde_json::json!(focus) },
            "ambiguous_terms": ambiguous_terms_cl,
            "possible_misses": possible_misses_cl,
            "files_to_inspect": files_to_inspect,
            "invariants_to_preserve": invariants,
            "tests_to_run": test_rows,
            "test_gap": test_gap,
            "proposed_test_path": proposed_test_path,
            "suggested_test_coverage": suggested_test_coverage,
            "scenario_tests": scenario_tests,
            "known_hazards": hazards,
            "effects_to_verify": effects_list,
            "task_close_suggestions": task_close_suggestions,
        })).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Blast-radius analysis for a symbol before editing. Returns transitive callers (up to depth), aggregated effects, invariants/hazards from all callers, affected test symbols, and recent git touches per file. Use this before any code change to understand scope."
    )]
    async fn impact(&self, params: Parameters<ImpactParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);

        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let tier = symbol_tier(&symbol.file);
        let layer = classify_layer_sym(&symbol.file, &symbol.qname, tier, &layer_overrides);

        // Build id_map for call graph resolution.
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let all_qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => vec![],
        };
        let mut id_map: std::collections::HashMap<String, agentstatedeveloper_core::Symbol> =
            std::collections::HashMap::new();
        for qn in &all_qnames {
            if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, qn) {
                id_map.insert(s.symbol_id.clone(), s);
            }
        }

        // BFS transitive callers.
        let max_depth = p.depth.max(1) as usize;
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        visited.insert(symbol.symbol_id.clone());
        queue.push_back((symbol.symbol_id.clone(), 0));

        let mut caller_rows: Vec<serde_json::Value> = Vec::new();
        let mut affected_test_rows: Vec<serde_json::Value> = Vec::new();
        let mut touched_files: Vec<(String, usize)> = vec![(symbol.file.clone(), 0)];

        while let Some((sym_id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let neighbors = index.get_callers(&ref_name, &sym_id).unwrap_or_default();
            for nbr_id in neighbors {
                if visited.contains(&nbr_id) {
                    continue;
                }
                visited.insert(nbr_id.clone());
                if let Some(s) = id_map.get(&nbr_id) {
                    let t = symbol_tier(&s.file);
                    let l = classify_layer_sym(&s.file, &s.qname, t, &layer_overrides);
                    let row = serde_json::json!({
                        "qname": s.qname,
                        "file": s.file,
                        "line": s.start.line,
                        "depth": depth + 1,
                        "layer": l,
                    });
                    if t == 2 {
                        affected_test_rows.push(row);
                    } else {
                        caller_rows.push(row);
                    }
                    if !touched_files.iter().any(|(f, _)| f == &s.file) {
                        touched_files.push((s.file.clone(), depth + 1));
                    }
                    if depth + 1 < max_depth {
                        queue.push_back((nbr_id, depth + 1));
                    }
                }
            }
        }

        // Collect invariants/hazards/validation_scenarios/known_bugs
        // from target + all callers. Plan J t-013 extends the prior
        // (invariants + hazards only) shape to surface
        // ValidationScenario and KnownBug — same blast-radius
        // pattern, same `source_symbol_id` annotation. Invariants are
        // deduped by summary; the other kinds aren't (callers want
        // every instance for impact reasoning).
        // Pre-existing bug surfaced by Plan J t-013 tests: `visited`
        // already contains the target symbol, so chain(once(target),
        // visited) walked the target's ledger twice. Dedupe at source.
        let mut seen_ids: HashSet<String> = HashSet::new();
        let all_sym_ids: Vec<String> = std::iter::once(symbol.symbol_id.clone())
            .chain(visited.iter().cloned())
            .filter(|id| seen_ids.insert(id.clone()))
            .collect();
        let mut all_invariants: Vec<serde_json::Value> = Vec::new();
        let mut all_hazards: Vec<serde_json::Value> = Vec::new();
        let mut all_validation_scenarios: Vec<serde_json::Value> = Vec::new();
        let mut all_known_bugs: Vec<serde_json::Value> = Vec::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        for sym_id in &all_sym_ids {
            let entries = ledger_store
                .list_entries(&ref_name, sym_id)
                .unwrap_or_default();
            for entry in entries {
                let key = entry.summary.clone();
                match entry.kind {
                    LedgerKind::Invariant => {
                        if seen_inv.insert(key) {
                            let mut v = serde_json::to_value(&entry).unwrap_or_default();
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert(
                                    "source_symbol_id".to_string(),
                                    serde_json::json!(sym_id),
                                );
                            }
                            all_invariants.push(v);
                        }
                    }
                    LedgerKind::Hazard => {
                        let mut v = serde_json::to_value(&entry).unwrap_or_default();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("source_symbol_id".to_string(), serde_json::json!(sym_id));
                        }
                        all_hazards.push(v);
                    }
                    LedgerKind::ValidationScenario => {
                        let mut v = serde_json::to_value(&entry).unwrap_or_default();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("source_symbol_id".to_string(), serde_json::json!(sym_id));
                        }
                        all_validation_scenarios.push(v);
                    }
                    LedgerKind::KnownBug => {
                        let mut v = serde_json::to_value(&entry).unwrap_or_default();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("source_symbol_id".to_string(), serde_json::json!(sym_id));
                        }
                        all_known_bugs.push(v);
                    }
                    _ => {}
                }
            }
        }

        // Effects for the target symbol.
        let effects = effect_store
            .get_effects(&ref_name, &symbol.symbol_id)
            .unwrap_or(None);

        // Recent git touches.
        let git_depth = p.git_depth.max(1) as usize;
        let recently_touched = mcp_git_recent_touches(&touched_files, git_depth);

        let mut sym_val = serde_json::to_value(&symbol).unwrap_or_default();
        if let Some(obj) = sym_val.as_object_mut() {
            obj.remove("body");
        }

        serde_json::to_string(&serde_json::json!({
            "symbol": sym_val,
            "layer": layer,
            "caller_count": caller_rows.len(),
            "test_count": affected_test_rows.len(),
            "invariants": all_invariants,
            "hazards": all_hazards,
            "validation_scenarios": all_validation_scenarios,
            "known_bugs": all_known_bugs,
            "effects": effects,
            "callers": caller_rows,
            "affected_tests": affected_test_rows,
            "recently_touched": recently_touched,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    /// Symbols in files changed since a commit + combined blast radius.
    /// PR-review workflow: pass the base SHA to get full impact without knowing any symbol names.
    #[tool(
        description = "Symbols in files changed since a commit and their combined blast radius. Pass the base SHA of a branch/PR to discover all symbols touched by the diff, their transitive callers, affected tests, invariants, hazards, and effects — without needing to know any symbol names upfront."
    )]
    async fn since(&self, params: Parameters<SinceParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);

        // Build id_map.
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let all_qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => vec![],
        };
        let mut id_map: std::collections::HashMap<String, agentstatedeveloper_core::Symbol> =
            std::collections::HashMap::new();
        for qn in &all_qnames {
            if let Ok(Some(s)) = index.get_symbol_by_qname(&ref_name, qn) {
                id_map.insert(s.symbol_id.clone(), s);
            }
        }

        // Get changed files.
        let changed_files: Vec<String> = {
            let out = Proc::new("git")
                .args(["diff", "--name-only", &format!("{}..HEAD", p.sha)])
                .output();
            match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect(),
                _ => vec![],
            }
        };

        if changed_files.is_empty() {
            return serde_json::to_string(&serde_json::json!({
                "sha": p.sha, "changed_files": [], "touched_symbols": {},
                "callers": [], "affected_tests": [], "invariants": [], "hazards": [], "effects": [],
            }))
            .unwrap_or_else(|_| "{}".to_string());
        }

        let changed_set: HashSet<&str> = changed_files.iter().map(String::as_str).collect();

        // Seeds: all symbols in changed files.
        let seed_ids: Vec<String> = id_map
            .values()
            .filter(|s| changed_set.contains(s.file.as_str()))
            .map(|s| s.symbol_id.clone())
            .collect();

        // Group touched symbols by layer.
        let mut by_layer: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        for sid in &seed_ids {
            if let Some(s) = id_map.get(sid) {
                let tier = symbol_tier(&s.file);
                let layer = classify_layer_sym(&s.file, &s.qname, tier, &layer_overrides);
                by_layer
                    .entry(layer.to_string())
                    .or_default()
                    .push(serde_json::json!({
                        "qname": s.qname, "file": s.file, "line": s.start.line, "layer": layer,
                    }));
            }
        }

        // BFS blast radius.
        let max_depth = p.depth.max(1) as usize;
        let mut visited: HashSet<String> = seed_ids.iter().cloned().collect();
        let mut queue: VecDeque<(String, usize)> =
            seed_ids.iter().map(|id| (id.clone(), 0)).collect();
        let mut caller_rows: Vec<serde_json::Value> = Vec::new();
        let mut affected_test_rows: Vec<serde_json::Value> = Vec::new();
        let mut touched_files: Vec<(String, usize)> =
            changed_files.iter().map(|f| (f.clone(), 0)).collect();
        let mut seen_files: HashSet<String> = changed_files.iter().cloned().collect();

        while let Some((sym_id, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let neighbors = index.get_callers(&ref_name, &sym_id).unwrap_or_default();
            for nbr_id in neighbors {
                if visited.contains(&nbr_id) {
                    continue;
                }
                visited.insert(nbr_id.clone());
                if let Some(s) = id_map.get(&nbr_id) {
                    let t = symbol_tier(&s.file);
                    let l = classify_layer_sym(&s.file, &s.qname, t, &layer_overrides);
                    let row = serde_json::json!({
                        "qname": s.qname, "file": s.file, "line": s.start.line,
                        "depth": depth + 1, "layer": l,
                    });
                    if t == 2 {
                        affected_test_rows.push(row);
                    } else {
                        caller_rows.push(row);
                    }
                    if seen_files.insert(s.file.clone()) {
                        touched_files.push((s.file.clone(), depth + 1));
                    }
                    if depth + 1 < max_depth {
                        queue.push_back((nbr_id, depth + 1));
                    }
                }
            }
        }

        // Aggregate invariants/hazards/effects from seeds.
        let mut all_invariants: Vec<serde_json::Value> = Vec::new();
        let mut all_hazards: Vec<serde_json::Value> = Vec::new();
        let mut all_effects: Vec<serde_json::Value> = Vec::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        for sym_id in &seed_ids {
            let entries = ledger_store
                .list_entries(&ref_name, sym_id)
                .unwrap_or_default();
            let sym_qname = id_map.get(sym_id).map(|s| s.qname.as_str()).unwrap_or("");
            for entry in entries {
                let key = entry.summary.clone();
                match entry.kind {
                    LedgerKind::Invariant => {
                        if seen_inv.insert(key) {
                            all_invariants.push(serde_json::json!({ "summary": entry.summary, "source": sym_qname }));
                        }
                    }
                    LedgerKind::Hazard => {
                        all_hazards.push(
                            serde_json::json!({ "summary": entry.summary, "source": sym_qname }),
                        );
                    }
                    _ => {}
                }
            }
            if let Ok(Some(decl)) = effect_store.get_effects(&ref_name, sym_id) {
                let qn = id_map
                    .get(sym_id)
                    .map(|s| s.qname.clone())
                    .unwrap_or_default();
                for eff in &decl.declared {
                    all_effects.push(serde_json::json!({ "category": format!("{:?}", eff.effect), "source": qn }));
                }
            }
        }

        let git_depth = p.git_depth.max(1) as usize;
        let recently_touched =
            mcp_git_recent_touches(&touched_files[..touched_files.len().min(5)], git_depth);

        serde_json::to_string(&serde_json::json!({
            "sha": p.sha,
            "changed_files": changed_files,
            "touched_symbols": by_layer,
            "caller_count": caller_rows.len(),
            "test_count": affected_test_rows.len(),
            "callers": caller_rows,
            "affected_tests": affected_test_rows,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "effects": all_effects,
            "recently_touched": recently_touched,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Record an invariant that must hold at a symbol. Shortcut for `ledger_append` with kind=invariant. Invariants appear in investigate, checklist, and prepare_change outputs — record them here so future agents see them."
    )]
    async fn invariant_add(&self, params: Parameters<InvariantAddParams>) -> String {
        let p = params.0;
        let Ok(engine) = Engine::open_sqlite(&self.db_path()) else {
            return err_json("failed to open database");
        };
        let index_store = AsgIndexStore::from_engine(&engine);
        let Ok(Some(symbol)) = index_store.get_symbol_by_qname(&engine.ref_name, &p.qname) else {
            return err_json(&format!("symbol not found: {}", p.qname));
        };
        let author = Author {
            kind: AuthorKind::Agent,
            id: p.author_id.clone(),
        };
        let entry = LedgerEntry::new(
            &symbol.symbol_id,
            LedgerKind::Invariant,
            p.summary.clone(),
            author,
        );
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        match ledger_store.append_entry(&engine.ref_name, &entry, "asd-mcp") {
            Ok(_) => serde_json::to_string(&serde_json::json!({
                "status": "added",
                "entry_id": entry.entry_id,
                "symbol_id": entry.symbol_id,
                "qname": p.qname,
                "summary": p.summary,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List invariants recorded against symbols. Pass qname to filter to one symbol; omit to list all invariants in the index."
    )]
    async fn invariant_list(&self, params: Parameters<InvariantListParams>) -> String {
        let p = params.0;
        let Ok(engine) = Engine::open_sqlite(&self.db_path()) else {
            return err_json("failed to open database");
        };
        let ledger_store = AsgLedgerStore::from_engine(&engine);

        let rows: Vec<serde_json::Value> = if let Some(qname) = p.qname {
            let index_store = AsgIndexStore::from_engine(&engine);
            match index_store.get_symbol_by_qname(&engine.ref_name, &qname) {
                Ok(Some(symbol)) => ledger_store
                    .list_entries(&engine.ref_name, &symbol.symbol_id)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e.kind == LedgerKind::Invariant)
                    .map(|e| {
                        serde_json::json!({
                            "entry_id": e.entry_id,
                            "qname": qname,
                            "summary": e.summary,
                            "created_at": e.created_at,
                            "tags": e.tags,
                        })
                    })
                    .collect(),
                _ => return err_json(&format!("symbol not found: {}", qname)),
            }
        } else {
            let ref_name = &engine.ref_name;
            let tree = match engine.repo.get_tree(ref_name, "/asd/v1/ledger") {
                Ok(v) => v,
                _ => {
                    return serde_json::to_string(&serde_json::json!({ "invariants": [] }))
                        .unwrap_or_else(|_| "{}".to_string());
                }
            };
            let index_store_all = AsgIndexStore::from_engine(&engine);
            let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
            let all_qnames: Vec<String> = match engine.repo.get_tree(ref_name, &prefix) {
                Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
                _ => vec![],
            };
            let mut id_map: std::collections::HashMap<String, agentstatedeveloper_core::Symbol> =
                std::collections::HashMap::new();
            for qn in &all_qnames {
                if let Ok(Some(s)) = index_store_all.get_symbol_by_qname(ref_name, qn) {
                    id_map.insert(s.symbol_id.clone(), s);
                }
            }
            let mut rows: Vec<serde_json::Value> = Vec::new();
            if let Some(sym_map) = tree.as_object() {
                for per_symbol in sym_map.values() {
                    if let Some(entry_map) = per_symbol.as_object() {
                        for entry_val in entry_map.values() {
                            if let Ok(e) = serde_json::from_value::<LedgerEntry>(entry_val.clone())
                            {
                                if e.kind == LedgerKind::Invariant {
                                    let qname = id_map
                                        .get(&e.symbol_id)
                                        .map(|s| s.qname.as_str())
                                        .unwrap_or("");
                                    rows.push(serde_json::json!({
                                        "entry_id": e.entry_id,
                                        "qname": qname,
                                        "summary": e.summary,
                                        "created_at": e.created_at,
                                        "tags": e.tags,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            rows.sort_by(|a, b| {
                a.get("qname")
                    .and_then(serde_json::Value::as_str)
                    .cmp(&b.get("qname").and_then(serde_json::Value::as_str))
            });
            rows
        };

        serde_json::to_string(&serde_json::json!({ "invariants": rows }))
            .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Record a verdict on a search result. Verdicts: useful (good match), noisy (irrelevant), missing (should have appeared), wrong_layer (architectural misclassification), already_covered (this symbol's behavior is covered by another — Plan C t-005, also surface a Mapping ledger entry), diagnostic_only (this symbol is a diagnostic/instrumentation test — Plan C t-005, also surface a Classification entry with role=diagnostic-test). Persisted and applied as score adjustments in future searches."
    )]
    async fn feedback_mark(&self, params: Parameters<FeedbackMarkParams>) -> String {
        let p = params.0;
        // Plan C t-005: delegate to FeedbackVerdict::from_str so the
        // taxonomy stays single-sourced in core.
        let verdict = match FeedbackVerdict::from_str(&p.verdict) {
            Some(v) => v,
            None => {
                return err_json(&format!(
                    "unknown verdict {:?}; valid: useful, noisy, missing, wrong_layer, already_covered, diagnostic_only",
                    p.verdict
                ));
            }
        };
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore::from_engine(&engine);
        let symbol = match index_store.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let entry_id = format!("fb_{}", uuid::Uuid::new_v4().simple());
        let now = chrono::Utc::now();
        let expires_at = p.ttl_days.map(|days| now + chrono::Duration::days(days));
        let entry = FeedbackEntry {
            entry_id: entry_id.clone(),
            symbol_id: symbol.symbol_id.clone(),
            symbol_qname: p.qname.clone(),
            query: p.query.to_lowercase().trim().to_string(),
            verdict,
            note: p.note.clone(),
            author: p.author_id.clone(),
            created_at: now,
            file_scope: None,
            expires_at,
        };
        let feedback_store = AsgFeedbackStore::from_engine(&engine);
        if let Err(e) = feedback_store.record(&ref_name, &entry, &p.author_id) {
            return err_json(&e.to_string());
        }

        // Plan E t-009: auto-write paired ledger entries so the
        // verdict's intent is durable, not just a per-query verdict.
        let mut paired_kind: Option<&'static str> = None;
        let author_struct = Author {
            kind: AuthorKind::Agent,
            id: p.author_id.clone(),
        };
        let ledger_store = AsgLedgerStore::from_engine(&engine);

        if matches!(verdict, FeedbackVerdict::AlreadyCovered) {
            let cover = match p.covered_by.as_deref() {
                Some(c) if !c.is_empty() => c,
                _ => return err_json("covered_by is required when verdict=already_covered"),
            };
            let body = serde_json::json!({
                "from_qname": &p.qname,
                "to_qname": cover,
                "source": "feedback-pair",
            })
            .to_string();
            let mut led = LedgerEntry::new(
                &symbol.symbol_id,
                LedgerKind::Mapping,
                format!("covered by {cover}"),
                author_struct.clone(),
            );
            led.body = Some(body);
            led.tags.push("plan-e:t-009".into());
            if let Err(e) = ledger_store.append_entry(&ref_name, &led, &p.author_id) {
                return err_json(&e.to_string());
            }
            paired_kind = Some("mapping");
        } else if matches!(verdict, FeedbackVerdict::DiagnosticOnly) {
            let mut led = LedgerEntry::new(
                &symbol.symbol_id,
                LedgerKind::Ownership,
                format!("diagnostic-only: {}", p.query),
                author_struct.clone(),
            );
            led.role = Some("diagnostic-test".to_string());
            led.tags.push("plan-e:t-009".into());
            if let Err(e) = ledger_store.append_entry(&ref_name, &led, &p.author_id) {
                return err_json(&e.to_string());
            }
            paired_kind = Some("classification");
        }

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "entry_id": entry_id,
            "verdict": p.verdict,
            "qname": p.qname,
            "paired_ledger": paired_kind,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Designate a symbol as the canonical source-of-truth for a domain concept. Writes an Ownership ledger entry (3x ranking boost) so future searches for that concept reliably surface this symbol. Use when you know which function/struct truly owns a concept."
    )]
    async fn feedback_promote(&self, params: Parameters<FeedbackPromoteParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore::from_engine(&engine);
        let symbol = match index_store.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let author_kind = if p.author_id.contains("human") {
            AuthorKind::Human
        } else {
            AuthorKind::Agent
        };
        let mut entry = LedgerEntry::new(
            &symbol.symbol_id,
            LedgerKind::Ownership,
            &p.concept,
            Author {
                kind: author_kind,
                id: p.author_id.clone(),
            },
        );
        entry.tags = vec!["promote-as-truth".to_string()];
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        match ledger_store.append_entry(&ref_name, &entry, &p.author_id) {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true,
                "entry_id": entry.entry_id,
                "qname": p.qname,
                "concept": p.concept,
                "kind": "ownership",
            }))
            .unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List recorded feedback verdicts. Pass qname to filter to one symbol; omit to list all. Use this to audit search quality signals or review past verdicts."
    )]
    async fn feedback_list(&self, params: Parameters<FeedbackListParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let feedback_store = AsgFeedbackStore::from_engine(&engine);
        let entries: Vec<serde_json::Value> = if let Some(ref qname) = p.qname {
            let index_store = AsgIndexStore::from_engine(&engine);
            match index_store.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => feedback_store
                    .list_for_symbol(&ref_name, &sym.symbol_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| serde_json::to_value(&e).unwrap_or_default())
                    .collect(),
                Ok(None) => return err_json(&format!("symbol not found: {}", qname)),
                Err(e) => return err_json(&e.to_string()),
            }
        } else {
            feedback_store
                .list_all(&ref_name)
                .unwrap_or_default()
                .into_iter()
                .map(|e| serde_json::to_value(&e).unwrap_or_default())
                .collect()
        };
        serde_json::to_string(&serde_json::json!({ "feedback": entries }))
            .unwrap_or_else(|_| "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // Tool 1: search — Full agent-quality ranked search
    // -----------------------------------------------------------------------

    #[tool(
        description = "Full ranked symbol search with confidence, uncertainty model, feedback adjustments, and agent-quality output. Prefer this over code_search for agent workflows."
    )]
    async fn search(&self, params: Parameters<SearchParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let layer_overrides = load_layer_overrides(&db_path);
        let intent = p.intent.as_deref().and_then(parse_intent).unwrap_or("");
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let dq_state_str: String = {
            let trust = compute_trust_score(&db_path);
            trust.data_quality.state.clone()
        };

        let all_feedback: Vec<agentstatedeveloper_core::FeedbackEntry> = {
            let fb_store = AsgFeedbackStore::from_engine(&engine);
            // Plan J t-014: expired entries don't influence ranking;
            // storage is preserved so users can audit via feedback_list.
            fb_store
                .list_all(&ref_name)
                .unwrap_or_default()
                .into_iter()
                .filter(|e| !e.is_expired())
                .collect()
        };

        let (tokens, mut inline_exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
            {
                inline_exclusions.push(term);
            }
        }

        if tokens.is_empty() {
            return serde_json::json!({"query": p.query, "results": [], "document_hits": []})
                .to_string();
        }

        let limit = p.limit.max(1) as usize;
        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref scope) = p.scope {
            paths_filter.extend(resolve_scope(scope, &db_path));
        }
        if let Some(ref paths_str) = p.paths {
            paths_filter.extend(
                paths_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            tests_only: false,
            exclude_terms: inline_exclusions.clone(),
            paths_filter: paths_filter.clone(),
            exclude_paths: vec![],
            exclude_languages: vec![],
        };

        let doc_hits: Vec<serde_json::Value> = if !p.symbols_only {
            SearchDocsDb::open(&db_path)
                .ok()
                .filter(|db| !db.is_empty())
                .and_then(|db| db.search(&tokens, limit, None).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "source": "document",
                        "score": h.bm25_score,
                        "kind": h.kind,
                        "path": h.path,
                        "line": h.span_start,
                        "title": h.title,
                        "preview": h.preview,
                        "owner_symbol_id": h.owner_symbol_id,
                    })
                })
                .collect()
        } else {
            vec![]
        };

        let fts_result = engine
            .fts
            .as_ref()
            .filter(|fts| fts.has_data())
            .and_then(|fts| fts.search(&p.query, &filters, limit * 2).ok());

        if let Some(hits) = fts_result {
            let ledger_store = AsgLedgerStore::from_engine(&engine);
            let effect_store = AsgEffectStore::from_engine(&engine);
            let index_store = AsgIndexStore::from_engine(&engine);

            let mut has_ledger_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            const GENERIC_BOOST_SKIP: &[&str] = &[
                "state",
                "update",
                "position",
                "value",
                "cursor",
                "progress",
                "indicator",
                "status",
                "mode",
                "flag",
                "current",
                "local",
                "playhead",
                "tick",
                "item",
                "data",
                "info",
                "manager",
            ];
            let mut scored: Vec<(f64, _)> = {
                let mut tmp = Vec::with_capacity(hits.len());
                for hit in hits {
                    let hybrid = hybrid_boost(&hit, &tokens);
                    let ledger_boost = if hit.ledger_text.is_empty() {
                        0.0
                    } else {
                        tokens
                            .iter()
                            .filter(|t| hit.ledger_text.contains(t.as_str()))
                            .count() as f64
                    };
                    let haystack =
                        format!("{} {}", hit.qname.to_lowercase(), hit.file.to_lowercase());
                    let domain_overlap = tokens
                        .iter()
                        .filter(|t| !GENERIC_BOOST_SKIP.contains(&t.as_str()))
                        .filter(|t| haystack.contains(t.as_str()))
                        .count();
                    let has_ownership = hit.has_ownership();
                    let has_invariant = hit.has_invariant();
                    if hit.has_ledger() {
                        has_ledger_ids.insert(hit.symbol_id.clone());
                    }
                    let is_state_holder =
                        matches!(hit.kind.as_str(), "class" | "struct" | "type" | "enum")
                            && !has_ownership
                            && !has_invariant
                            && !tokens.iter().any(|t| {
                                matches!(
                                    t.as_str(),
                                    "state"
                                        | "model"
                                        | "type"
                                        | "class"
                                        | "struct"
                                        | "enum"
                                        | "schema"
                                )
                            });
                    let state_penalty = if is_state_holder { -0.8 } else { 0.0 };
                    let sot_boost = if has_ownership && domain_overlap >= 2 {
                        5.0
                    } else if has_ownership && domain_overlap >= 1 {
                        3.5
                    } else if has_ownership {
                        2.0
                    } else if has_invariant && domain_overlap >= 1 {
                        1.5
                    } else {
                        0.0
                    };
                    let total = hit.bm25_score + hybrid + ledger_boost + sot_boost + state_penalty;
                    tmp.push((total, hit));
                }
                tmp
            };

            // Apply feedback adjustments
            let mut feedback_metrics = agentstatedeveloper_core::FeedbackMetrics::default();
            {
                if !all_feedback.is_empty() {
                    // Plan J t-016: tuple gained created_at for age-decay.
                    let fb_tuples: Vec<_> = all_feedback
                        .iter()
                        .filter(|e| e.file_scope.is_none())
                        .map(|e| {
                            (
                                e.symbol_id.clone(),
                                e.query.clone(),
                                e.verdict,
                                e.created_at,
                            )
                        })
                        .collect();
                    let fs_tuples: Vec<_> = all_feedback
                        .iter()
                        .filter_map(|e| {
                            e.file_scope
                                .as_ref()
                                .map(|g| (g.clone(), e.verdict, e.query.clone(), e.created_at))
                        })
                        .collect();
                    let mut adj: Vec<(f64, String)> =
                        scored.iter().map(|(s, h)| (*s, h.qname.clone())).collect();
                    feedback_metrics = apply_feedback_adjustments(
                        &engine,
                        &index_store,
                        &p.query,
                        &mut adj,
                        &fb_tuples,
                    );
                    apply_file_scope_feedback(
                        &engine,
                        &index_store,
                        &p.query,
                        &mut adj,
                        &fs_tuples,
                    );
                    let adj_map: std::collections::HashMap<String, f64> =
                        adj.into_iter().map(|(s, q)| (q, s)).collect();
                    scored.retain(|(_, h)| adj_map.contains_key(&h.qname));
                    for (score, h) in scored.iter_mut() {
                        if let Some(&new_s) = adj_map.get(&h.qname) {
                            *score = new_s;
                        }
                    }
                }
            }

            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.qname.cmp(&b.1.qname))
            });
            scored.truncate(limit);

            let recency = gather_recency(200, 14.0);
            let raw_scores: Vec<f64> = scored.iter().map(|(s, _)| *s).collect();
            let confidences = confidence_scores(&raw_scores);
            let ambiguous_terms = detect_ambiguous_tokens(&tokens, engine.fts.as_ref(), &filters);

            let all_result_qnames: Vec<String> =
                scored.iter().map(|(_, h)| h.qname.clone()).collect();
            let all_feedback_impacts = explain_feedback_impacts(
                &engine,
                &index_store,
                &p.query,
                &all_result_qnames,
                &all_feedback,
            );

            let mut layers_present: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut ledger_cache: std::collections::HashMap<String, Vec<LedgerEntry>> =
                std::collections::HashMap::new();
            let results: Vec<serde_json::Value> = scored.iter().zip(confidences.iter()).map(|((score, hit), conf)| {
                let rec = recency.get(&hit.file);
                let is_hot = rec.map(|r| r.hot).unwrap_or(false);
                let tier = symbol_tier(&hit.file);
                let layer = classify_layer_sym(&hit.file, &hit.qname, tier, &layer_overrides);
                layers_present.insert(layer.to_string());
                if !ledger_cache.contains_key(&hit.symbol_id) {
                    if let Ok(entries) = ledger_store.list_entries(&ref_name, &hit.symbol_id) {
                        ledger_cache.insert(hit.symbol_id.clone(), entries);
                    }
                }
                let ledger_entries = ledger_cache.get(&hit.symbol_id).cloned().unwrap_or_default();
                let has_ledger = has_ledger_ids.contains(&hit.symbol_id) || !ledger_entries.is_empty();
                let match_reasons = if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&ref_name, &hit.qname) {
                    explain_match(&sym, &tokens, &ledger_entries, is_hot)
                } else { vec![] };
                let bucket = result_bucket(&hit.file, &match_reasons, has_ledger, is_hot);
                let conf_reason = confidence_reason(&match_reasons, has_ledger, is_hot);
                let fb_status = {
                    let q = p.query.to_lowercase();
                    all_feedback.iter().find(|e| {
                        e.symbol_id == hit.symbol_id
                            && (e.query.is_empty() || q.contains(e.query.as_str()) || e.query.contains(q.as_str()))
                    }).map(|e| e.verdict.as_str().to_string())
                };
                let feedback_rule: Option<serde_json::Value> = all_feedback_impacts.get(&hit.qname).map(|imp| {
                    serde_json::json!({"verdict": imp.verdict, "matched_query": imp.matched_query, "author": imp.author})
                });
                let effect_detail = {
                    let decl = effect_store.get_effects(&ref_name, &hit.symbol_id).ok().flatten();
                    effect_detail_reason(decl.as_ref())
                };
                serde_json::json!({
                    "score": score, "confidence": conf, "bucket": bucket,
                    "confidence_reason": conf_reason,
                    "qname": hit.qname, "kind": hit.kind,
                    "file": hit.file, "line": hit.line, "layer": layer,
                    "summary": extract_summary(hit.doc.as_deref(), hit.signature.as_deref()),
                    "last_touched_days": rec.and_then(|r| r.last_touched_days),
                    "hot": is_hot,
                    "match_reasons": match_reasons,
                    "feedback_status": fb_status,
                    "feedback_rule": feedback_rule,
                    "effect_detail": effect_detail,
                })
            }).collect();

            let scope_narrowed =
                !filters.paths_filter.is_empty() || !filters.exclude_terms.is_empty();
            let layers_ref: std::collections::HashSet<&str> =
                layers_present.iter().map(|s| s.as_str()).collect();
            let possible_misses = if scope_narrowed {
                vec![]
            } else {
                detect_possible_misses(&p.query, &layers_ref, results.len())
            };
            let confidence_warnings = detect_confidence_warnings(
                &tokens,
                results.len(),
                &ambiguous_terms,
                engine.fts.as_ref(),
            );
            let query_suggestions = if scope_narrowed {
                vec![]
            } else {
                suggest_better_queries(&tokens, &p.query)
            };
            let top_qnames: Vec<String> = results
                .iter()
                .take(5)
                .filter_map(|r| r["qname"].as_str().map(|s| s.to_string()))
                .collect();
            let scoped_suggestions = if scope_narrowed || ambiguous_terms.is_empty() {
                vec![]
            } else {
                suggest_scoped_queries(&tokens, &ambiguous_terms, &top_qnames)
            };
            let uncertainty = compute_uncertainty(
                &tokens,
                &ambiguous_terms,
                &possible_misses,
                results.len(),
                &scoped_suggestions,
                engine.fts.as_ref(),
                Some(dq_state_str.as_str()),
            );
            let feedback_state = build_feedback_state_from_entries(
                &all_feedback,
                &p.query,
                feedback_metrics.entries_applied,
            );
            let raw = serde_json::json!({
                "query": p.query,
                "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
                "uncertainty": uncertainty.to_json(),
                "ambiguous_terms": ambiguous_terms,
                "possible_misses": possible_misses,
                "confidence_warnings": confidence_warnings,
                "query_suggestions": query_suggestions,
                "scoped_suggestions": scoped_suggestions,
                "scope_narrowed": scope_narrowed,
                "feedback_state": feedback_state.to_json(),
                "results": results,
                "document_hits": doc_hits,
            });
            let max_list = (p.agent_budget as usize / 500).max(3).min(20);
            let trimmed = trim_for_agent(&raw, max_list);
            let json_str = serde_json::to_string(&trimmed).unwrap_or_default();
            let token_est = estimate_tokens(&json_str);
            let mut out = trimmed.clone();
            if let Some(obj) = out.as_object_mut() {
                obj.insert("token_estimate".into(), serde_json::json!(token_est));
            }
            return serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string());
        }

        // Fallback: in-memory scoring
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => return serde_json::json!({"query": p.query, "results": [], "document_hits": doc_hits}).to_string(),
        };
        let index = AsgIndexStore::from_engine(&engine);
        let mut scored: Vec<(u32, Symbol)> = Vec::new();
        for qname in &qnames {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            if let Some(ref k) = filters.kind {
                let sk = format!("{:?}", sym.kind).to_lowercase();
                if &sk != k {
                    continue;
                }
            }
            if let Some(ref lang) = filters.language {
                if &sym.language != lang {
                    continue;
                }
            }
            let qn = sym.qname.to_lowercase();
            let sig = sym.signature.as_deref().unwrap_or("").to_lowercase();
            let doc = sym.doc.as_deref().unwrap_or("").to_lowercase();
            let file = sym.file.to_lowercase();
            let ledger_text: String = ledger_store
                .list_entries(&ref_name, &sym.symbol_id)
                .unwrap_or_default()
                .iter()
                .map(|e| e.summary.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            let mut score: u32 = 0;
            for token in &tokens {
                if qn.contains(token.as_str()) {
                    score += 4;
                }
                if !sig.is_empty() && sig.contains(token.as_str()) {
                    score += 3;
                }
                if !doc.is_empty() && doc.contains(token.as_str()) {
                    score += 3;
                }
                if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) {
                    score += 2;
                }
                if file.contains(token.as_str()) {
                    score += 1;
                }
            }
            if score > 0 {
                scored.push((score, sym));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.qname.cmp(&b.1.qname)));
        scored.truncate(limit);
        let recency = gather_recency(200, 14.0);
        let results: Vec<serde_json::Value> = scored
            .iter()
            .map(|(score, sym)| {
                let tier = symbol_tier(&sym.file);
                let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
                let rec = recency.get(&sym.file);
                serde_json::json!({
                    "score": score, "qname": sym.qname,
                    "kind": format!("{:?}", sym.kind).to_lowercase(),
                    "file": sym.file, "line": sym.start.line,
                    "tier": tier, "layer": layer,
                    "summary": extract_summary(sym.doc.as_deref(), sym.signature.as_deref()),
                    "last_touched_days": rec.and_then(|r| r.last_touched_days),
                    "hot": rec.map(|r| r.hot).unwrap_or(false),
                })
            })
            .collect();
        // Token economy (1.0.80): MCP is always agent-consumed.
        // Drop input echo (p.query) and any empty top-level fields.
        let raw =
            serde_json::json!({"results": results, "document_hits": doc_hits});
        let raw = agentstatedeveloper_core::drop_empty_top_level(raw);
        serde_json::to_string(&raw).unwrap_or_else(|_| "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // Tool 2: context_for — Deep per-symbol context
    // -----------------------------------------------------------------------

    #[tool(
        description = "Assemble deep per-symbol context: signature, callers/callees, effects, all ledger entries (invariants, hazards, decisions), covering tests, and ownership discovery. Pass comma-separated qnames."
    )]
    async fn context_for(&self, params: Parameters<ContextForParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        let id_map = index_store.build_id_map(&engine);

        let qnames: Vec<&str> = p
            .qnames
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let budget_chars = p.budget_tokens.map(|t| t as usize * 4);
        let include_body = p.include_body;

        let mut symbols_out = Vec::new();
        for qname in &qnames {
            let symbol = match index_store.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    symbols_out
                        .push(serde_json::json!({ "qname": qname, "error": "symbol not found" }));
                    continue;
                }
                Err(e) => {
                    symbols_out.push(serde_json::json!({ "qname": qname, "error": e.to_string() }));
                    continue;
                }
            };

            // Callers/callees
            let callee_ids = index_store
                .get_callees(&ref_name, &symbol.symbol_id)
                .unwrap_or_default();
            let caller_ids = index_store
                .get_callers(&ref_name, &symbol.symbol_id)
                .unwrap_or_default();
            let resolve = |ids: &[String]| -> Vec<serde_json::Value> {
                ids.iter().map(|id| {
                    if let Some(s) = id_map.get(id) {
                        serde_json::json!({ "qname": s.qname, "file": s.file, "line": s.start.line })
                    } else {
                        serde_json::json!({ "symbol_id": id })
                    }
                }).collect()
            };

            // Effects
            let effects = effect_store
                .get_effects(&ref_name, &symbol.symbol_id)
                .unwrap_or(None);

            // Ledger — grouped by kind
            let ledger = ledger_store
                .list_entries(&ref_name, &symbol.symbol_id)
                .unwrap_or_default();
            let mut invariants: Vec<serde_json::Value> = Vec::new();
            let mut hazards: Vec<serde_json::Value> = Vec::new();
            let mut ownership: Vec<serde_json::Value> = Vec::new();
            let mut proofs: Vec<serde_json::Value> = Vec::new();
            let mut validation_scenarios: Vec<serde_json::Value> = Vec::new();
            let mut known_bugs: Vec<serde_json::Value> = Vec::new();
            let mut concepts: Vec<serde_json::Value> = Vec::new();
            let mut other_ledger: Vec<serde_json::Value> = Vec::new();
            for entry in &ledger {
                let v = serde_json::to_value(entry).unwrap_or_default();
                match entry.kind {
                    LedgerKind::Invariant => invariants.push(v),
                    LedgerKind::Hazard => hazards.push(v),
                    LedgerKind::Ownership => ownership.push(v),
                    LedgerKind::Proof => proofs.push(v),
                    LedgerKind::ValidationScenario => validation_scenarios.push(v),
                    LedgerKind::KnownBug => known_bugs.push(v),
                    LedgerKind::Concept => concepts.push(v),
                    _ => other_ledger.push(v),
                }
            }

            // Symbol value (without body if !include_body)
            let mut sym_val = serde_json::to_value(&symbol).unwrap_or_default();
            if !include_body {
                if let Some(obj) = sym_val.as_object_mut() {
                    obj.remove("body");
                }
            }

            // Ownership discovery
            let ownership_signal = discover_symbol_ownership(
                &symbol.file,
                symbol.start.line,
                symbol.end.line,
                symbol.doc.as_deref(),
            );
            let mut discovered_ownership: serde_json::Map<String, serde_json::Value> =
                serde_json::Map::new();
            if let Some(ref author) = ownership_signal.primary_author {
                discovered_ownership.insert("primary_author".into(), serde_json::json!(author));
            }
            if let Some(ref doc_owner) = ownership_signal.doc_owner {
                discovered_ownership.insert("doc_owner".into(), serde_json::json!(doc_owner));
            }
            if !ownership_signal.recent_committers.is_empty() {
                discovered_ownership.insert(
                    "recent_committers".into(),
                    serde_json::json!(ownership_signal.recent_committers),
                );
            }
            if !ownership_signal.annotated.is_empty() {
                let annotated_val: Vec<serde_json::Value> = ownership_signal.annotated.iter().map(|a| {
                    serde_json::json!({ "name": a.name, "source": serde_json::to_value(a.source).unwrap_or(serde_json::json!("unknown")) })
                }).collect();
                discovered_ownership.insert("annotated".into(), serde_json::json!(annotated_val));
            }

            // Covering tests
            let covering_tests: Vec<serde_json::Value> = find_covering_tests(engine.fts.as_ref(), &symbol.qname)
                .into_iter().map(|ct| serde_json::json!({
                    "qname": ct.qname, "file": ct.file, "line": ct.line, "run_command": ct.run_command,
                })).collect();

            // Effects detail
            let effects_detail: Vec<serde_json::Value> = if let Some(ref decl) = effects {
                let mismatch_effects: std::collections::HashSet<String> = decl
                    .verification
                    .as_ref()
                    .map(|v| {
                        v.mismatches
                            .iter()
                            .map(|m| m.effect.as_str().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let overall_ok = decl
                    .verification
                    .as_ref()
                    .map(|v| matches!(v.status, VerificationStatus::Ok))
                    .unwrap_or(false);
                decl.declared
                    .iter()
                    .map(|e| {
                        let effect_str = e.effect.as_str();
                        let is_mismatched = mismatch_effects.contains(effect_str);
                        let status = if decl.verification.is_none() {
                            "unverified"
                        } else if is_mismatched {
                            "mismatch"
                        } else if overall_ok {
                            "ok"
                        } else {
                            "ok"
                        };
                        let mut obj = serde_json::Map::new();
                        obj.insert("effect".into(), serde_json::json!(effect_str));
                        obj.insert("status".into(), serde_json::json!(status));
                        if let Some(ref adapter) = e.adapter {
                            obj.insert("adapter".into(), serde_json::json!(adapter));
                        }
                        if let Some(ref pattern) = e.source_pattern {
                            obj.insert("source_pattern".into(), serde_json::json!(pattern));
                        }
                        if let Some(note) = &e.note {
                            obj.insert("note".into(), serde_json::json!(note));
                        }
                        serde_json::Value::Object(obj)
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let sym_ctx = serde_json::json!({
                "symbol": sym_val,
                "invariants": invariants,
                "hazards": hazards,
                "known_bugs": known_bugs,
                "concepts": concepts,
                "ownership": ownership,
                "ownership_discovery": discovered_ownership,
                "covering_tests": covering_tests,
                "validation_scenarios": validation_scenarios,
                "callers": resolve(&caller_ids),
                "callees": resolve(&callee_ids),
                "effects": effects,
                "effects_detail": effects_detail,
                "proofs": proofs,
                "decisions_and_notes": other_ledger,
            });
            symbols_out.push(sym_ctx);
        }

        // Plan G t-006: surface captured thinking across the requested qnames.
        // p.qnames is a comma-separated String per the existing API.
        let qnames_vec: Vec<String> = p
            .qnames
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // ExampleFlow refinement: thinking_summary always emits, even
        // when prior_thinking is Null — load-bearing signal for agents
        // ("entries exist but filtered" vs "no entries on these symbols").
        let pt = thinking::gather_prior_thinking(
            &engine,
            &qnames_vec,
            thinking::DEFAULT_CONFIDENCE_FLOOR,
        );

        // Plan F t-006: brief projects each per-symbol context down to the
        // compact shape (symbol{qname,file,signature,doc} + capped callers/
        // callees + effects categories + ledger_count). The outer envelope
        // stays the same.
        let symbols_projected: Vec<serde_json::Value> = if brief::brief_from_env() {
            symbols_out.iter().map(brief::brief_context_for).collect()
        } else {
            symbols_out
        };

        // Token economy (1.0.80): drop input echo `query`. The MCP
        // client just sent `p.qnames`; echoing it costs ~50-200
        // chars depending on the qname list size.
        let mut out_map = serde_json::Map::new();
        out_map.insert("count".into(), serde_json::json!(symbols_projected.len()));
        out_map.insert("symbols".into(), serde_json::json!(symbols_projected));
        if !pt.entries.is_null() {
            out_map.insert("prior_thinking".into(), pt.entries);
        }
        out_map.insert(
            "thinking_summary".into(),
            serde_json::to_value(&pt.summary).unwrap_or(serde_json::Value::Null),
        );
        let out = agentstatedeveloper_core::drop_empty_top_level(
            serde_json::Value::Object(out_map),
        );

        let mut output = serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string());
        if let Some(max_chars) = budget_chars {
            if output.len() > max_chars {
                let mut v = out.clone();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "_budget_warning".into(),
                        serde_json::json!(
                            "output exceeds budget; pass fewer qnames or increase budget_tokens"
                        ),
                    );
                }
                output = serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string());
            }
        }
        output
    }

    // -----------------------------------------------------------------------
    // Tool 3: task_close — Workflow task closure
    // -----------------------------------------------------------------------

    #[tool(
        description = "Close a task: write Proof (and optional ValidationScenario) ledger entries for affected symbols. Resolves symbols from git HEAD changed files when symbols is omitted."
    )]
    async fn task_close(&self, params: Parameters<TaskCloseParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);

        let plan_id = p.plan.clone().unwrap_or_default();
        let task_id = p.task.clone().unwrap_or_default();

        let mut ctx_tags: Vec<String> = Vec::new();
        if !plan_id.is_empty() {
            ctx_tags.push(format!("ctx:plan:{}", plan_id));
        }
        if !task_id.is_empty() {
            ctx_tags.push(format!("ctx:task:{}", task_id));
        }

        // Resolve symbols
        let target_symbols: Vec<Symbol> = if let Some(ref sym_list) = p.symbols {
            sym_list
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .filter_map(|q| index_store.get_symbol_by_qname(&ref_name, q).ok().flatten())
                .collect()
        } else {
            let out = Proc::new("git")
                .args(["diff-tree", "--no-commit-id", "-r", "--name-only", "HEAD"])
                .output()
                .unwrap_or_else(|_| std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: vec![],
                    stderr: vec![],
                });
            let changed: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let tree = engine
                .repo
                .get_tree(&ref_name, "/asd/v1/index/by-qname")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let mut syms: Vec<Symbol> = tree
                .as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                        .filter(|s| {
                            changed
                                .iter()
                                .any(|f| s.file.ends_with(f.as_str()) || s.file == *f)
                        })
                        .collect()
                })
                .unwrap_or_default();
            syms.truncate(20);
            syms
        };

        if target_symbols.is_empty() {
            return serde_json::to_string(&serde_json::json!({
                "written": [],
                "note": "no symbols resolved — pass symbols or ensure HEAD has changed files",
                "ctx": {"plan": plan_id, "task": task_id},
            }))
            .unwrap_or_else(|_| "{}".to_string());
        }

        let proof_base = p
            .proof
            .clone()
            .unwrap_or_else(|| "task completed".to_string());
        let proof_text = if let Some(ref ev) = p.evidence {
            format!("{} [evidence: {}]", proof_base, ev)
        } else {
            proof_base.clone()
        };

        let author = Author {
            kind: AuthorKind::Human,
            id: p.author.clone(),
        };
        let mut written: Vec<serde_json::Value> = Vec::new();
        let closed_at = chrono::Utc::now().to_rfc3339();

        for sym in &target_symbols {
            let mut proof_entry = LedgerEntry::new(
                &sym.symbol_id,
                LedgerKind::Proof,
                &proof_text,
                author.clone(),
            );
            proof_entry.tags.extend(ctx_tags.iter().cloned());
            if let Some(ref ev) = p.evidence {
                proof_entry.tags.push(format!("evidence:{}", ev));
            }
            match ledger_store.append_entry(&ref_name, &proof_entry, &p.author) {
                Ok(_) => written.push(serde_json::json!({"symbol": sym.qname, "kind": "proof", "summary": proof_text})),
                Err(e) => return err_json(&format!("ledger write failed for {}: {}", sym.qname, e)),
            }

            if p.validated {
                let validation_text = p
                    .validation_note
                    .clone()
                    .unwrap_or_else(|| format!("validated: {}", proof_base));
                let mut vs_entry = LedgerEntry::new(
                    &sym.symbol_id,
                    LedgerKind::ValidationScenario,
                    &validation_text,
                    author.clone(),
                );
                vs_entry.tags.extend(ctx_tags.iter().cloned());
                if let Ok(_) = ledger_store.append_entry(&ref_name, &vs_entry, &p.author) {
                    written.push(serde_json::json!({"symbol": sym.qname, "kind": "validation_scenario", "summary": validation_text}));
                }
            }
        }

        // Workflow integration
        let pre_existing: Vec<LedgerEntry> = target_symbols
            .iter()
            .flat_map(|sym| {
                ledger_store
                    .list_entries(&ref_name, &sym.symbol_id)
                    .unwrap_or_default()
            })
            .filter(|e| {
                !written
                    .iter()
                    .any(|w| w.get("summary").and_then(|s| s.as_str()) == Some(e.summary.as_str()))
            })
            .collect();

        let proof_was_explicit = p.proof.is_some();
        let eq = score_evidence_quality(
            &pre_existing,
            p.validated,
            p.evidence.as_deref(),
            proof_was_explicit,
            target_symbols.len(),
            written.len(),
        );
        let has_invariants = pre_existing.iter().any(|e| e.kind == LedgerKind::Invariant);
        let (wf_type, steps_detected, missing_steps) =
            detect_workflow(&pre_existing, &eq, has_invariants);
        let trust = compute_trust_score(&db_path);
        let db_state = trust.data_quality.state.clone();
        let db_state_note = match db_state.as_str() {
            "clean_room" => "fresh workspace — low evidence score is expected".to_string(),
            "unannotated" => "no prior annotations — low evidence score expected".to_string(),
            "degraded" => "warning: sparse ledger despite prior activity".to_string(),
            _ => String::new(),
        };

        let workflow_summary = WorkflowSummary {
            workflow_type: wf_type,
            steps_detected,
            missing_recommended_steps: missing_steps,
            evidence_quality: eq,
            task_id: task_id.clone(),
            plan_id: plan_id.clone(),
            closed_at: closed_at.clone(),
            symbols_annotated: target_symbols.len(),
            ledger_entries_written: written.len(),
            db_state,
            db_state_note,
        };
        append_workflow_session(&db_path, &workflow_summary);

        let out = serde_json::json!({
            "status": "closed",
            "closed_at": closed_at,
            "proof": proof_text,
            "validated": p.validated,
            "symbols_annotated": target_symbols.len(),
            "ledger_entries_written": written.len(),
            "ctx": {
                "plan": if plan_id.is_empty() { serde_json::Value::Null } else { serde_json::json!(plan_id) },
                "task": if task_id.is_empty() { serde_json::Value::Null } else { serde_json::json!(task_id) },
            },
            "workflow": workflow_summary.to_json(),
            "written": written,
        });
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // Tool 4: verify_effects — Static effect verification
    // -----------------------------------------------------------------------

    #[tool(
        description = "Compare declared effects against what the static checker infers from source. Returns ok/mismatch/unverified and the list of mismatches."
    )]
    async fn verify_effects(&self, params: Parameters<VerifyEffectsParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore::from_engine(&engine);
        let effect_store = AsgEffectStore::from_engine(&engine);
        let adapters = default_adapters();

        let symbol = match index_store.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let mut effect_decl = effect_store
            .get_effects(&ref_name, &symbol.symbol_id)
            .unwrap_or(None)
            .unwrap_or_else(|| EffectDecl {
                symbol_id: symbol.symbol_id.clone(),
                declared: Vec::new(),
                transitive: Vec::new(),
                verification: None,
                confidence: None,
                matched_policy: None,
            });

        let adapter = adapters
            .iter()
            .find(|a| a.language() == symbol.language.as_str());

        let (status, mismatches, inferred_strs) = if let Some(adapter) = adapter {
            match std::fs::read_to_string(&symbol.file) {
                Ok(source) => {
                    let stub = ParsedSymbol {
                        qname: symbol.qname.clone(),
                        kind: symbol.kind,
                        start_line: symbol.start.line,
                        start_col: symbol.start.col,
                        end_line: symbol.end.line,
                        end_col: symbol.end.col,
                        body: String::new(),
                        signature: symbol.signature.clone(),
                        doc: symbol.doc.clone(),
                    };
                    let inferred: Vec<EffectCategory> = adapter
                        .infer_effects(&source, &stub)
                        .into_iter()
                        .map(|e| e.effect)
                        .collect();
                    let declared_cats: Vec<EffectCategory> = effect_decl
                        .declared
                        .iter()
                        .map(|e| e.effect.clone())
                        .collect();
                    let mut mismatches: Vec<Mismatch> = Vec::new();
                    for cat in &declared_cats {
                        if !inferred.contains(cat) {
                            mismatches.push(Mismatch {
                                kind: "declared_not_inferred".to_string(),
                                effect: cat.clone(),
                                detected_in: Some(symbol.file.clone()),
                                note: Some("declared but not found by static checker".to_string()),
                            });
                        }
                    }
                    for cat in &inferred {
                        if !declared_cats.contains(cat) {
                            mismatches.push(Mismatch {
                                kind: "inferred_not_declared".to_string(),
                                effect: cat.clone(),
                                detected_in: Some(symbol.file.clone()),
                                note: Some(
                                    "found by static checker but not in declared effects"
                                        .to_string(),
                                ),
                            });
                        }
                    }
                    let status = if mismatches.is_empty() {
                        VerificationStatus::Ok
                    } else {
                        VerificationStatus::Mismatch
                    };
                    let inferred_strs: Vec<String> =
                        inferred.iter().map(|e| e.as_str().to_string()).collect();
                    (status, mismatches, inferred_strs)
                }
                Err(_) => (VerificationStatus::Unverified, Vec::new(), Vec::new()),
            }
        } else {
            (VerificationStatus::Unverified, Vec::new(), Vec::new())
        };

        if p.write {
            let verification = Verification {
                by: VerificationSource::StaticChecker,
                at: chrono::Utc::now(),
                status,
                mismatches: mismatches.clone(),
            };
            effect_decl.verification = Some(verification);
            if let Err(e) = effect_store.put_effects(
                &ref_name,
                &effect_decl.symbol_id,
                &effect_decl,
                "asd-mcp-verify",
            ) {
                return err_json(&format!("failed to write verification: {}", e));
            }
        }

        let out = serde_json::json!({
            "qname": symbol.qname,
            "symbol_id": symbol.symbol_id,
            "status": match status {
                VerificationStatus::Ok => "ok",
                VerificationStatus::Mismatch => "mismatch",
                VerificationStatus::Unverified => "unverified",
            },
            "declared": effect_decl.declared.iter().map(|e| e.effect.as_str()).collect::<Vec<_>>(),
            "inferred": inferred_strs,
            "mismatches": mismatches.iter().map(|m| serde_json::json!({
                "kind": m.kind, "effect": m.effect.as_str(), "note": m.note,
            })).collect::<Vec<_>>(),
            "written": p.write,
        });
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // Tool 5: status — Workspace health
    // -----------------------------------------------------------------------

    #[tool(
        description = "Workspace index health: symbol count, index age, sidecar state, dirty files, concept gaps, and State Trust Score."
    )]
    async fn status(&self, params: Parameters<StatusParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let fts = match SearchFtsDb::open(&db_path) {
            Ok(f) => f,
            Err(e) => return err_json(&format!("cannot open FTS db: {}", e)),
        };

        let project_root = db_path.parent().unwrap_or(std::path::Path::new("."));
        let sidecar_state = sidecar_lifecycle_state(project_root);
        let sidecar_key = match sidecar_state {
            SidecarState::Missing => "missing",
            SidecarState::Present => "present",
            SidecarState::Hydrated => "hydrated",
            SidecarState::FreshReset => "fresh-reset",
        };
        let sidecar_action = match sidecar_state {
            SidecarState::Missing => "run 'asd sync' to create sidecar",
            SidecarState::Present => "run 'asd hydrate' to load sidecar into ASG",
            SidecarState::Hydrated => "sidecar is current",
            SidecarState::FreshReset => "re-run 'asd index' then 'asd sync'",
        };

        if !fts.has_data() {
            return serde_json::to_string(&serde_json::json!({
                "state": "empty",
                "note": "run 'asd index <dir>' to build",
                "sidecar": sidecar_key,
            }))
            .unwrap_or_else(|_| "{}".to_string());
        }

        let count = fts.symbol_count();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (indexed_at, age_hours, index_state) = match fts.last_indexed_at() {
            Some(ts) => {
                let age_h = (now - ts).max(0) / 3600;
                let state = if age_h == 0 {
                    "fresh"
                } else if age_h >= 1 {
                    "stale"
                } else {
                    "ok"
                };
                (Some(ts), Some(age_h), state)
            }
            None => (None, None, "unknown"),
        };

        let dirty_files: Vec<String> = if p.show_dirty {
            match Proc::new("git")
                .args(["status", "--short", "--untracked-files=no"])
                .current_dir(project_root)
                .output()
            {
                Ok(o) if o.status.success() => {
                    let source_exts = [
                        ".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go", ".kt", ".java", ".rb",
                        ".cs",
                    ];
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter(|l| source_exts.iter().any(|ext| l.ends_with(ext)))
                        .map(|l| l.trim().to_string())
                        .collect()
                }
                _ => vec![],
            }
        } else {
            vec![]
        };

        // Concept gaps
        let concept_gaps: Vec<serde_json::Value> = {
            let ledger_store = AsgLedgerStore::from_engine(&engine);
            let tree = engine
                .repo
                .get_tree(&ref_name, "/asd/v1/index/by-qname")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            tree.as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                        .filter_map(|sym| {
                            let entries = ledger_store
                                .list_entries(&ref_name, &sym.symbol_id)
                                .unwrap_or_default();
                            let has_ownership =
                                entries.iter().any(|e| e.kind == LedgerKind::Ownership);
                            let has_concept = entries.iter().any(|e| e.kind == LedgerKind::Concept);
                            if has_ownership && !has_concept {
                                Some(serde_json::json!({"qname": sym.qname, "file": sym.file}))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        let trust = compute_trust_score(&db_path);

        // Plan J t-005: walk by-qname once to also report the
        // ASG-side symbol count alongside the FTS-side `count`,
        // so MCP `status` matches MCP `health`'s reconciliation
        // view and field-eval no longer sees two divergent
        // numbers without explanation.
        let asg_symbol_count = engine
            .repo
            .get_tree(&ref_name, "/asd/v1/index/by-qname")
            .ok()
            .and_then(|v| v.as_object().map(|m| m.len()));
        let index_consistency = asg_symbol_count
            .map(|asg| {
                agentstatedeveloper_core::compute_index_consistency(asg, count as usize)
            })
            .unwrap_or(serde_json::Value::Null);

        let out = serde_json::json!({
            "db": db_path.display().to_string(),
            "symbols": count,
            "indexed_at_unix": indexed_at,
            "age_hours": age_hours,
            "state": index_state,
            "sidecar": sidecar_key,
            "sidecar_action": sidecar_action,
            "dirty_files": dirty_files,
            "concept_gaps": concept_gaps,
            "index_consistency": index_consistency,
            "trust": trust.to_json(),
        });
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // Tool 6: scorecard — Benchmark dimensions
    // -----------------------------------------------------------------------

    #[tool(
        description = "Five-dimension benchmark scorecard: truth, feedback, change, uncertainty, workflow. Each scored 0-100. Use drill_down to see which symbols are gaps."
    )]
    async fn scorecard(&self, params: Parameters<ScorecardParams>) -> String {
        let p = params.0;
        let db_path = self.db_path();
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let effect_store = AsgEffectStore::from_engine(&engine);
        let feedback_store = AsgFeedbackStore::from_engine(&engine);

        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref s) = p.scope {
            paths_filter.extend(resolve_scope(s, &db_path));
        }
        if let Some(ref paths_str) = p.paths {
            paths_filter.extend(
                paths_str
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty()),
            );
        }
        let scoped = !paths_filter.is_empty();

        let all_syms: Vec<Symbol> = {
            let tree = engine
                .repo
                .get_tree(&ref_name, "/asd/v1/index/by-qname")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            tree.as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default()
        };

        let scored_syms: Vec<&Symbol> = if scoped {
            all_syms
                .iter()
                .filter(|s| paths_filter.iter().any(|pat| glob_match(pat, &s.file)))
                .collect()
        } else {
            all_syms.iter().collect()
        };
        let total_symbols = scored_syms.len();

        if total_symbols == 0 {
            let note = if scoped {
                "no symbols matched the path filter"
            } else {
                "no symbols indexed — run `asd index` first"
            };
            return serde_json::to_string(&serde_json::json!({
                "note": note,
                "scores": {"truth": 0, "feedback": 0, "change": 0, "uncertainty": 0, "workflow": 0, "overall": 0}
            })).unwrap_or_else(|_| "{}".to_string());
        }

        // Bulk load ledger
        let ledger_by_sym: std::collections::HashMap<String, Vec<LedgerEntry>> = {
            let tree = engine
                .repo
                .get_tree(&ref_name, "/asd/v1/ledger")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let mut map: std::collections::HashMap<String, Vec<LedgerEntry>> =
                std::collections::HashMap::new();
            if let serde_json::Value::Object(by_symbol) = tree {
                for (sym_id, per_symbol) in by_symbol {
                    if let serde_json::Value::Object(entries_map) = per_symbol {
                        let mut entries: Vec<LedgerEntry> = entries_map
                            .values()
                            .filter_map(|v| serde_json::from_value::<LedgerEntry>(v.clone()).ok())
                            .collect();
                        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                        let superseded: std::collections::HashSet<String> = entries
                            .iter()
                            .flat_map(|e| e.supersedes.iter().cloned())
                            .collect();
                        entries.retain(|e| !superseded.contains(&e.entry_id));
                        map.insert(sym_id, entries);
                    }
                }
            }
            map
        };

        let drill = p.drill_down.as_deref().unwrap_or("").to_lowercase();
        let need_drill = !drill.is_empty();
        let mut drill_rows: Vec<serde_json::Value> = Vec::new();

        let mut verified_count = 0usize;
        let mut owned_count = 0usize;
        let mut has_invariant_count = 0usize;
        let mut has_validation_count = 0usize;
        let mut total_ledger_entries = 0usize;
        let mut ctx_tagged_entries = 0usize;

        for sym in &scored_syms {
            let has_verified =
                if let Ok(Some(decl)) = effect_store.get_effects(&ref_name, &sym.symbol_id) {
                    decl.verification
                        .as_ref()
                        .map(|v| matches!(v.status, VerificationStatus::Ok))
                        .unwrap_or(false)
                } else {
                    false
                };
            if has_verified {
                verified_count += 1;
            }

            let entries = ledger_by_sym
                .get(&sym.symbol_id)
                .cloned()
                .unwrap_or_default();
            total_ledger_entries += entries.len();

            let mut sym_owned = false;
            let mut sym_inv = false;
            let mut sym_vs = false;
            let mut sym_ctx = false;
            for entry in &entries {
                match entry.kind {
                    LedgerKind::Invariant => sym_inv = true,
                    LedgerKind::ValidationScenario => sym_vs = true,
                    LedgerKind::Ownership => sym_owned = true,
                    _ => {}
                }
                if entry.tags.iter().any(|t| t.starts_with("ctx:")) {
                    sym_ctx = true;
                    ctx_tagged_entries += 1;
                }
            }
            if sym_owned {
                owned_count += 1;
            }
            if sym_inv {
                has_invariant_count += 1;
            }
            if sym_vs {
                has_validation_count += 1;
            }

            if need_drill {
                let include = match drill.as_str() {
                    "truth" => !has_verified || !sym_owned,
                    "change" => !sym_inv || !sym_vs,
                    "workflow" => entries.is_empty() || !sym_ctx,
                    "uncertainty" => !has_verified,
                    _ => false,
                };
                if include {
                    drill_rows.push(serde_json::json!({
                        "qname": sym.qname,
                        "file": sym.file,
                        "has_verified_effects": has_verified,
                        "has_ownership": sym_owned,
                        "has_invariant": sym_inv,
                        "has_validation_scenario": sym_vs,
                        "ledger_entries": entries.len(),
                        "ctx_tagged": sym_ctx,
                    }));
                }
            }
        }

        let feedback_count = feedback_store
            .list_all(&ref_name)
            .map(|v| v.len())
            .unwrap_or(0);

        // Compute scores
        let truth = if total_symbols == 0 {
            0.0
        } else {
            ((verified_count as f64 / total_symbols as f64
                + owned_count as f64 / total_symbols as f64)
                / 2.0
                * 100.0)
                .min(100.0)
        };
        let feedback_score = (feedback_count as f64 / 50.0 * 100.0).min(100.0);
        let change = if total_symbols == 0 {
            0.0
        } else {
            ((has_invariant_count as f64 / total_symbols as f64
                + has_validation_count as f64 / total_symbols as f64)
                / 2.0
                * 100.0)
                .min(100.0)
        };
        let uncertainty = {
            let effect_rate = if total_symbols == 0 {
                0.0
            } else {
                verified_count as f64 / total_symbols as f64
            };
            let volume_score = (total_symbols as f64 / 500.0).min(1.0);
            ((effect_rate + volume_score) / 2.0 * 100.0).min(100.0)
        };
        let workflow = {
            let density =
                (total_ledger_entries as f64 / total_symbols.max(1) as f64 / 2.0).min(1.0);
            let ctx_adoption = if total_ledger_entries == 0 {
                0.0
            } else {
                (ctx_tagged_entries as f64 / total_ledger_entries as f64).min(1.0)
            };
            ((density * 0.6 + ctx_adoption * 0.4) * 100.0).min(100.0)
        };
        let overall = (truth + feedback_score + change + uncertainty + workflow) / 5.0;

        let ledger_density = total_ledger_entries as f64 / total_symbols.max(1) as f64;
        let sparse_db = ledger_density < 0.5 && total_symbols > 0;

        let mut out = serde_json::json!({
            "scores": {
                "truth": truth.round() as u64,
                "feedback": feedback_score.round() as u64,
                "change": change.round() as u64,
                "uncertainty": uncertainty.round() as u64,
                "workflow": workflow.round() as u64,
                "overall": overall.round() as u64,
            },
            "data_quality": {
                "ledger_density": ledger_density,
                "symbols_scored": total_symbols,
                "sparse_db": sparse_db,
                "scope": if scoped { serde_json::json!(paths_filter) } else { serde_json::Value::Null },
            },
            "details": {
                "total_symbols": total_symbols,
                "verified_effects": verified_count,
                "owned_symbols": owned_count,
                "invariant_symbols": has_invariant_count,
                "validation_symbols": has_validation_count,
                "feedback_entries": feedback_count,
                "total_ledger_entries": total_ledger_entries,
                "ctx_tagged_ledger_entries": ctx_tagged_entries,
            },
        });

        if need_drill {
            let limit = p.limit as usize;
            let total_gaps = drill_rows.len();
            let shown: Vec<_> = drill_rows.into_iter().take(limit).collect();
            let omitted = total_gaps.saturating_sub(shown.len());
            out.as_object_mut().unwrap().insert(
                "drill_down".into(),
                serde_json::json!({
                    "dimension": drill,
                    "total_gaps": total_gaps,
                    "shown": shown.len(),
                    "omitted": omitted,
                    "gap_symbols": shown,
                }),
            );
        }

        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }

    // -----------------------------------------------------------------------
    // Tool 7: annotate_commit — Derive ledger annotations from a commit
    // -----------------------------------------------------------------------

    #[tool(
        description = "Derive ledger annotations (decisions, invariants, hazards, proofs) from a git commit's message and changed files. Dry-run by default; pass write=true to persist."
    )]
    async fn annotate_commit(&self, params: Parameters<AnnotateCommitParams>) -> String {
        let p = params.0;
        let _db_path = self.db_path();
        let sha = p.sha.clone().unwrap_or_else(|| "HEAD".to_string());

        // Commit metadata
        let log_out = match Proc::new("git")
            .args(["log", "-1", "--format=%H%n%s%n%b", &sha])
            .output()
        {
            Ok(o) => o,
            Err(e) => return err_json(&format!("git log failed: {}", e)),
        };
        let log_str = String::from_utf8_lossy(&log_out.stdout);
        let mut log_lines = log_str.lines();
        let commit_hash = log_lines.next().unwrap_or("").trim().to_string();
        let subject = log_lines.next().unwrap_or("").trim().to_string();
        let body: String = log_lines.collect::<Vec<_>>().join("\n");

        if commit_hash.is_empty() {
            return err_json(&format!("could not resolve commit: {}", sha));
        }

        let full_body = if let Some(ref td) = p.task_description {
            format!("{body}\n{td}")
        } else {
            body
        };

        // Changed files
        let diff_out = match Proc::new("git")
            .args([
                "diff-tree",
                "--no-commit-id",
                "-r",
                "--name-only",
                &commit_hash,
            ])
            .output()
        {
            Ok(o) => o,
            Err(e) => return err_json(&format!("git diff-tree failed: {}", e)),
        };
        let changed_files: Vec<String> = String::from_utf8_lossy(&diff_out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        if changed_files.is_empty() {
            return serde_json::to_string(&serde_json::json!({
                "commit": commit_hash, "subject": subject,
                "note": "no changed files detected", "suggested_entries": []
            }))
            .unwrap_or_else(|_| "{}".to_string());
        }

        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore::from_engine(&engine);
        let ledger_store = AsgLedgerStore::from_engine(&engine);

        let all_syms: Vec<Symbol> = index_store.build_id_map(&engine).into_values().collect();
        let mut touched_symbols: Vec<Symbol> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for sym in &all_syms {
            if changed_files
                .iter()
                .any(|f| sym.file.ends_with(f.as_str()) || sym.file == *f)
            {
                if seen_ids.insert(sym.symbol_id.clone()) {
                    touched_symbols.push(sym.clone());
                }
            }
        }
        touched_symbols.truncate(20);

        // Docs-only path
        let all_docs = !changed_files.is_empty()
            && changed_files
                .iter()
                .all(|f| agentstatedeveloper_core::is_doc_file(std::path::Path::new(f)));

        if all_docs && touched_symbols.is_empty() {
            let candidate_syms: Vec<&Symbol> = all_syms
                .iter()
                .filter(|s| !agentstatedeveloper_core::is_doc_file(std::path::Path::new(&s.file)))
                .collect();
            for doc_file in &changed_files {
                let stem = std::path::Path::new(doc_file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let file_terms: Vec<String> = stem
                    .replace(['-', '_', '.'], " ")
                    .split_whitespace()
                    .filter(|w| {
                        w.len() >= 3
                            && !matches!(
                                *w,
                                "the"
                                    | "and"
                                    | "for"
                                    | "doc"
                                    | "docs"
                                    | "readme"
                                    | "design"
                                    | "notes"
                                    | "plan"
                                    | "spec"
                                    | "guide"
                                    | "api"
                            )
                    })
                    .map(|w| w.to_string())
                    .collect();
                if file_terms.is_empty() {
                    continue;
                }
                let best = candidate_syms
                    .iter()
                    .copied()
                    .map(|s| {
                        let haystack =
                            format!("{} {}", s.qname.to_lowercase(), s.file.to_lowercase());
                        let ts = file_terms
                            .iter()
                            .filter(|t| haystack.contains(t.as_str()))
                            .count();
                        let kb: isize = match kind_str(&s.kind) {
                            "module" | "class" | "struct" | "enum" | "trait" | "type"
                            | "interface" | "protocol" | "namespace" => 2,
                            _ => 0,
                        };
                        ((ts as isize) * 2 + kb, ts, s)
                    })
                    .filter(|(_, ts, _)| *ts >= 1)
                    .max_by_key(|(score, _, _)| *score);
                if let Some((_, _, sym)) = best {
                    if seen_ids.insert(sym.symbol_id.clone()) {
                        touched_symbols.push(sym.clone());
                    }
                }
            }
        }

        // Parse commit body for annotations
        struct Annotation {
            kind: LedgerKind,
            summary: String,
        }
        let mut annotations: Vec<Annotation> = Vec::new();
        if !subject.is_empty() {
            let subject_kind = if all_docs {
                LedgerKind::Concept
            } else {
                LedgerKind::Decision
            };
            annotations.push(Annotation {
                kind: subject_kind,
                summary: subject.clone(),
            });
        }
        for line in full_body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (kind, text) = if let Some(rest) = line.strip_prefix("invariant:") {
                (LedgerKind::Invariant, rest.trim())
            } else if let Some(rest) = line.strip_prefix("hazard:") {
                (LedgerKind::Hazard, rest.trim())
            } else if let Some(rest) = line.strip_prefix("proof:") {
                (LedgerKind::Proof, rest.trim())
            } else if let Some(rest) = line.strip_prefix("validation_scenario:") {
                (LedgerKind::ValidationScenario, rest.trim())
            } else if let Some(rest) = line.strip_prefix("known_bug:") {
                (LedgerKind::KnownBug, rest.trim())
            } else if let Some(rest) = line.strip_prefix("concept:") {
                (LedgerKind::Concept, rest.trim())
            } else if let Some(rest) = line.strip_prefix("decision:") {
                (LedgerKind::Decision, rest.trim())
            } else {
                continue;
            };
            if !text.is_empty() {
                annotations.push(Annotation {
                    kind,
                    summary: text.to_string(),
                });
            }
        }

        // CTX provenance tags
        let ctx_plan = p.ctx_plan.clone().unwrap_or_default();
        let ctx_task_id = p.ctx_task.clone().unwrap_or_default();
        let mut ctx_tags: Vec<String> = Vec::new();
        if !ctx_plan.is_empty() {
            ctx_tags.push(format!("ctx:plan:{}", ctx_plan));
        }
        if !ctx_task_id.is_empty() {
            ctx_tags.push(format!("ctx:task:{}", ctx_task_id));
        }
        ctx_tags.push(format!(
            "commit:{}",
            &commit_hash[..8.min(commit_hash.len())]
        ));

        let author_id = p.author.clone().unwrap_or_else(|| {
            Proc::new("git")
                .args(["config", "user.name"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
                .trim()
                .to_string()
        });
        let author = Author {
            kind: AuthorKind::Human,
            id: author_id,
        };

        let mut suggested: Vec<serde_json::Value> = Vec::new();
        let mut written: Vec<serde_json::Value> = Vec::new();

        for sym in &touched_symbols {
            for ann in &annotations {
                let existing = ledger_store
                    .list_entries(&ref_name, &sym.symbol_id)
                    .unwrap_or_default();
                let already_exists = existing.iter().any(|e| {
                    e.kind == ann.kind && e.summary.to_lowercase() == ann.summary.to_lowercase()
                });
                if already_exists {
                    continue;
                }

                let entry_val = serde_json::json!({
                    "symbol": sym.qname,
                    "file": sym.file,
                    "kind": format!("{:?}", ann.kind).to_lowercase(),
                    "summary": ann.summary,
                });

                if p.write {
                    let mut entry = LedgerEntry::new(
                        sym.symbol_id.clone(),
                        ann.kind,
                        ann.summary.clone(),
                        author.clone(),
                    );
                    entry.tags.extend(ctx_tags.iter().cloned());
                    match ledger_store.append_entry(&ref_name, &entry, &author.id) {
                        Ok(_) => written.push(entry_val),
                        Err(e) => eprintln!("warn: could not write entry for {}: {e}", sym.qname),
                    }
                } else {
                    suggested.push(entry_val);
                }
            }
        }

        let out = if p.write {
            serde_json::json!({
                "commit": commit_hash, "subject": subject,
                "changed_files": changed_files,
                "touched_symbols": touched_symbols.len(),
                "written_entries": written,
            })
        } else {
            serde_json::json!({
                "commit": commit_hash, "subject": subject,
                "changed_files": changed_files,
                "touched_symbols": touched_symbols.iter().map(|s| &s.qname).collect::<Vec<_>>(),
                "suggested_entries": suggested,
                "note": "dry-run — pass write=true to record these entries",
            })
        };
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AsdMcpServer {}

// -- Helpers ----------------------------------------------------------------

fn mcp_git_recent_touches(files: &[(String, usize)], git_depth: usize) -> serde_json::Value {
    let mut result: Vec<serde_json::Value> = Vec::new();
    let mut sorted = files.to_vec();
    sorted.sort_by_key(|(_, d)| *d);
    for (file, _) in &sorted {
        let output = Proc::new("git")
            .args([
                "log",
                "--follow",
                &format!("-n{git_depth}"),
                "--pretty=format:%H\x1f%an\x1f%ad\x1f%s",
                "--date=short",
                "--",
                file,
            ])
            .output();
        let commits: Vec<serde_json::Value> = match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(|line| {
                    let p: Vec<&str> = line.splitn(4, '\x1f').collect();
                    if p.len() == 4 {
                        Some(serde_json::json!({
                            "sha": &p[0][..8.min(p[0].len())],
                            "author": p[1], "date": p[2], "message": p[3],
                        }))
                    } else {
                        None
                    }
                })
                .collect(),
            _ => vec![],
        };
        if !commits.is_empty() {
            result.push(serde_json::json!({ "file": file, "commits": commits }));
        }
    }
    serde_json::json!(result)
}

fn err_json(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "error": msg }))
        .unwrap_or_else(|_| "{\"error\":\"unknown\"}".to_string())
}

/// Plan G t-003: deterministic ledger entry id for `asd think` writes so
/// re-running the initial-read prompt overwrites rather than duplicates.
/// Mirror of the CLI helper in commands/think.rs.
fn think_det_id(intent: &str, qname: &str, content: &str) -> String {
    let key = format!("think:{intent}:{qname}:{content}");
    let h = blake3::hash(key.as_bytes()).to_hex();
    let short: String = h.chars().take(24).collect();
    format!("led_think_{short}")
}

/// Plan G t-007: read the active CTX task id (env `CTX_ACTIVE_TASK`
/// JSON `{"task_id": "..."}`, fallback `.asd/cache/active-task.json`
/// under the DB parent). Mirrors the CLI helper so MCP-driven writes
/// inherit the same `ctx:task:<id>` provenance trail.
fn think_active_ctx_task_tag(db_path: &std::path::Path) -> Option<String> {
    let env_raw = std::env::var("CTX_ACTIVE_TASK").ok();
    let db_parent = db_path.parent();
    let raw = match env_raw {
        Some(s) if !s.is_empty() => s,
        _ => {
            let p = db_parent?.join(".asd").join("cache").join("active-task.json");
            std::fs::read_to_string(p).ok()?
        }
    };
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let id = v.get("task_id")?.as_str()?;
    Some(format!("ctx:task:{id}"))
}

/// Stamp every `think_*` write with `source:asd-think` and (when set)
/// the active `ctx:task:<id>` tag.
fn think_push_provenance_tags(db_path: &std::path::Path, tags: &mut Vec<String>) {
    tags.push("source:asd-think".into());
    if let Some(t) = think_active_ctx_task_tag(db_path) {
        tags.push(t);
    }
}



fn parse_ledger_kind(s: &str) -> Result<LedgerKind, String> {
    match s.to_lowercase().as_str() {
        "decision" => Ok(LedgerKind::Decision),
        "assumption" => Ok(LedgerKind::Assumption),
        "constraint" => Ok(LedgerKind::Constraint),
        "rationale" => Ok(LedgerKind::Rationale),
        "hazard" => Ok(LedgerKind::Hazard),
        "tradeoff" => Ok(LedgerKind::Tradeoff),
        "invariant" => Ok(LedgerKind::Invariant),
        "ownership" => Ok(LedgerKind::Ownership),
        "proof" => Ok(LedgerKind::Proof),
        "validation_scenario" | "validationscenario" => Ok(LedgerKind::ValidationScenario),
        "known_bug" | "knownbug" => Ok(LedgerKind::KnownBug),
        "concept" => Ok(LedgerKind::Concept),
        "mapping" => Ok(LedgerKind::Mapping),
        "follow_up" | "followup" => Ok(LedgerKind::FollowUp),
        other => Err(format!(
            "unknown ledger kind: {}. Valid: decision, assumption, constraint, rationale, hazard, tradeoff, invariant, ownership, proof, validation_scenario, known_bug, concept, mapping, follow_up",
            other
        )),
    }
}

fn parse_author_kind(s: &str) -> Result<AuthorKind, String> {
    match s.to_lowercase().as_str() {
        "agent" => Ok(AuthorKind::Agent),
        "human" => Ok(AuthorKind::Human),
        other => Err(format!("unknown author kind: {}", other)),
    }
}

/// Resolve symbol_ids to full Symbol records by scanning the qname index.
fn resolve_symbols_by_ids(
    engine: &Engine,
    ids: &[String],
) -> agentstatedeveloper_core::Result<Vec<agentstatedeveloper_core::Symbol>> {
    let ref_name = &engine.ref_name;
    let prefix = format!(
        "{}/index/by-qname",
        agentstatedeveloper_core::ASD_PATH_PREFIX
    );
    let tree = match engine.repo.get_tree(ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let qnames: Vec<String> = match tree {
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        _ => return Ok(Vec::new()),
    };
    let index = AsgIndexStore::from_engine(&engine);
    let id_set: std::collections::HashSet<&String> = ids.iter().collect();
    let mut out = Vec::new();
    for qn in qnames {
        if let Some(sym) = index.get_symbol_by_qname(ref_name, &qn)? {
            if id_set.contains(&sym.symbol_id) {
                out.push(sym);
            }
        }
    }
    out.sort_by(|a, b| a.qname.cmp(&b.qname));
    Ok(out)
}

/// Shell out to `rg --json --fixed-strings --word-regexp <name>` and return
/// a flat occurrence list. Used by the `references` MCP tool to provide rg
/// parity for literal symbol queries (Plan A, t-004).
fn rg_scan(
    root: &std::path::Path,
    name: &str,
    globs: &[String],
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let mut cmd = Proc::new("rg");
    cmd.arg("--json")
        .arg("--fixed-strings")
        .arg("--word-regexp")
        .arg("--no-messages");
    for g in globs {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(name).arg(".");
    cmd.current_dir(root);

    let output = cmd
        .output()
        .map_err(|e| format!("failed to spawn `rg` ({e}). Install ripgrep or skip this tool."))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut occurrences = Vec::new();
    for line in stdout.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let data = match v.get("data") {
            Some(d) => d,
            None => continue,
        };
        let path = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let line_no = data
            .get("line_number")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let text = data
            .get("lines")
            .and_then(|l| l.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string();
        let columns: Vec<u64> = data
            .get("submatches")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|sm| sm.get("start").and_then(|s| s.as_u64()))
                    .collect()
            })
            .unwrap_or_default();
        occurrences.push(serde_json::json!({
            "file": path,
            "line": line_no,
            "columns": columns,
            "text": text,
        }));
        if limit > 0 && occurrences.len() >= limit {
            break;
        }
    }
    Ok(occurrences)
}

#[cfg(test)]
mod tool_name_regression {
    //! MCP/CLI parity regression probe (Plan A, t-002).
    //!
    //! Asserts that the canonical tool names land in the router and that the
    //! drift patterns we explicitly retired (the `_of` suffix) do not return.
    //! If this test fails, the parity audit needs to be redone.

    use super::AsdMcpServer;

    #[test]
    fn renamed_tools_are_present_under_canonical_names() {
        let r = AsdMcpServer::tool_router();
        for name in ["callers", "callees", "effects", "traces"] {
            assert!(
                r.has_route(name),
                "expected canonical MCP tool `{name}` to be registered"
            );
        }
    }

    #[test]
    fn retired_of_suffix_names_are_absent() {
        let r = AsdMcpServer::tool_router();
        for name in ["callers_of", "callees_of", "effects_of", "traces_of"] {
            assert!(
                !r.has_route(name),
                "retired MCP tool name `{name}` is registered — drift has crept back in"
            );
        }
    }

    #[test]
    fn no_tool_name_uses_of_suffix() {
        let r = AsdMcpServer::tool_router();
        let offenders: Vec<String> = r
            .into_iter()
            .map(|route| route.attr.name.to_string())
            .filter(|n| n.ends_with("_of"))
            .collect();
        assert!(
            offenders.is_empty(),
            "MCP tool names must not end with `_of`; offenders: {offenders:?}"
        );
    }

    #[test]
    fn references_tool_is_registered() {
        // Plan A, t-004: `references` is the exact-symbol rg-parity tool.
        assert!(
            AsdMcpServer::tool_router().has_route("references"),
            "expected `references` MCP tool to be registered"
        );
    }

    #[test]
    fn scopes_list_tool_is_registered() {
        // Plan A, t-005: discoverability for `--scope` / `--paths` filters.
        assert!(
            AsdMcpServer::tool_router().has_route("scopes_list"),
            "expected `scopes_list` MCP tool to be registered"
        );
    }

    #[test]
    fn conclusions_list_tool_is_registered() {
        // Plan B, t-003: bucketed view over the new conclusion classes.
        assert!(
            AsdMcpServer::tool_router().has_route("conclusions_list"),
            "expected `conclusions_list` MCP tool to be registered"
        );
    }

    #[test]
    fn conclusions_export_tool_is_registered() {
        // Plan B, t-004: write ledger → .asd/conclusions/*.jsonl.
        assert!(
            AsdMcpServer::tool_router().has_route("conclusions_export"),
            "expected `conclusions_export` MCP tool to be registered"
        );
    }

    #[test]
    fn conclusions_import_tool_is_registered() {
        // Plan B, t-005: read .asd/conclusions/*.jsonl → ledger.
        assert!(
            AsdMcpServer::tool_router().has_route("conclusions_import"),
            "expected `conclusions_import` MCP tool to be registered"
        );
    }

    #[test]
    fn recipe_classify_test_migration_tool_is_registered() {
        // Plan C t-004: first concrete recipe.
        assert!(
            AsdMcpServer::tool_router().has_route("recipe_classify_test_migration"),
            "expected `recipe_classify_test_migration` MCP tool to be registered"
        );
    }

    #[test]
    fn recipe_migrate_stale_tests_tool_is_registered() {
        // Plan F t-002: second recipe, adds Move action.
        assert!(
            AsdMcpServer::tool_router().has_route("recipe_migrate_stale_tests"),
            "expected `recipe_migrate_stale_tests` MCP tool to be registered"
        );
    }

    #[test]
    fn plan_g_think_tools_are_registered() {
        // Plan G t-003: agent-thinking write surface.
        let router = AsdMcpServer::tool_router();
        for name in [
            "think_speculate",
            "think_model",
            "think_failed",
            "think_question",
            "think_list",
        ] {
            assert!(
                router.has_route(name),
                "expected `{name}` MCP tool to be registered"
            );
        }
    }

    #[test]
    fn prefix_drifted_tools_keep_code_prefix() {
        // `code_*` names exist because the bare CLI counterparts (`search`,
        // `read`) would collide in MCP's flat namespace.
        let r = AsdMcpServer::tool_router();
        for name in ["code_search", "code_read", "code_query"] {
            assert!(
                r.has_route(name),
                "expected `{name}` to remain registered with the `code_` prefix"
            );
        }
    }
}

/// How often the registry watcher polls `~/.config/asd/repos.toml` for an
/// mtime change. Below the round-trip of any meaningful MCP tool call, so a
/// `asd repo use <other>` triggered from a separate shell becomes visible
/// before the agent's next tool invocation lands.
const REGISTRY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Spawn a background task that follows the active repo recorded in
/// `~/.config/asd/repos.toml`. On every mtime change we re-read the registry
/// and, if the active repo's db path differs from what the engine currently
/// holds, open the new db and swap it in place. Errors are logged at WARN —
/// the watcher never poisons the running engine.
fn spawn_registry_watcher(
    engine: Arc<Mutex<Engine>>,
    db_path: Arc<std::sync::RwLock<PathBuf>>,
) {
    let registry_path = match agentstatedeveloper_core::registry::Registry::path() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "registry watcher disabled: cannot resolve registry path");
            return;
        }
    };
    tokio::spawn(async move {
        let mut cached_mtime: Option<std::time::SystemTime> = None;
        loop {
            tokio::time::sleep(REGISTRY_POLL_INTERVAL).await;

            let mtime = std::fs::metadata(&registry_path)
                .and_then(|m| m.modified())
                .ok();
            if mtime == cached_mtime {
                continue;
            }
            cached_mtime = mtime;

            // Mtime changed (or first observation). Re-read the registry and
            // decide whether the active path actually moved.
            let reg = match agentstatedeveloper_core::registry::Registry::load_from(
                &registry_path,
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "registry watcher: load failed");
                    continue;
                }
            };
            let Some(active) = reg.active() else {
                continue;
            };
            let current = db_path
                .read()
                .ok()
                .map(|g| g.clone())
                .unwrap_or_default();
            if active.path == current {
                continue;
            }

            match Engine::open_sqlite(&active.path) {
                Ok(new_engine) => {
                    {
                        let mut guard = engine.lock().await;
                        *guard = new_engine;
                    }
                    if let Ok(mut g) = db_path.write() {
                        *g = active.path.clone();
                    }
                    tracing::info!(
                        from = %current.display(),
                        to = %active.path.display(),
                        name = %active.name,
                        "switched active repo via registry",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %active.path.display(),
                        "registry watcher: opening new db failed; keeping current engine",
                    );
                }
            }
        }
    });
}
