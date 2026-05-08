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
    AsgScratchStore, AuditEvent, Author, AuthorKind, CleanFilter, Decision, Effect, EffectCategory,
    EffectDecl, EffectStore, Engine, FeedbackEntry, FeedbackStore, FeedbackVerdict, FtsFilters,
    IndexStore, LedgerEntry, LedgerKind, LedgerStore, Rebind, ScratchEntry, ScratchFilter,
    ScratchStatus, ScratchStore, SearchDocsDb, SearchFtsDb, Situation, actions, classify_layer_sym,
    confidence_scores, derive_cold_hints, detect_ambiguous_tokens, detect_possible_misses,
    emit_audit, event_types, explain_match, extract_summary, find_candidates, gather_recency,
    git_dirty_files, hybrid_boost, intent_focus, intent_layer_order, load_layer_overrides,
    parse_intent, paths, parse_query, propose_test_path, resolve_scope, result_bucket, symbol_tier,
};

/// The AgentStateDeveloper MCP server.
#[derive(Clone)]
pub struct AsdMcpServer {
    engine: Arc<Mutex<Engine>>,
    db_path: PathBuf,
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
    /// Comma-separated terms to exclude (e.g. "sample editor,waveform").
    pub exclude: Option<String>,
    /// Comma-separated glob patterns to restrict to specific paths (e.g. "App/**/DriftPad*").
    pub paths: Option<String>,
    /// Named scope alias from .asd/scopes.toml (e.g. "drift-pad").
    pub scope: Option<String>,
}

fn default_search_limit() -> u32 { 20 }

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

fn default_investigate_depth() -> u32 { 10 }
fn default_impact_depth() -> u32 { 3 }
fn default_git_depth() -> u32 { 20 }
fn default_checklist_depth() -> u32 { 10 }
fn default_test_depth() -> u32 { 2 }
fn default_prepare_depth() -> u32 { 10 }
fn default_prepare_git_depth() -> u32 { 10 }

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

fn default_since_git_depth() -> u32 { 10 }

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
    /// Verdict: "useful", "noisy", "missing", or "wrong_layer".
    pub verdict: String,
    /// Optional free-text note explaining the verdict.
    pub note: Option<String>,
    /// Agent/author identifier (default: "asd-mcp-agent").
    #[serde(default = "default_author_id")]
    pub author_id: String,
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
    /// Ledger kind: decision, assumption, constraint, rationale, hazard, tradeoff, invariant, ownership, proof, validation_scenario, known_bug.
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

// -- Tool implementations ---------------------------------------------------

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
            db_path,
            audit_log_path,
            tool_router: Self::tool_router(),
        }
    }

    // -- Read tools --

    #[tool(
        description = "Health check: reports MCP server status, ASG db path, and indexed symbol count."
    )]
    async fn health(&self) -> String {
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let symbol_count = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.len(),
            _ => 0,
        };
        let db_path = self
            .db_path
            .canonicalize()
            .unwrap_or_else(|_| self.db_path.clone())
            .to_string_lossy()
            .to_string();
        let ledger_prefix = format!("{}/ledger", ASD_PATH_PREFIX);
        let orphan_count = match engine.repo.get_tree(&ref_name, &ledger_prefix) {
            Ok(serde_json::Value::Object(by_symbol)) => {
                let indexed_prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
                let indexed: std::collections::HashSet<String> = match engine.repo.get_tree(&ref_name, &indexed_prefix) {
                    Ok(serde_json::Value::Object(m)) => m.values()
                        .filter_map(|v| v.get("symbol_id")?.as_str().map(|s| s.to_string()))
                        .collect(),
                    _ => std::collections::HashSet::new(),
                };
                by_symbol.keys().filter(|sym_id| !indexed.contains(*sym_id)).count()
            }
            _ => 0,
        };
        let payload = serde_json::json!({
            "status": "ok",
            "db_path": db_path,
            "symbol_count": symbol_count,
            "orphaned_symbol_count": orphan_count,
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Query indexed symbols. Filters (all optional, AND-combined): name_contains, kind, language. Returns up to `limit` symbol summaries."
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

        let index = AsgIndexStore { repo: &engine.repo };
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
        serde_json::to_string(&symbols).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Ranked concept search over indexed symbols using FTS5/BM25. Returns symbols sorted by relevance. Use this when you need to discover entry points for a feature or concept — 'playhead over clips', 'auth flow', 'export pipeline', etc."
    )]
    async fn code_search(&self, params: Parameters<CodeSearchParams>) -> String {
        let p = params.0;
        let db_path = self.db_path.clone();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (tokens, mut exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
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
            paths_filter.extend(paths.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            exclude_terms: exclusions.clone(),
            paths_filter,
        };

        // --- FTS path ---
        let fts_result = SearchFtsDb::open(&db_path)
            .ok()
            .filter(|fts| fts.has_data())
            .and_then(|fts| fts.search(&p.query, &filters, limit * 4).ok());

        if let Some(hits) = fts_result {
            let ledger_store = AsgLedgerStore { repo: &engine.repo };
            let mut scored: Vec<(f64, _)> = hits
                .into_iter()
                .map(|hit| {
                    let boost = hybrid_boost(&hit, &tokens);
                    let entries = ledger_store
                        .list_entries(&ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let text = entries.iter().map(|e| e.summary.to_lowercase())
                        .collect::<Vec<_>>().join(" ");
                    let ledger_boost = if text.is_empty() { 0.0 } else {
                        tokens.iter().filter(|t| text.contains(t.as_str())).count() as f64
                    };
                    (hit.bm25_score + boost + ledger_boost, hit)
                })
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.qname.cmp(&b.1.qname)));
            if !exclusions.is_empty() {
                scored.retain(|(_, hit)| {
                    let qn = hit.qname.to_lowercase();
                    let fl = hit.file.to_lowercase();
                    let doc = hit.doc.as_deref().unwrap_or("").to_lowercase();
                    let sig = hit.signature.as_deref().unwrap_or("").to_lowercase();
                    !exclusions.iter().any(|e| qn.contains(e.as_str()) || fl.contains(e.as_str())
                        || doc.contains(e.as_str()) || sig.contains(e.as_str()))
                });
            }
            scored.truncate(limit);

            let recency = gather_recency(200, 14.0);
            let index_store = AsgIndexStore { repo: &engine.repo };
            let raw_scores: Vec<f64> = scored.iter().map(|(s, _)| *s).collect();
            let confidences = confidence_scores(&raw_scores);
            let mut layers_present: std::collections::HashSet<String> = std::collections::HashSet::new();
            let results: Vec<serde_json::Value> = scored.iter().zip(confidences.iter()).map(|((score, hit), conf)| {
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
                    .ok().flatten()
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
            }).collect();
            let layers_ref: std::collections::HashSet<&str> = layers_present.iter().map(|s| s.as_str()).collect();
            let ambiguous_terms = detect_ambiguous_tokens(&tokens, &db_path, &filters);
            let possible_misses = detect_possible_misses(&p.query, &layers_ref, results.len());
            // Document hits from the broad corpus (markdown, config, manifests, etc.)
            let doc_hits: Vec<serde_json::Value> = SearchDocsDb::open(&db_path)
                .ok()
                .filter(|db| !db.is_empty())
                .and_then(|db| db.search(&tokens, limit, None).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|h| serde_json::json!({
                    "source": "document",
                    "score": h.bm25_score,
                    "kind": h.kind,
                    "path": h.path,
                    "line": h.span_start,
                    "title": h.title,
                    "preview": h.preview,
                    "owner_symbol_id": h.owner_symbol_id,
                }))
                .collect();
            let out = serde_json::json!({
                "query": p.query,
                "ambiguous_terms": ambiguous_terms,
                "possible_misses": possible_misses,
                "results": results,
                "document_hits": doc_hits,
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
        let index = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let mut scored: Vec<(u32, agentstatedeveloper_core::Symbol)> = Vec::new();
        for qname in &qnames {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            let sk = format!("{:?}", sym.kind).to_lowercase();
            if let Some(ref k) = kind_filter { if &sk != k { continue; } }
            if let Some(ref lang) = lang_filter { if &sym.language != lang { continue; } }
            let qn = sym.qname.to_lowercase();
            let sig = sym.signature.as_deref().unwrap_or("").to_lowercase();
            let doc = sym.doc.as_deref().unwrap_or("").to_lowercase();
            let file = sym.file.to_lowercase();
            let ledger_text: String = ledger_store.list_entries(&ref_name, &sym.symbol_id)
                .unwrap_or_default().iter().map(|e| e.summary.to_lowercase())
                .collect::<Vec<_>>().join(" ");
            let mut score: u32 = 0;
            for token in &tokens {
                if qn.contains(token.as_str()) { score += 4; }
                if !sig.is_empty() && sig.contains(token.as_str()) { score += 3; }
                if !doc.is_empty() && doc.contains(token.as_str()) { score += 3; }
                if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) { score += 2; }
                if file.contains(token.as_str()) { score += 1; }
            }
            if score > 0 { scored.push((score, sym)); }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.qname.cmp(&b.1.qname)));
        scored.truncate(limit);
        let recency = gather_recency(200, 14.0);
        let results: Vec<serde_json::Value> = scored.iter().map(|(score, sym)| {
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
        }).collect();
        serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Feature archaeology in one pass: FTS5 search for entry points, then expand each with call chains, effects, invariants, and hazards. Use this at the start of any broad investigation — 'playhead over clips', 'auth flow', 'export pipeline', etc."
    )]
    async fn investigate(&self, params: Parameters<InvestigateParams>) -> String {
        let p = params.0;
        let intent = p.intent.as_deref().and_then(parse_intent).unwrap_or("");
        let db_path = self.db_path.clone();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (tokens, mut exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
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
            paths_filter.extend(paths.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            exclude_terms: exclusions,
            paths_filter,
        };

        let index = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let effect_store = AsgEffectStore { repo: &engine.repo };

        let mut top_qnames = find_candidates(
            &engine, &db_path, &p.query, &tokens, &filters,
            &ledger_store, &index, depth,
        );

        // Apply durable feedback adjustments.
        {
            use agentstatedeveloper_core::{apply_feedback_adjustments, FeedbackStore};
            let fb_store = AsgFeedbackStore { repo: &engine.repo };
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
            let callee_ids = index.get_callees(&ref_name, &sym.symbol_id).unwrap_or_default();
            let caller_ids = index.get_callers(&ref_name, &sym.symbol_id).unwrap_or_default();
            let effects = effect_store.get_effects(&ref_name, &sym.symbol_id).unwrap_or(None);
            let ledger = ledger_store.list_entries(&ref_name, &sym.symbol_id).unwrap_or_default();

            let mut invariants: Vec<serde_json::Value> = Vec::new();
            let mut hazards: Vec<serde_json::Value> = Vec::new();
            let mut ownership: Vec<serde_json::Value> = Vec::new();
            let mut validation_scenarios: Vec<serde_json::Value> = Vec::new();
            let mut known_bugs: Vec<serde_json::Value> = Vec::new();
            let mut other_ledger: Vec<serde_json::Value> = Vec::new();
            for entry in &ledger {
                let v = serde_json::to_value(entry).unwrap_or_default();
                match entry.kind {
                    LedgerKind::Invariant => invariants.push(v),
                    LedgerKind::Hazard => hazards.push(v),
                    LedgerKind::Ownership => ownership.push(v),
                    LedgerKind::ValidationScenario => validation_scenarios.push(v),
                    LedgerKind::KnownBug => known_bugs.push(v),
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
                    let key = inv.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !key.is_empty() && seen.insert(key) {
                        let mut v = inv.clone();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("source_qname".to_string(), serde_json::Value::String(qname.to_string()));
                        }
                        all_invariants.push(v);
                    }
                }
            }
            if let Some(hzs) = ep.get("hazards").and_then(|v| v.as_array()) {
                for hz in hzs {
                    let mut v = hz.clone();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("source_qname".to_string(), serde_json::Value::String(qname.to_string()));
                    }
                    all_hazards.push(v);
                }
            }
        }

        // Group by layer (intent-aware ordering).
        let layer_order = intent_layer_order(intent);
        let mut by_layer = serde_json::Map::new();
        for lk in layer_order {
            let members: Vec<&serde_json::Value> = entry_points.iter()
                .filter(|ep| ep.get("layer").and_then(|v| v.as_str()) == Some(*lk))
                .collect();
            if !members.is_empty() {
                by_layer.insert(lk.to_string(), serde_json::Value::Array(members.into_iter().cloned().collect()));
            }
        }

        let focus = intent_focus(intent);
        let layers_present: std::collections::HashSet<&str> = entry_points.iter()
            .filter_map(|ep| ep.get("layer").and_then(serde_json::Value::as_str))
            .collect();
        let ambiguous_terms = detect_ambiguous_tokens(&tokens, &db_path, &filters);
        let possible_misses = detect_possible_misses(&p.query, &layers_present, entry_points.len());
        serde_json::to_string(&serde_json::json!({
            "query": p.query,
            "intent": if intent.is_empty() { serde_json::Value::Null } else { serde_json::json!(intent) },
            "focus": if focus.is_empty() { serde_json::Value::Null } else { serde_json::json!(focus) },
            "tokens": tokens,
            "ambiguous_terms": ambiguous_terms,
            "possible_misses": possible_misses,
            "invariants": all_invariants,
            "hazards": all_hazards,
            "by_layer": by_layer,
        })).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Read a symbol by qname. Returns { symbol, effects, ledger } — full context needed to reason about the code unit."
    )]
    async fn code_read(&self, params: Parameters<CodeReadParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore { repo: &engine.repo };
        let effects = match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let ledger = match ledger_store.list_entries(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let payload = serde_json::json!({
            "symbol": symbol,
            "effects": effects,
            "ledger": ledger,
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Return declared + transitive effects for a symbol (resolved via qname)."
    )]
    async fn effects_of(&self, params: Parameters<EffectsOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore { repo: &engine.repo };
        match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(Some(decl)) => {
                serde_json::to_string(&decl).unwrap_or_else(|_| "null".to_string())
            }
            Ok(None) => "null".to_string(),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List symbols that call the given symbol (inbound call edges, intra-module)."
    )]
    async fn callers_of(&self, params: Parameters<CallersOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore { repo: &engine.repo };
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
        serde_json::to_string(&syms).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List symbols called by the given symbol (outbound call edges, intra-module)."
    )]
    async fn callees_of(&self, params: Parameters<CalleesOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore { repo: &engine.repo };
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
        serde_json::to_string(&syms).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List ledger entries for a symbol, newest first. By default, entries superseded by later entries are omitted; set include_superseded=true to include them."
    )]
    async fn ledger_get(&self, params: Parameters<LedgerGetParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
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

        let index = AsgIndexStore { repo: &engine.repo };
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

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
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
        if let Ok(Decision::Deny { matched_policy, reason }) =
            engine.policy.evaluate(&situation, actions::LEDGER_APPROVE, &p.approver)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
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
        if let Ok(Decision::Deny { matched_policy, reason }) =
            engine.policy.evaluate(&situation, actions::LEDGER_REJECT, &p.reviewer)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
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
        if let Ok(Decision::Deny { matched_policy, reason }) =
            engine.policy.evaluate(&situation, actions::LEDGER_WITHDRAW, &p.author_id)
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        match ledger_store.withdraw_entry(&ref_name, &p.entry_id, &p.author_id, "asd-mcp") {
            Ok(outcome) => {
                let status = if outcome.already_resolved {
                    "already-withdrawn"
                } else {
                    "withdrawn"
                };
                let event = AuditEvent::new(
                    event_types::LEDGER_WITHDRAW,
                    &p.author_id,
                    "agent",
                    status,
                )
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
                let event = AuditEvent::new(
                    event_types::LEDGER_WITHDRAW,
                    &p.author_id,
                    "agent",
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
        let index = AsgIndexStore { repo: &engine.repo };
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
        if let Ok(Decision::Deny { matched_policy, reason }) =
            engine.policy.evaluate(&situation, actions::LEDGER_SUPERSEDE, &p.author_id)
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

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
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

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let event = AuditEvent::new(
                    event_types::EFFECT_DECLARE,
                    &p.author_id,
                    "agent",
                    "error",
                )
                .with_reason(format!("symbol not found: {}", p.qname))
                .with_payload(serde_json::json!({ "qname": &p.qname }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&format!("symbol not found: {}", p.qname));
            }
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::EFFECT_DECLARE,
                    &p.author_id,
                    "agent",
                    "error",
                )
                .with_reason(e.to_string())
                .with_payload(serde_json::json!({ "qname": &p.qname }));
                emit_audit(engine.audit.as_ref(), event);
                return err_json(&e.to_string());
            }
        };

        let effects_store = AsgEffectStore { repo: &engine.repo };
        let existing = match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => {
                let event = AuditEvent::new(
                    event_types::EFFECT_DECLARE,
                    &p.author_id,
                    "agent",
                    "error",
                )
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
        let new_categories: Vec<String> =
            declared.iter().map(|e| e.effect.as_str().to_string()).collect();
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
                let event = AuditEvent::new(
                    event_types::EFFECT_DECLARE,
                    &p.author_id,
                    "agent",
                    "error",
                )
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
            let event = AuditEvent::new(
                event_types::EFFECT_DECLARE,
                &p.author_id,
                "agent",
                "denied",
            )
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
            transitive: existing.as_ref().map(|d| d.transitive.clone()).unwrap_or_default(),
            verification: existing.as_ref().and_then(|d| d.verification.clone()),
            confidence: existing.as_ref().and_then(|d| d.confidence),
            matched_policy: matched_policy.clone(),
        };

        if let Err(e) =
            effects_store.put_effects(&ref_name, &symbol.symbol_id, &updated, &p.author_id)
        {
            let event = AuditEvent::new(
                event_types::EFFECT_DECLARE,
                &p.author_id,
                "agent",
                "error",
            )
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

        let event = AuditEvent::new(
            event_types::EFFECT_DECLARE,
            &p.author_id,
            "agent",
            status,
        )
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
    async fn traces_of(&self, params: Parameters<TracesOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore { repo: &engine.repo };
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
            Some(&self.db_path),
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
        match engine.policy.evaluate(&situation, actions::LEDGER_REBIND, &p.agent_id) {
            Ok(Decision::Deny { matched_policy, reason }) => {
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
        let index_store = AsgIndexStore { repo: &engine.repo };
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
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
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
            if engine.repo.set_json(
                ref_name,
                &new_path,
                &val,
                CommitOptions::new(
                    &p.agent_id,
                    IntentCategory::Refine,
                    format!("reparent ledger entry {} after rebind", entry.entry_id),
                ),
            ).is_ok() {
                let old_path = paths::ledger_entry_path(&p.from_symbol_id, &entry.entry_id);
                let _ = engine.repo.delete(ref_name, &old_path, CommitOptions::new(
                    &p.agent_id,
                    IntentCategory::Refine,
                    format!("remove old ledger entry {} after rebind", entry.entry_id),
                ));
                reparented += 1;
            }
        }

        let audit_event = AuditEvent::new(event_types::LEDGER_REBIND, &p.agent_id, "agent", "allow")
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
            let index = AsgIndexStore { repo: &engine.repo };
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => { entry.symbol_id = Some(sym.symbol_id); }
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
            let index = AsgIndexStore { repo: &engine.repo };
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => { filter.symbol_id = Some(sym.symbol_id); }
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
        let ledger_store = AsgLedgerStore { repo: &engine.repo };

        // 1. Read scratch entry.
        let entry = match scratch_store.read_entry(&ref_name, &p.scratch_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        // 2. Resolve symbol_id.
        let symbol_id = if let Some(ref qname) = p.qname {
            let index = AsgIndexStore { repo: &engine.repo };
            match index.get_symbol_by_qname(&ref_name, qname) {
                Ok(Some(sym)) => sym.symbol_id,
                Ok(None) => return err_json(&format!("symbol not found: {qname}")),
                Err(e) => return err_json(&e.to_string()),
            }
        } else if let Some(ref sid) = entry.symbol_id {
            sid.clone()
        } else {
            return err_json(
                "no symbol attached to scratch entry and qname was not provided",
            );
        };

        // 3. Parse ledger kind.
        let kind = match parse_ledger_kind(&p.kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };

        // 4. Build summary.
        let summary = p.summary.unwrap_or_else(|| {
            entry.content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or(&entry.content)
                .chars()
                .take(140)
                .collect()
        });

        // 5. Create LedgerEntry.
        let author = Author { kind: AuthorKind::Agent, id: "asd-mcp".to_string() };
        let mut ledger_entry = LedgerEntry::new(&symbol_id, kind, &summary, author);
        ledger_entry.body = Some(entry.content.clone());

        if let Err(e) = ledger_store.append_entry(&ref_name, &ledger_entry, "asd-mcp") {
            return err_json(&e.to_string());
        }

        // 6. Mark scratch promoted.
        match scratch_store.mark_promoted(&ref_name, &entry.scratch_id, &ledger_entry.entry_id, "asd-mcp") {
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

        let statuses: Vec<ScratchStatus> = p.statuses
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
            Ok(count) => serde_json::to_string(&serde_json::json!({ "deleted": count, "dry_run": p.dry_run }))
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
        let db_path = self.db_path.clone();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (mut tokens, mut exclusions) = parse_query(&p.description);
        if let Some(ref excl) = p.exclude {
            for term in excl.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
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
            return serde_json::json!({ "description": p.description, "entry_points": {} }).to_string();
        }

        let depth = p.depth.max(1) as usize;
        let test_depth = p.test_depth.max(1) as usize;
        let git_depth = p.git_depth.max(1) as usize;
        let mut paths_filter: Vec<String> = Vec::new();
        if let Some(ref scope) = p.scope {
            paths_filter.extend(resolve_scope(scope, &db_path));
        }
        if let Some(ref paths) = p.paths {
            paths_filter.extend(paths.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            exclude_terms: exclusions,
            paths_filter,
        };

        let index = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let effect_store = AsgEffectStore { repo: &engine.repo };

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
            &engine, &db_path, &p.description, &tokens, &filters,
            &ledger_store, &index, depth,
        );

        // Apply durable feedback adjustments.
        {
            use agentstatedeveloper_core::{apply_feedback_adjustments, FeedbackStore};
            let fb_store = AsgFeedbackStore { repo: &engine.repo };
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
        let mut file_scores: Vec<(f64, String, String, Option<f64>, bool)> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        let mut seen_vs: HashSet<String> = HashSet::new();
        let mut seen_effect: HashSet<String> = HashSet::new();
        let mut top_sym_id: Option<String> = None;
        let effect_score_floor = candidates.first().map(|(s, _)| s * 0.25).unwrap_or(0.0);

        for (score, qname) in &candidates {
            let sym = match index.get_symbol_by_qname(&ref_name, qname) { Ok(Some(s)) => s, _ => continue };
            let tier = symbol_tier(&sym.file);
            let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);
            let summary = extract_summary(sym.doc.as_deref(), sym.signature.as_deref());
            let rec = recency.get(&sym.file);
            let ltd = rec.and_then(|r| r.last_touched_days);
            let hot = rec.map(|r| r.hot).unwrap_or(false);
            if top_sym_id.is_none() { top_sym_id = Some(sym.symbol_id.clone()); }
            if seen_files.insert(sym.file.clone()) {
                file_scores.push((*score, sym.file.clone(), layer.to_string(), ltd, hot));
            }
            let entries = ledger_store.list_entries(&ref_name, &sym.symbol_id).unwrap_or_default();
            for entry in &entries {
                match entry.kind {
                    LedgerKind::Invariant => {
                        if seen_inv.insert(entry.summary.clone()) {
                            design_invariants.push(serde_json::json!({ "summary": entry.summary, "source": sym.qname }));
                        }
                    }
                    LedgerKind::Hazard => {
                        known_hazards.push(serde_json::json!({ "summary": entry.summary, "source": sym.qname }));
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
                        if has_high_signal && eff.effect.is_low_signal() { continue; }
                        let cat = eff.effect.as_str().to_string();
                        let key = format!("{}:{}", cat, sym.qname);
                        if seen_effect.insert(key) {
                            effects_summary.push(serde_json::json!({ "category": cat, "source": sym.qname }));
                        }
                    }
                }
            }
            let ep = serde_json::json!({
                "score": score, "qname": sym.qname, "file": sym.file,
                "line": sym.start.line, "layer": layer, "summary": summary,
                "last_touched_days": ltd, "hot": hot,
            });
            by_layer.entry(layer.to_string())
                .or_insert_with(|| serde_json::Value::Array(vec![]))
                .as_array_mut().unwrap().push(ep);
        }

        // Reorder by_layer.
        let mut ordered: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for lk in layer_order {
            if let Some(v) = by_layer.remove(*lk) { ordered.insert(lk.to_string(), v); }
        }

        file_scores.sort_by(|a, b| b.4.cmp(&a.4).then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)));
        let dirty_files_pc = git_dirty_files();
        let likely_edit_files: Vec<serde_json::Value> = file_scores.iter().map(|(score, file, layer, days, hot)| {
            let fl = file.to_lowercase();
            let file_role = if fl.contains("/example") || fl.contains("/sample") || fl.contains("/demo") { "example" }
                else if fl.contains("/test") || fl.contains("/spec") || fl.contains("_test.") || fl.contains("spec.") { "test" }
                else if fl.contains("/reference") || fl.contains("/doc") || fl.ends_with(".md") { "reference" }
                else { "impl" };
            let conflict_risk = dirty_files_pc.contains(file.as_str());
            serde_json::json!({
                "file": file, "layer": layer, "score": score,
                "last_touched_days": days, "hot": hot,
                "file_role": file_role, "conflict_risk": conflict_risk,
            })
        }).collect();

        // Affected tests via BFS from top entry point.
        let mut affected_tests: Vec<serde_json::Value> = Vec::new();
        if let Some(start_id) = top_sym_id {
            let mut visited: HashSet<String> = HashSet::new();
            let mut queue: VecDeque<(String, usize)> = VecDeque::new();
            let mut seen_tnames: HashSet<String> = HashSet::new();
            visited.insert(start_id.clone());
            queue.push_back((start_id, 0));
            while let Some((sid, depth)) = queue.pop_front() {
                if depth >= test_depth { continue; }
                let callers = index.get_callers(&ref_name, &sid).unwrap_or_default();
                for cid in callers {
                    if visited.contains(&cid) { continue; }
                    visited.insert(cid.clone());
                    if let Some(s) = id_map.get(&cid) {
                        if symbol_tier(&s.file) == 2 && seen_tnames.insert(s.qname.clone()) {
                            let qname_words: Vec<String> = s.qname
                                .split(|c: char| !c.is_alphabetic())
                                .filter(|t: &&str| t.len() > 2)
                                .map(|t| t.to_lowercase())
                                .collect();
                            let doc_words: Vec<String> = s.doc.as_deref().unwrap_or("")
                                .split(|c: char| !c.is_alphabetic())
                                .filter(|t: &&str| t.len() > 2)
                                .map(|t| t.to_lowercase())
                                .collect();
                            let test_tokens: Vec<&str> = qname_words.iter()
                                .chain(doc_words.iter())
                                .map(|s| s.as_str())
                                .collect();
                            let covers: Vec<&str> = design_invariants.iter()
                                .filter_map(|inv| inv.get("summary").and_then(serde_json::Value::as_str))
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
                        if depth + 1 < test_depth { queue.push_back((cid, depth + 1)); }
                    }
                }
            }
        }

        // Recent git touches on top 3 files.
        let top_files: Vec<(String, usize)> = file_scores.iter().take(3).map(|(_, f, _, _, _)| (f.clone(), 0)).collect();
        let recently_touched = mcp_git_recent_touches(&top_files, git_depth);

        let test_gap = affected_tests.is_empty();
        let proposed_test_path = test_gap.then(|| {
            file_scores.first().map(|(_, f, _, _, _)| propose_test_path(f))
        }).flatten();
        let suggested_test_coverage: Vec<String> = if test_gap {
            let mut hints: Vec<String> = design_invariants.iter()
                .filter_map(|inv| inv.get("summary").and_then(serde_json::Value::as_str))
                .map(|s| s.to_string())
                .collect();
            for eff in &effects_summary {
                if let Some(cat) = eff.get("category").and_then(serde_json::Value::as_str) {
                    let hint = format!("verify {} after change", cat.to_lowercase());
                    if !hints.contains(&hint) { hints.push(hint); }
                }
            }
            if design_invariants.is_empty() {
                if let Some((_, qname)) = candidates.first() {
                    if let Ok(Some(sym)) = index.get_symbol_by_qname(&ref_name, qname) {
                        for h in derive_cold_hints(&sym.qname, sym.signature.as_deref(), sym.doc.as_deref()) {
                            if !hints.contains(&h) { hints.push(h); }
                        }
                    }
                }
            }
            hints
        } else { vec![] };

        const CONSTRAINT_WORDS: &[&str] = &[
            "must", "never", "shall", "always", "only", "cannot", "no ", "not ",
            "require", "ensure", "prevent", "guarantee", "invariant", "forbidden",
        ];
        let scenario_tests: Vec<&str> = design_invariants.iter()
            .filter_map(|inv| inv.get("summary").and_then(serde_json::Value::as_str))
            .filter(|s| {
                let sl = s.to_lowercase();
                CONSTRAINT_WORDS.iter().any(|w| sl.contains(w))
            })
            .collect();

        // T1: safe-change recipe.  T4: manually_validate includes ValidationScenario entries.
        let recipe_inspect: Vec<serde_json::Value> = file_scores.iter()
            .map(|(score, file, layer, days, hot)| serde_json::json!({
                "file": file, "layer": layer, "score": score, "last_touched_days": days, "hot": hot,
            }))
            .collect();
        let recipe_preserve: Vec<serde_json::Value> = design_invariants.iter()
            .map(|inv| serde_json::json!({ "constraint": inv["summary"], "source": inv["source"], "kind": "invariant" }))
            .chain(known_hazards.iter().map(|h| serde_json::json!({ "constraint": h["summary"], "source": h["source"], "kind": "hazard" })))
            .collect();
        let recipe_edit: Vec<serde_json::Value> = likely_edit_files.iter()
            .filter(|f| f["file_role"].as_str() == Some("impl"))
            .cloned()
            .chain(likely_edit_files.iter().filter(|f| f["file_role"].as_str() != Some("impl")).cloned())
            .collect();
        let recipe_run: Vec<serde_json::Value> = affected_tests.iter()
            .map(|t| serde_json::json!({ "qname": t["qname"], "file": t["file"], "covers_invariants": t["covers_invariants"] }))
            .collect();
        let mut recipe_manually_validate: Vec<serde_json::Value> = validation_scenarios_ledger.clone();
        for s in &scenario_tests {
            recipe_manually_validate.push(serde_json::json!({ "scenario": s, "source": "invariant", "kind": "constraint_check" }));
        }
        for eff in &effects_summary {
            let desc = format!("verify {} side-effect still correct after change",
                eff["category"].as_str().unwrap_or("").to_lowercase());
            recipe_manually_validate.push(serde_json::json!({ "scenario": desc, "source": eff["source"], "kind": "effect_check" }));
        }
        let safe_change_recipe = serde_json::json!({
            "inspect": recipe_inspect,
            "preserve": recipe_preserve,
            "edit": recipe_edit,
            "run": recipe_run,
            "manually_validate": recipe_manually_validate,
        });

        let focus = intent_focus(intent);
        let layers_present_pc: std::collections::HashSet<&str> = file_scores.iter()
            .map(|(_, _, layer, _, _)| layer.as_str())
            .collect();
        let ambiguous_terms = detect_ambiguous_tokens(&tokens, &db_path, &filters);
        let possible_misses = detect_possible_misses(&p.description, &layers_present_pc, file_scores.len());
        serde_json::to_string(&serde_json::json!({
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
            "suggested_test_coverage": suggested_test_coverage,
            "scenario_tests": scenario_tests,
            "effects_summary": effects_summary,
            "recently_touched": recently_touched,
        })).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Pre-edit checklist for a query: files to inspect, invariants to preserve, tests to run, known hazards, and effects to verify. Returns structured JSON. Use this before any code edit to get a focused action list."
    )]
    async fn checklist(&self, params: Parameters<ChecklistParams>) -> String {
        let p = params.0;
        let intent = p.intent.as_deref().and_then(parse_intent).unwrap_or("");
        let db_path = self.db_path.clone();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let (tokens, mut exclusions) = parse_query(&p.query);
        if let Some(ref excl) = p.exclude {
            for term in excl.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()) {
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
            paths_filter.extend(paths.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        let filters = FtsFilters {
            kind: p.kind.as_deref().map(|k| k.to_lowercase()),
            language: p.language.as_deref().map(|l| l.to_lowercase()),
            include_tests: p.include_tests,
            exclude_terms: exclusions,
            paths_filter,
        };

        let index = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let effect_store = AsgEffectStore { repo: &engine.repo };

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
            &engine, &db_path, &p.query, &tokens, &filters,
            &ledger_store, &index, depth,
        );

        // Apply durable feedback adjustments.
        {
            use agentstatedeveloper_core::{apply_feedback_adjustments, FeedbackStore};
            let fb_store = AsgFeedbackStore { repo: &engine.repo };
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
            let sym = match index.get_symbol_by_qname(&ref_name, qname) { Ok(Some(s)) => s, _ => continue };
            let tier = symbol_tier(&sym.file);
            let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);

            if seen_files.insert(sym.file.clone()) {
                files_to_inspect.push(serde_json::json!({
                    "file": sym.file, "qname": sym.qname, "layer": layer, "line": sym.start.line,
                }));
            }

            let entries = ledger_store.list_entries(&ref_name, &sym.symbol_id).unwrap_or_default();
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
                if depth >= test_depth { continue; }
                let callers = index.get_callers(&ref_name, &sid).unwrap_or_default();
                for cid in callers {
                    if visited.contains(&cid) { continue; }
                    visited.insert(cid.clone());
                    if let Some(s) = id_map.get(&cid) {
                        if symbol_tier(&s.file) == 2 && seen_tests.insert(s.qname.clone()) {
                            test_rows.push(serde_json::json!({
                                "qname": s.qname, "file": s.file, "line": s.start.line,
                            }));
                        }
                        if depth + 1 < test_depth { queue.push_back((cid, depth + 1)); }
                    }
                }
            }
        }

        let test_gap = test_rows.is_empty();
        let proposed_test_path = test_gap.then(|| {
            files_to_inspect.first()
                .and_then(|v| v.get("file").and_then(serde_json::Value::as_str))
                .map(propose_test_path)
        }).flatten();
        let suggested_test_coverage: Vec<String> = if test_gap {
            let mut hints: Vec<String> = invariants.iter()
                .filter_map(|inv| inv.get("summary").and_then(serde_json::Value::as_str))
                .map(|s| s.to_string())
                .collect();
            for eff in &effects_list {
                if let Some(cat) = eff.get("category").and_then(serde_json::Value::as_str) {
                    let hint = format!("verify {} after change", cat.to_lowercase());
                    if !hints.contains(&hint) { hints.push(hint); }
                }
            }
            if invariants.is_empty() {
                if let Some((_, qname)) = candidates.first() {
                    if let Ok(Some(sym)) = index.get_symbol_by_qname(&ref_name, qname) {
                        for h in derive_cold_hints(&sym.qname, sym.signature.as_deref(), sym.doc.as_deref()) {
                            if !hints.contains(&h) { hints.push(h); }
                        }
                    }
                }
            }
            hints
        } else { vec![] };

        const CONSTRAINT_WORDS_CL: &[&str] = &[
            "must", "never", "shall", "always", "only", "cannot", "no ", "not ",
            "require", "ensure", "prevent", "guarantee", "invariant", "forbidden",
        ];
        let scenario_tests: Vec<&str> = invariants.iter()
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
                let source = inv.get("source").and_then(serde_json::Value::as_str).unwrap_or("");
                let summary = inv.get("summary").and_then(serde_json::Value::as_str).unwrap_or("");
                if !source.is_empty() && !summary.is_empty() {
                    suggestions.push(serde_json::json!({
                        "action": "ledger_append", "kind": "proof", "symbol": source,
                        "suggested_summary": format!("verified that {} holds after change", summary),
                    }));
                }
            }
            for h in hazards.iter().take(2) {
                let source = h.get("source").and_then(serde_json::Value::as_str).unwrap_or("");
                let summary = h.get("summary").and_then(serde_json::Value::as_str).unwrap_or("");
                if !source.is_empty() && !summary.is_empty() {
                    suggestions.push(serde_json::json!({
                        "action": "ledger_append", "kind": "validation_scenario", "symbol": source,
                        "suggested_summary": format!("validate that hazard '{}' was not triggered", summary),
                    }));
                }
            }
            for eff in effects_list.iter().take(2) {
                let source = eff.get("source").and_then(serde_json::Value::as_str).unwrap_or("");
                let cat = eff.get("category").and_then(serde_json::Value::as_str).unwrap_or("");
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
        let layers_present_cl: std::collections::HashSet<&str> = files_to_inspect.iter()
            .filter_map(|f| f.get("layer").and_then(serde_json::Value::as_str))
            .collect();
        let ambiguous_terms_cl = detect_ambiguous_tokens(&tokens, &db_path, &filters);
        let possible_misses_cl = detect_possible_misses(&p.query, &layers_present_cl, files_to_inspect.len());
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
        let db_path = self.db_path.clone();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let effect_store = AsgEffectStore { repo: &engine.repo };

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
            if depth >= max_depth { continue; }
            let neighbors = index.get_callers(&ref_name, &sym_id).unwrap_or_default();
            for nbr_id in neighbors {
                if visited.contains(&nbr_id) { continue; }
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

        // Collect invariants/hazards from target + all callers.
        let all_sym_ids: Vec<String> = std::iter::once(symbol.symbol_id.clone())
            .chain(visited.iter().cloned())
            .collect();
        let mut all_invariants: Vec<serde_json::Value> = Vec::new();
        let mut all_hazards: Vec<serde_json::Value> = Vec::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        for sym_id in &all_sym_ids {
            let entries = ledger_store.list_entries(&ref_name, sym_id).unwrap_or_default();
            for entry in entries {
                let key = entry.summary.clone();
                match entry.kind {
                    LedgerKind::Invariant => {
                        if seen_inv.insert(key) {
                            let mut v = serde_json::to_value(&entry).unwrap_or_default();
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert("source_symbol_id".to_string(), serde_json::json!(sym_id));
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
                    _ => {}
                }
            }
        }

        // Effects for the target symbol.
        let effects = effect_store.get_effects(&ref_name, &symbol.symbol_id).unwrap_or(None);

        // Recent git touches.
        let git_depth = p.git_depth.max(1) as usize;
        let recently_touched = mcp_git_recent_touches(&touched_files, git_depth);

        let mut sym_val = serde_json::to_value(&symbol).unwrap_or_default();
        if let Some(obj) = sym_val.as_object_mut() { obj.remove("body"); }

        serde_json::to_string(&serde_json::json!({
            "symbol": sym_val,
            "layer": layer,
            "caller_count": caller_rows.len(),
            "test_count": affected_test_rows.len(),
            "invariants": all_invariants,
            "hazards": all_hazards,
            "effects": effects,
            "callers": caller_rows,
            "affected_tests": affected_test_rows,
            "recently_touched": recently_touched,
        })).unwrap_or_else(|_| "{}".to_string())
    }

    /// Symbols in files changed since a commit + combined blast radius.
    /// PR-review workflow: pass the base SHA to get full impact without knowing any symbol names.
    #[tool(description = "Symbols in files changed since a commit and their combined blast radius. Pass the base SHA of a branch/PR to discover all symbols touched by the diff, their transitive callers, affected tests, invariants, hazards, and effects — without needing to know any symbol names upfront.")]
    async fn since(&self, params: Parameters<SinceParams>) -> String {
        let p = params.0;
        let db_path = self.db_path.clone();
        let layer_overrides = load_layer_overrides(&db_path);
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let effect_store = AsgEffectStore { repo: &engine.repo };

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
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).lines()
                        .filter(|l| !l.is_empty()).map(|l| l.to_string()).collect()
                }
                _ => vec![],
            }
        };

        if changed_files.is_empty() {
            return serde_json::to_string(&serde_json::json!({
                "sha": p.sha, "changed_files": [], "touched_symbols": {},
                "callers": [], "affected_tests": [], "invariants": [], "hazards": [], "effects": [],
            })).unwrap_or_else(|_| "{}".to_string());
        }

        let changed_set: HashSet<&str> = changed_files.iter().map(String::as_str).collect();

        // Seeds: all symbols in changed files.
        let seed_ids: Vec<String> = id_map.values()
            .filter(|s| changed_set.contains(s.file.as_str()))
            .map(|s| s.symbol_id.clone())
            .collect();

        // Group touched symbols by layer.
        let mut by_layer: std::collections::HashMap<String, Vec<serde_json::Value>> = std::collections::HashMap::new();
        for sid in &seed_ids {
            if let Some(s) = id_map.get(sid) {
                let tier = symbol_tier(&s.file);
                let layer = classify_layer_sym(&s.file, &s.qname, tier, &layer_overrides);
                by_layer.entry(layer.to_string()).or_default().push(serde_json::json!({
                    "qname": s.qname, "file": s.file, "line": s.start.line, "layer": layer,
                }));
            }
        }

        // BFS blast radius.
        let max_depth = p.depth.max(1) as usize;
        let mut visited: HashSet<String> = seed_ids.iter().cloned().collect();
        let mut queue: VecDeque<(String, usize)> = seed_ids.iter().map(|id| (id.clone(), 0)).collect();
        let mut caller_rows: Vec<serde_json::Value> = Vec::new();
        let mut affected_test_rows: Vec<serde_json::Value> = Vec::new();
        let mut touched_files: Vec<(String, usize)> = changed_files.iter().map(|f| (f.clone(), 0)).collect();
        let mut seen_files: HashSet<String> = changed_files.iter().cloned().collect();

        while let Some((sym_id, depth)) = queue.pop_front() {
            if depth >= max_depth { continue; }
            let neighbors = index.get_callers(&ref_name, &sym_id).unwrap_or_default();
            for nbr_id in neighbors {
                if visited.contains(&nbr_id) { continue; }
                visited.insert(nbr_id.clone());
                if let Some(s) = id_map.get(&nbr_id) {
                    let t = symbol_tier(&s.file);
                    let l = classify_layer_sym(&s.file, &s.qname, t, &layer_overrides);
                    let row = serde_json::json!({
                        "qname": s.qname, "file": s.file, "line": s.start.line,
                        "depth": depth + 1, "layer": l,
                    });
                    if t == 2 { affected_test_rows.push(row); } else { caller_rows.push(row); }
                    if seen_files.insert(s.file.clone()) {
                        touched_files.push((s.file.clone(), depth + 1));
                    }
                    if depth + 1 < max_depth { queue.push_back((nbr_id, depth + 1)); }
                }
            }
        }

        // Aggregate invariants/hazards/effects from seeds.
        let mut all_invariants: Vec<serde_json::Value> = Vec::new();
        let mut all_hazards: Vec<serde_json::Value> = Vec::new();
        let mut all_effects: Vec<serde_json::Value> = Vec::new();
        let mut seen_inv: HashSet<String> = HashSet::new();
        for sym_id in &seed_ids {
            let entries = ledger_store.list_entries(&ref_name, sym_id).unwrap_or_default();
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
                        all_hazards.push(serde_json::json!({ "summary": entry.summary, "source": sym_qname }));
                    }
                    _ => {}
                }
            }
            if let Ok(Some(decl)) = effect_store.get_effects(&ref_name, sym_id) {
                let qn = id_map.get(sym_id).map(|s| s.qname.clone()).unwrap_or_default();
                for eff in &decl.declared {
                    all_effects.push(serde_json::json!({ "category": format!("{:?}", eff.effect), "source": qn }));
                }
            }
        }

        let git_depth = p.git_depth.max(1) as usize;
        let recently_touched = mcp_git_recent_touches(&touched_files[..touched_files.len().min(5)], git_depth);

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
        })).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Record an invariant that must hold at a symbol. Shortcut for `ledger_append` with kind=invariant. Invariants appear in investigate, checklist, and prepare_change outputs — record them here so future agents see them."
    )]
    async fn invariant_add(&self, params: Parameters<InvariantAddParams>) -> String {
        let p = params.0;
        let Ok(engine) = Engine::open_sqlite(&self.db_path) else {
            return err_json("failed to open database");
        };
        let index_store = AsgIndexStore { repo: &engine.repo };
        let Ok(Some(symbol)) = index_store.get_symbol_by_qname(&engine.ref_name, &p.qname) else {
            return err_json(&format!("symbol not found: {}", p.qname));
        };
        let author = Author { kind: AuthorKind::Agent, id: p.author_id.clone() };
        let entry = LedgerEntry::new(
            &symbol.symbol_id,
            LedgerKind::Invariant,
            p.summary.clone(),
            author,
        );
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        match ledger_store.append_entry(&engine.ref_name, &entry, "asd-mcp") {
            Ok(_) => serde_json::to_string(&serde_json::json!({
                "status": "added",
                "entry_id": entry.entry_id,
                "symbol_id": entry.symbol_id,
                "qname": p.qname,
                "summary": p.summary,
            })).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List invariants recorded against symbols. Pass qname to filter to one symbol; omit to list all invariants in the index."
    )]
    async fn invariant_list(&self, params: Parameters<InvariantListParams>) -> String {
        let p = params.0;
        let Ok(engine) = Engine::open_sqlite(&self.db_path) else {
            return err_json("failed to open database");
        };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };

        let rows: Vec<serde_json::Value> = if let Some(qname) = p.qname {
            let index_store = AsgIndexStore { repo: &engine.repo };
            match index_store.get_symbol_by_qname(&engine.ref_name, &qname) {
                Ok(Some(symbol)) => {
                    ledger_store
                        .list_entries(&engine.ref_name, &symbol.symbol_id)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| e.kind == LedgerKind::Invariant)
                        .map(|e| serde_json::json!({
                            "entry_id": e.entry_id,
                            "qname": qname,
                            "summary": e.summary,
                            "created_at": e.created_at,
                            "tags": e.tags,
                        }))
                        .collect()
                }
                _ => return err_json(&format!("symbol not found: {}", qname)),
            }
        } else {
            let ref_name = &engine.ref_name;
            let tree = match engine.repo.get_tree(ref_name, "/asd/v1/ledger") {
                Ok(v) => v,
                _ => return serde_json::to_string(&serde_json::json!({ "invariants": [] }))
                    .unwrap_or_else(|_| "{}".to_string()),
            };
            let index_store_all = AsgIndexStore { repo: &engine.repo };
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
                            if let Ok(e) = serde_json::from_value::<LedgerEntry>(entry_val.clone()) {
                                if e.kind == LedgerKind::Invariant {
                                    let qname = id_map.get(&e.symbol_id)
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
                a.get("qname").and_then(serde_json::Value::as_str)
                    .cmp(&b.get("qname").and_then(serde_json::Value::as_str))
            });
            rows
        };

        serde_json::to_string(&serde_json::json!({ "invariants": rows }))
            .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Record a verdict on a search result: useful (good match), noisy (irrelevant), missing (should have appeared), wrong_layer (architectural misclassification). Verdicts are persisted and applied as score adjustments in future searches."
    )]
    async fn feedback_mark(&self, params: Parameters<FeedbackMarkParams>) -> String {
        let p = params.0;
        let verdict = match p.verdict.to_lowercase().as_str() {
            "useful" => FeedbackVerdict::Useful,
            "noisy" => FeedbackVerdict::Noisy,
            "missing" => FeedbackVerdict::Missing,
            "wrong_layer" => FeedbackVerdict::WrongLayer,
            other => return err_json(&format!(
                "unknown verdict {:?}; valid: useful, noisy, missing, wrong_layer", other
            )),
        };
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore { repo: &engine.repo };
        let symbol = match index_store.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let entry_id = format!("fb_{}", uuid::Uuid::new_v4().simple());
        let entry = FeedbackEntry {
            entry_id: entry_id.clone(),
            symbol_id: symbol.symbol_id.clone(),
            symbol_qname: p.qname.clone(),
            query: p.query.to_lowercase().trim().to_string(),
            verdict,
            note: p.note.clone(),
            author: p.author_id.clone(),
            created_at: chrono::Utc::now(),
        };
        let feedback_store = AsgFeedbackStore { repo: &engine.repo };
        match feedback_store.record(&ref_name, &entry, &p.author_id) {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true,
                "entry_id": entry_id,
                "verdict": p.verdict,
                "qname": p.qname,
            })).unwrap_or_else(|_| "{}".to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "Designate a symbol as the canonical source-of-truth for a domain concept. Writes an Ownership ledger entry (3x ranking boost) so future searches for that concept reliably surface this symbol. Use when you know which function/struct truly owns a concept."
    )]
    async fn feedback_promote(&self, params: Parameters<FeedbackPromoteParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index_store = AsgIndexStore { repo: &engine.repo };
        let symbol = match index_store.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let author_kind = if p.author_id.contains("human") { AuthorKind::Human } else { AuthorKind::Agent };
        let mut entry = LedgerEntry::new(
            &symbol.symbol_id,
            LedgerKind::Ownership,
            &p.concept,
            Author { kind: author_kind, id: p.author_id.clone() },
        );
        entry.tags = vec!["promote-as-truth".to_string()];
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        match ledger_store.append_entry(&ref_name, &entry, &p.author_id) {
            Ok(()) => serde_json::to_string(&serde_json::json!({
                "ok": true,
                "entry_id": entry.entry_id,
                "qname": p.qname,
                "concept": p.concept,
                "kind": "ownership",
            })).unwrap_or_else(|_| "{}".to_string()),
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
        let feedback_store = AsgFeedbackStore { repo: &engine.repo };
        let entries: Vec<serde_json::Value> = if let Some(ref qname) = p.qname {
            let index_store = AsgIndexStore { repo: &engine.repo };
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
            .args(["log", "--follow", &format!("-n{git_depth}"),
                   "--pretty=format:%H\x1f%an\x1f%ad\x1f%s", "--date=short", "--", file])
            .output();
        let commits: Vec<serde_json::Value> = match output {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).lines()
                    .filter(|l| !l.is_empty())
                    .filter_map(|line| {
                        let p: Vec<&str> = line.splitn(4, '\x1f').collect();
                        if p.len() == 4 {
                            Some(serde_json::json!({
                                "sha": &p[0][..8.min(p[0].len())],
                                "author": p[1], "date": p[2], "message": p[3],
                            }))
                        } else { None }
                    }).collect()
            }
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
        other => Err(format!(
            "unknown ledger kind: {}. Valid: decision, assumption, constraint, rationale, hazard, tradeoff, invariant, ownership, proof, validation_scenario, known_bug",
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
    let prefix = format!("{}/index/by-qname", agentstatedeveloper_core::ASD_PATH_PREFIX);
    let tree = match engine.repo.get_tree(ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let qnames: Vec<String> = match tree {
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        _ => return Ok(Vec::new()),
    };
    let index = AsgIndexStore { repo: &engine.repo };
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

