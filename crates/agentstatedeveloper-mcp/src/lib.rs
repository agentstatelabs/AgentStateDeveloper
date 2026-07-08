//! HTTP server for AgentStateDeveloper (ASD) — powers the M2 Lens consumer.
//!
//! Exposes a read-only JSON API over the ASG-backed ASD state. See
//! [`build_router`] for the full route surface. Binary entrypoint lives in
//! `src/bin/asd-serve.rs`.
//!
//! Also hosts the MCP stdio server module [`mcp_server`] — wired up by the
//! `asd-mcp` binary.

pub mod events;
pub mod mcp_params;
pub mod mcp_server;

pub use mcp_server::AsdMcpServer;

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::{
    AsdError, AsgEffectStore, AsgIndexStore, AsgLedgerStore, AuditEvent, EffectStore, Engine,
    FtsFilters, IndexStore, LedgerStore, declared_effect_blast_radius, emit_audit, event_types,
    explain_match, find_candidates, kind_str, list_all_effect_decls, parse_query,
};
use axum::http::HeaderValue;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

/// Engine is shared across handlers. The underlying `Repository` holds a
/// `Box<dyn Storage>` trait object that is not guaranteed Send+Sync at the
/// trait-object level, so we serialize access through an async Mutex.
pub type SharedEngine = Arc<Mutex<Engine>>;

#[derive(Clone)]
pub struct AppState {
    pub engine: SharedEngine,
    pub db_path: PathBuf,
    /// Optional audit log path. When Some, `/api/v1/audit` reads events
    /// from this JSONL file; otherwise the endpoint returns an empty
    /// list with a `configured: false` flag.
    pub audit_log_path: Option<PathBuf>,
    /// Live-events hub backing `/api/v1/events` (SSE). Spawns its poller
    /// lazily on the first subscriber — see [`events::EventHub`].
    pub events: Arc<events::EventHub>,
    /// Memoized `symbol_id → Symbol` map keyed by the ASG ref head, shared by
    /// the graph endpoints (`/callers`, `/callees`, `/graph`). On a cold FTS
    /// cache `build_id_map` falls back to a full by-qname tree walk (~2s at
    /// 10k symbols); this memo makes that a once-per-head cost instead of a
    /// once-per-request cost. `repo.head` is the same cheap change cursor the
    /// events poller uses, so writes landed by other processes invalidate the
    /// memo on the next request.
    id_map_memo: Arc<std::sync::Mutex<IdMapMemo>>,
    /// Same memo scheme for the bulk call-edge maps `(callers_of,
    /// callees_of)` used by the `/graph` BFS — one bulk read per head change
    /// instead of one git read per visited node.
    edge_maps_memo: Arc<std::sync::Mutex<EdgeMapsMemo>>,
}

type IdMapMemo = Option<(
    agentstategraph_core::ObjectId,
    Arc<std::collections::HashMap<String, agentstatedeveloper_core::Symbol>>,
)>;

/// `(callers_of, callees_of)`, each `symbol_id → [neighbor_id, …]`.
type EdgeMaps = (
    std::collections::HashMap<String, Vec<String>>,
    std::collections::HashMap<String, Vec<String>>,
);

type EdgeMapsMemo = Option<(agentstategraph_core::ObjectId, Arc<EdgeMaps>)>;

/// Fetch the shared `symbol_id → Symbol` map, rebuilding only when the ref
/// head has moved since the last build. Falls back to an unmemoized build
/// when the head can't be resolved — never worse than one bulk read per
/// request. Callers hold the engine mutex, so the brief std-mutex locks here
/// never contend across an `.await`.
fn shared_id_map(
    state: &AppState,
    engine: &Engine,
) -> Arc<std::collections::HashMap<String, agentstatedeveloper_core::Symbol>> {
    let head = engine.repo.head(&engine.ref_name).ok();
    if let Some(h) = &head {
        if let Some((cached_head, map)) = state
            .id_map_memo
            .lock()
            .expect("id_map_memo poisoned")
            .as_ref()
        {
            if cached_head == h {
                return Arc::clone(map);
            }
        }
    }
    let index = AsgIndexStore::from_engine(engine);
    let map = Arc::new(index.build_id_map(engine));
    if let Some(h) = head {
        *state.id_map_memo.lock().expect("id_map_memo poisoned") = Some((h, Arc::clone(&map)));
    }
    map
}

/// Fetch the shared bulk call-edge maps, rebuilding only when the ref head
/// has moved. Same contract as [`shared_id_map`].
fn shared_edge_maps(state: &AppState, engine: &Engine) -> Arc<EdgeMaps> {
    let head = engine.repo.head(&engine.ref_name).ok();
    if let Some(h) = &head {
        if let Some((cached_head, maps)) = state
            .edge_maps_memo
            .lock()
            .expect("edge_maps_memo poisoned")
            .as_ref()
        {
            if cached_head == h {
                return Arc::clone(maps);
            }
        }
    }
    let index = AsgIndexStore::from_engine(engine);
    let maps = Arc::new(index.build_edge_maps(engine));
    if let Some(h) = head {
        *state
            .edge_maps_memo
            .lock()
            .expect("edge_maps_memo poisoned") = Some((h, Arc::clone(&maps)));
    }
    maps
}

/// Build the axum router wired to the given engine + db path.
///
/// If `lens_dir` is Some and the directory exists, it's mounted as a static
/// fallback so the Lens SPA can be served alongside the API. Otherwise
/// unmatched non-API requests 404 gracefully.
///
/// `cors_permissive`: when true, allows any origin (dev convenience). When
/// false (the default), only `http://localhost:*` and `http://127.0.0.1:*`
/// are permitted. Set via `ASD_CORS_PERMISSIVE=1` or `--cors-permissive`.
pub fn build_router(
    engine: SharedEngine,
    db_path: PathBuf,
    lens_dir: Option<PathBuf>,
    audit_log_path: Option<PathBuf>,
    cors_permissive: bool,
) -> Router {
    let events = events::EventHub::new(&engine, audit_log_path.clone());
    let state = AppState {
        engine,
        db_path,
        audit_log_path,
        events,
        id_map_memo: Arc::new(std::sync::Mutex::new(None)),
        edge_maps_memo: Arc::new(std::sync::Mutex::new(None)),
    };

    let api = Router::new()
        .route("/health", get(health))
        .route("/events", get(events_stream))
        .route("/search", get(search_symbols))
        .route("/effects/overview", get(effects_overview))
        .route("/timeline", get(list_timeline))
        .route("/symbols", get(list_symbols))
        .route("/symbols/{qname}", get(get_symbol))
        .route("/symbols/{qname}/graph", get(get_symbol_graph))
        .route("/symbols/{qname}/ledger", get(get_symbol_ledger))
        .route("/symbols/{qname}/effects", get(get_symbol_effects))
        .route("/symbols/{qname}/callers", get(get_symbol_callers))
        .route("/symbols/{qname}/callees", get(get_symbol_callees))
        .route("/ledger", get(list_ledger))
        .route("/thinking", get(list_thinking))
        .route("/symbols/{qname}/thinking", get(get_symbol_thinking))
        .route("/audit", get(list_audit))
        .route("/audit/verify", get(verify_audit))
        .route(
            "/approvals/{entry_id}/approve",
            axum::routing::post(approve_entry),
        )
        .route(
            "/approvals/{entry_id}/reject",
            axum::routing::post(reject_entry),
        )
        .route(
            "/approvals/{entry_id}/withdraw",
            axum::routing::post(withdraw_entry),
        );

    let mut router = Router::new().nest("/api/v1", api);

    if let Some(dir) = lens_dir {
        if dir.exists() {
            // SPA fallback: ServeDir handles static assets; its own
            // `.fallback()` takes a service, and `ServeFile` serves one
            // specific file regardless of the request URL. So any path
            // that doesn't resolve to a static asset falls through to
            // index.html and client-side routing (e.g. /audit,
            // /approvals) takes over.
            let index = dir.join("index.html");
            router = router.fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(index)));
        }
    }

    let cors = if cors_permissive {
        CorsLayer::permissive()
    } else {
        CorsLayer::new().allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            let b = origin.as_bytes();
            b.starts_with(b"http://localhost") || b.starts_with(b"http://127.0.0.1")
        }))
    };

    router
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}

// -----------------------------------------------------------------------------
// Error type
// -----------------------------------------------------------------------------

pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

impl From<agentstatedeveloper_core::AsdError> for ApiError {
    fn from(e: agentstatedeveloper_core::AsdError) -> Self {
        ApiError::Internal(e.to_string())
    }
}

// -----------------------------------------------------------------------------
// Handlers
// -----------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    db_path: String,
    symbol_count: usize,
}

async fn health(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let prefix = format!(
        "{}/index/by-qname",
        agentstatedeveloper_core::ASD_PATH_PREFIX
    );
    let symbol_count = match engine.repo.get_tree(&ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map.len(),
        _ => 0,
    };
    let abs = state
        .db_path
        .canonicalize()
        .unwrap_or(state.db_path.clone())
        .to_string_lossy()
        .to_string();
    Ok(Json(HealthResponse {
        status: "ok",
        db_path: abs,
        symbol_count,
    }))
}

#[derive(Debug, Deserialize)]
struct SymbolQuery {
    /// Maximum symbols to return (default 500, max 2000).
    #[serde(default)]
    limit: Option<usize>,
    /// Zero-based offset for pagination (default 0).
    #[serde(default)]
    offset: Option<usize>,
}

async fn list_symbols(
    State(state): State<AppState>,
    Query(q): Query<SymbolQuery>,
) -> Result<Json<Vec<agentstatedeveloper_core::Symbol>>, ApiError> {
    let engine = state.engine.lock().await;
    // One bulk read (FTS cache when warm, one tree walk otherwise) instead
    // of a per-qname lookup — the per-qname form measured 8m45s per page on
    // a 9.6k-symbol repo with a cold cache, holding the engine mutex the
    // whole time (Plan T t-007 finding).
    let index = AsgIndexStore::from_engine(&engine);
    let mut symbols: Vec<agentstatedeveloper_core::Symbol> =
        index.build_id_map(&engine).into_values().collect();

    symbols.sort_by(|a, b| a.qname.cmp(&b.qname));

    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(500).min(2000);
    let page: Vec<_> = symbols.into_iter().skip(offset).take(limit).collect();
    Ok(Json(page))
}

#[derive(Serialize)]
struct SymbolDetail {
    symbol: agentstatedeveloper_core::Symbol,
    effects: Option<agentstatedeveloper_core::EffectDecl>,
    ledger: Vec<agentstatedeveloper_core::LedgerEntry>,
}

async fn get_symbol(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<SymbolDetail>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    let index = AsgIndexStore::from_engine(&engine);
    let symbol = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;

    let effects_store = AsgEffectStore::from_engine(&engine);
    let effects = effects_store
        .get_effects(&ref_name, &symbol.symbol_id)
        .map_err(ApiError::from)?;

    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let mut ledger = ledger_store
        .list_entries(&ref_name, &symbol.symbol_id)
        .map_err(ApiError::from)?;
    ledger.truncate(20);

    Ok(Json(SymbolDetail {
        symbol,
        effects,
        ledger,
    }))
}

async fn get_symbol_ledger(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<Vec<agentstatedeveloper_core::LedgerEntry>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    let index = AsgIndexStore::from_engine(&engine);
    let symbol = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;

    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let entries = ledger_store
        .list_entries(&ref_name, &symbol.symbol_id)
        .map_err(ApiError::from)?;
    Ok(Json(entries))
}

async fn get_symbol_callers(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<Vec<agentstatedeveloper_core::Symbol>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let index = AsgIndexStore::from_engine(&engine);
    let target = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;
    let ids = index
        .get_callers(&ref_name, &target.symbol_id)
        .map_err(ApiError::from)?;
    let syms = resolve_symbols_by_ids(&state, &engine, &ids);
    Ok(Json(syms))
}

async fn get_symbol_callees(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<Vec<agentstatedeveloper_core::Symbol>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let index = AsgIndexStore::from_engine(&engine);
    let target = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;
    let ids = index
        .get_callees(&ref_name, &target.symbol_id)
        .map_err(ApiError::from)?;
    let syms = resolve_symbols_by_ids(&state, &engine, &ids);
    Ok(Json(syms))
}

/// Resolve a list of `symbol_id`s to full `Symbol` records via the shared
/// (head-memoized) id map — one bulk read per head change, O(ids) per call.
/// The previous implementation listed every qname and then re-fetched each
/// symbol with a per-qname `get_symbol_by_qname` — ~10k git reads per request
/// on a cold cache (measured in minutes on the ExampleProj db); same disease
/// 91b2aaf cured in `list_symbols`.
fn resolve_symbols_by_ids(
    state: &AppState,
    engine: &Engine,
    ids: &[String],
) -> Vec<agentstatedeveloper_core::Symbol> {
    let id_map = shared_id_map(state, engine);
    let mut out: Vec<agentstatedeveloper_core::Symbol> = {
        let mut seen = std::collections::HashSet::new();
        ids.iter()
            .filter(|id| seen.insert(id.as_str()))
            .filter_map(|id| id_map.get(id).cloned())
            .collect()
    };
    out.sort_by(|a, b| a.qname.cmp(&b.qname));
    out
}

#[derive(Debug, Deserialize)]
struct LedgerQuery {
    tag: Option<String>,
    /// Comma-separated `LedgerKind` names in snake_case (e.g.
    /// `hypothesis,mental_model`). Unknown tokens are ignored — they can't
    /// match any entry anyway, and we don't want a typo to 400 the whole
    /// listing.
    kind: Option<String>,
    /// Maximum entries to return (default 100, max 1000).
    #[serde(default)]
    limit: Option<usize>,
    /// Zero-based offset for pagination (default 0).
    #[serde(default)]
    offset: Option<usize>,
}

/// Flat cross-symbol ledger listing. Optional `?tag=<name>` filters to entries
/// carrying that tag (e.g. `awaiting-approval`). Walks the `/asd/v1/ledger`
/// subtree directly so we don't have to re-resolve every symbol_id.
async fn list_ledger(
    State(state): State<AppState>,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<Vec<agentstatedeveloper_core::LedgerEntry>>, ApiError> {
    let engine = state.engine.lock().await;
    let mut entries = all_ledger_entries(&engine);

    if let Some(tag) = q.tag.as_deref() {
        entries.retain(|e| e.tags.iter().any(|t| t == tag));
    }

    apply_kind_filter(&mut entries, q.kind.as_deref());

    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(1000);
    let page: Vec<_> = entries.into_iter().skip(offset).take(limit).collect();
    Ok(Json(page))
}

/// Walk the `/asd/v1/ledger` subtree and return every entry across all
/// symbols. Shared by `list_ledger` and `list_timeline` so both stay in
/// lockstep on how the flat cross-symbol listing is produced.
fn all_ledger_entries(engine: &Engine) -> Vec<agentstatedeveloper_core::LedgerEntry> {
    let prefix = format!("{}/ledger", agentstatedeveloper_core::ASD_PATH_PREFIX);
    let tree = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };

    let mut entries: Vec<agentstatedeveloper_core::LedgerEntry> = Vec::new();
    if let serde_json::Value::Object(by_symbol) = tree {
        for (_symbol_id, symbol_bucket) in by_symbol {
            if let serde_json::Value::Object(entry_map) = symbol_bucket {
                for (_entry_id, entry_val) in entry_map {
                    if let Ok(e) =
                        serde_json::from_value::<agentstatedeveloper_core::LedgerEntry>(entry_val)
                    {
                        entries.push(e);
                    }
                }
            }
        }
    }
    entries
}

/// Apply a comma-separated `LedgerKind` filter (snake_case names, e.g.
/// `hypothesis,mental_model`) to an entry list. Unknown tokens are ignored —
/// they can't match any entry anyway, and we don't want a typo to 400 the
/// whole listing. If ONLY unknown tokens were passed, the list is cleared
/// rather than returned unfiltered, otherwise the filter is silent.
fn apply_kind_filter(entries: &mut Vec<agentstatedeveloper_core::LedgerEntry>, raw: Option<&str>) {
    let Some(raw) = raw else { return };
    let allowed: Vec<agentstatedeveloper_core::LedgerKind> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            // LedgerKind serializes snake_case, so round-trip via JSON
            // to parse the user's token without writing a hand-rolled
            // match against every variant.
            serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
        })
        .collect();
    if !allowed.is_empty() {
        entries.retain(|e| allowed.contains(&e.kind));
    } else {
        entries.clear();
    }
}

// -----------------------------------------------------------------------------
// Plan G / K — captured "thinking" projection
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ThinkingQuery {
    /// Comma-separated qnames to scan. When omitted, scans every qname in
    /// the by-qname index — fine on real-world workspaces (low thousands of
    /// symbols), but explicit qnames are always cheaper.
    qnames: Option<String>,
    /// Drop Hypothesis entries below this confidence. Defaults to
    /// [`agentstatedeveloper_core::thinking::DEFAULT_CONFIDENCE_FLOOR`]
    /// (0.3) so the floor stays consistent with the CLI / MCP.
    min_confidence: Option<f64>,
}

/// `GET /api/v1/thinking?qnames=a,b,c&min_confidence=0.3` — projects Plan G
/// thinking entries (hypotheses / mental models / open questions / failed
/// attempts) for the given qnames. With no `qnames`, scans every indexed
/// symbol. Response body is the `PriorThinking { entries, summary }` shape;
/// callers use `summary.surfaced > 0` to decide whether to render.
async fn list_thinking(
    State(state): State<AppState>,
    Query(q): Query<ThinkingQuery>,
) -> Result<Json<agentstatedeveloper_core::thinking::PriorThinking>, ApiError> {
    let engine = state.engine.lock().await;
    let floor = q
        .min_confidence
        .unwrap_or(agentstatedeveloper_core::thinking::DEFAULT_CONFIDENCE_FLOOR);
    let projection = match q.qnames.as_deref() {
        Some(raw) => {
            let qnames = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            agentstatedeveloper_core::thinking::gather_prior_thinking(&engine, &qnames, floor)
        }
        // Workspace-wide: the bulk single-walk variant, not a synthetic
        // "every qname" list (that form is quadratic — Plan T t-007).
        None => agentstatedeveloper_core::thinking::gather_prior_thinking_all(&engine, floor),
    };
    Ok(Json(projection))
}

#[derive(Debug, Deserialize)]
struct SymbolThinkingQuery {
    min_confidence: Option<f64>,
}

/// `GET /api/v1/symbols/{qname}/thinking?min_confidence=…` — same projection
/// scoped to one symbol. Lets the symbol detail page show an "inherited
/// thinking" panel without pulling the whole workspace.
async fn get_symbol_thinking(
    State(state): State<AppState>,
    Path(qname): Path<String>,
    Query(q): Query<SymbolThinkingQuery>,
) -> Result<Json<agentstatedeveloper_core::thinking::PriorThinking>, ApiError> {
    let engine = state.engine.lock().await;
    let floor = q
        .min_confidence
        .unwrap_or(agentstatedeveloper_core::thinking::DEFAULT_CONFIDENCE_FLOOR);
    let projection = agentstatedeveloper_core::thinking::gather_prior_thinking(
        &engine,
        std::slice::from_ref(&qname),
        floor,
    );
    Ok(Json(projection))
}

// -----------------------------------------------------------------------------
// Audit log
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AuditQuery {
    /// Substring match on event_type (e.g., `ledger.approve`, `ledger.`
    /// for all ledger events).
    event_type: Option<String>,
    /// Return only events AFTER this `event_id` (exclusive).
    since: Option<String>,
    /// Exact match on actor_id.
    actor: Option<String>,
    /// Exact match on outcome.
    outcome: Option<String>,
    /// Exact match on `subject_id` OR `secondary_id`. Lets consumers pull
    /// "every audit record naming this entry/symbol" server-side instead
    /// of scanning the last N events client-side (Plan I t-034; the
    /// lens-core AccountabilityCard is the primary consumer).
    subject: Option<String>,
    /// Max events to return (default 200, max 1000).
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_audit(
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(path) = state.audit_log_path.as_ref() else {
        return Ok(Json(json!({
            "configured": false,
            "count": 0,
            "events": [],
        })));
    };

    let events = agentstatedeveloper_core::read_jsonl(path)
        .map_err(|e| ApiError::Internal(format!("read audit log: {}", e)))?;

    // Apply `since` cursor (drop up to and including the matching id).
    let start_idx = match q.since {
        Some(ref id) => events
            .iter()
            .position(|e| &e.event_id == id)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };

    let limit = q.limit.unwrap_or(200).min(1000);
    let filtered: Vec<&agentstatedeveloper_core::AuditEvent> = events[start_idx..]
        .iter()
        .filter(|e| {
            if let Some(ref t) = q.event_type {
                if !e.event_type.contains(t) {
                    return false;
                }
            }
            if let Some(ref a) = q.actor {
                if &e.actor_id != a {
                    return false;
                }
            }
            if let Some(ref o) = q.outcome {
                if &e.outcome != o {
                    return false;
                }
            }
            if let Some(ref s) = q.subject {
                if e.subject_id.as_deref() != Some(s.as_str())
                    && e.secondary_id.as_deref() != Some(s.as_str())
                {
                    return false;
                }
            }
            true
        })
        .take(limit)
        .collect();

    Ok(Json(json!({
        "configured": true,
        "path": path.display().to_string(),
        "count": filtered.len(),
        "events": filtered,
    })))
}

async fn verify_audit(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    // Chain verification is a commercial feature (Enterprise tier).
    // OSS asd-serve returns a consistent shape so the Lens can detect
    // edition and render an upgrade CTA without 500-ing.
    Ok(Json(json!({
        "configured": state.audit_log_path.is_some(),
        "edition": "oss",
        "verified": false,
        "error": "audit verify is a commercial feature (Enterprise tier) — install asd-pro",
        "upgrade_url": "https://agentstatedeveloper.dev/pricing",
    })))
}

/// Body for POST /approvals/:entry_id/approve.
#[derive(Debug, Deserialize)]
struct ApproveBody {
    /// Approver id — recorded as `approved-by:<id>`.
    approver: String,
    /// Approver kind — must match one of the original `approver:*` tags.
    #[serde(default = "default_approver_kind")]
    approver_kind: String,
    /// Optional commit agent id. Defaults to "asd-http".
    #[serde(default = "default_http_agent_id")]
    agent_id: String,
    /// Optional rationale appended to the entry body.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RejectBody {
    reviewer: String,
    #[serde(default = "default_approver_kind")]
    reviewer_kind: String,
    reason: String,
    #[serde(default = "default_http_agent_id")]
    agent_id: String,
}

#[derive(Debug, Deserialize)]
struct WithdrawBody {
    author_id: String,
    #[serde(default = "default_http_agent_id")]
    agent_id: String,
}

fn default_approver_kind() -> String {
    "human".into()
}

fn default_http_agent_id() -> String {
    "asd-http".into()
}

async fn approve_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let result = match engine.ratify.as_ref() {
        Some(ratify) => ratify.approve_entry(
            &engine.repo,
            &ref_name,
            &entry_id,
            &body.approver,
            &body.approver_kind,
            body.message.as_deref(),
            &body.agent_id,
        ),
        None => Err(AsdError::Other(
            "ledger approve is a commercial feature (Team tier) — \
             install asd-pro to enable. See https://agentstatedeveloper.dev/pricing"
                .into(),
        )),
    };
    match result {
        Ok(outcome) => {
            let status = if outcome.already_approved {
                "already-approved"
            } else {
                "approved"
            };
            let event = AuditEvent::new(
                event_types::LEDGER_APPROVE,
                &body.approver,
                &body.approver_kind,
                status,
            )
            .with_subject(&outcome.entry.entry_id)
            .with_secondary(&outcome.entry.symbol_id)
            .with_matched_policy(outcome.entry.matched_policy.clone())
            .with_payload(json!({ "tags": outcome.entry.tags }));
            emit_audit(engine.audit.as_ref(), event);

            Ok(Json(json!({
                "status": status,
                "entry": outcome.entry,
            })))
        }
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_APPROVE,
                &body.approver,
                &body.approver_kind,
                "error",
            )
            .with_subject(&entry_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(ApiError::from(e))
        }
    }
}

async fn reject_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
    Json(body): Json<RejectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let result = match engine.ratify.as_ref() {
        Some(ratify) => ratify.reject_entry(
            &engine.repo,
            &ref_name,
            &entry_id,
            &body.reviewer,
            &body.reviewer_kind,
            &body.reason,
            &body.agent_id,
        ),
        None => Err(AsdError::Other(
            "ledger reject is a commercial feature (Team tier) — \
             install asd-pro to enable. See https://agentstatedeveloper.dev/pricing"
                .into(),
        )),
    };
    match result {
        Ok(outcome) => {
            let status = if outcome.already_resolved {
                "already-rejected"
            } else {
                "rejected"
            };
            let event = AuditEvent::new(
                event_types::LEDGER_REJECT,
                &body.reviewer,
                &body.reviewer_kind,
                status,
            )
            .with_subject(&outcome.entry.entry_id)
            .with_secondary(&outcome.entry.symbol_id)
            .with_matched_policy(outcome.entry.matched_policy.clone())
            .with_reason(&body.reason)
            .with_payload(json!({ "tags": outcome.entry.tags }));
            emit_audit(engine.audit.as_ref(), event);

            Ok(Json(json!({
                "status": status,
                "entry": outcome.entry,
            })))
        }
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_REJECT,
                &body.reviewer,
                &body.reviewer_kind,
                "error",
            )
            .with_subject(&entry_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(ApiError::from(e))
        }
    }
}

async fn withdraw_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
    Json(body): Json<WithdrawBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let result = match engine.ratify.as_ref() {
        Some(ratify) => ratify.withdraw_entry(
            &engine.repo,
            &ref_name,
            &entry_id,
            &body.author_id,
            &body.agent_id,
        ),
        None => Err(AsdError::Other(
            "ledger withdraw is a commercial feature (Team tier) — \
             install asd-pro to enable. See https://agentstatedeveloper.dev/pricing"
                .into(),
        )),
    };
    match result {
        Ok(outcome) => {
            let status = if outcome.already_resolved {
                "already-withdrawn"
            } else {
                "withdrawn"
            };
            let event = AuditEvent::new(
                event_types::LEDGER_WITHDRAW,
                &body.author_id,
                "agent",
                status,
            )
            .with_subject(&outcome.entry.entry_id)
            .with_secondary(&outcome.entry.symbol_id)
            .with_payload(json!({ "tags": outcome.entry.tags }));
            emit_audit(engine.audit.as_ref(), event);

            Ok(Json(json!({
                "status": status,
                "entry": outcome.entry,
            })))
        }
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_WITHDRAW,
                &body.author_id,
                "agent",
                "error",
            )
            .with_subject(&entry_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(ApiError::from(e))
        }
    }
}

async fn get_symbol_effects(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<Option<agentstatedeveloper_core::EffectDecl>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    let index = AsgIndexStore::from_engine(&engine);
    let symbol = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;

    let effects_store = AsgEffectStore::from_engine(&engine);
    let effects = effects_store
        .get_effects(&ref_name, &symbol.symbol_id)
        .map_err(ApiError::from)?;
    Ok(Json(effects))
}

// -----------------------------------------------------------------------------
// Plan T t-003 — Lens web-UI endpoints (search / graph / effects / timeline)
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchApiQuery {
    /// Concept or keyword(s) to search for. Same query syntax as
    /// `asd search` (inline `-term` exclusions supported).
    q: String,
    /// Filter by symbol kind: module, function, method, class, variable.
    kind: Option<String>,
    /// Filter by language (e.g. "swift", "python", "rust").
    lang: Option<String>,
    /// Maximum results (default 20, max 100).
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /api/v1/search?q=…&kind=…&lang=…&limit=…` — ranked symbol search
/// backed by the same `find_candidates` pipeline the CLI (`asd search`
/// via prepare_change/investigate) and MCP tools use, so Lens results and
/// scores match agent-side results. `why` carries the core
/// `explain_match` reasons (`name:token`, `file:token`,
/// `invariant-attached:N`, …) — no ranking is invented here.
async fn search_symbols(
    State(state): State<AppState>,
    Query(q): Query<SearchApiQuery>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let query = q.q.trim().to_string();
    if query.is_empty() {
        return Err(ApiError::BadRequest(
            "query parameter `q` must not be empty".into(),
        ));
    }
    let limit = q.limit.unwrap_or(20).clamp(1, 100);

    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let (tokens, exclusions) = parse_query(&query);
    let filters = FtsFilters {
        kind: q.kind.as_deref().map(|k| k.to_lowercase()),
        language: q.lang.as_deref().map(|l| l.to_lowercase()),
        include_tests: false,
        tests_only: false,
        exclude_terms: exclusions,
        paths_filter: Vec::new(),
        exclude_paths: Vec::new(),
        exclude_languages: Vec::new(),
    };

    let index = AsgIndexStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let candidates = find_candidates(
        &engine,
        &query,
        &tokens,
        &filters,
        &ledger_store,
        &index,
        limit,
    );

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(candidates.len());
    for (score, qname) in candidates {
        let sym = match index.get_symbol_by_qname(&ref_name, &qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let entries = ledger_store
            .list_entries(&ref_name, &sym.symbol_id)
            .unwrap_or_default();
        // is_hot=false: recency needs `git log` relative to the indexed
        // checkout's CWD, which the server can't assume. The match reasons
        // and scores are unaffected.
        let why = explain_match(&sym, &tokens, &entries, false);
        let name = sym
            .qname
            .rsplit(|c| c == '.' || c == ':')
            .next()
            .unwrap_or(sym.qname.as_str())
            .to_string();
        results.push(json!({
            "qname": sym.qname,
            "name": name,
            "kind": kind_str(&sym.kind),
            "language": sym.language,
            "file": sym.file,
            "line": sym.start.line,
            "score": score,
            "why": why,
        }));
    }
    Ok(Json(results))
}

#[derive(Debug, Deserialize)]
struct GraphQuery {
    /// BFS depth from the root symbol (1..=3, default 1).
    #[serde(default)]
    hops: Option<usize>,
    /// Which edges to walk: `callers`, `callees`, or `both` (default).
    direction: Option<String>,
}

/// Hard cap on nodes returned by `/symbols/{qname}/graph`. When hit, the
/// response carries `"truncated": true` — never a silent cut.
const GRAPH_NODE_CAP: usize = 500;

/// `GET /api/v1/symbols/{qname}/graph?hops=…&direction=…` — BFS over the
/// call graph from the symbol, in a render-ready nodes/links shape. Node
/// `id`s are the stable `symbol_id`s; links always point caller → callee
/// (`source` calls `target`) regardless of traversal direction, and are
/// deduped.
async fn get_symbol_graph(
    State(state): State<AppState>,
    Path(qname): Path<String>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let hops = q.hops.unwrap_or(1);
    if !(1..=3).contains(&hops) {
        return Err(ApiError::BadRequest(format!(
            "hops must be between 1 and 3, got {}",
            hops
        )));
    }
    let direction = q.direction.as_deref().unwrap_or("both");
    let (want_callers, want_callees) = match direction {
        "callers" => (true, false),
        "callees" => (false, true),
        "both" => (true, true),
        other => {
            return Err(ApiError::BadRequest(format!(
                "direction must be one of callers|callees|both, got {}",
                other
            )));
        }
    };

    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let index = AsgIndexStore::from_engine(&engine);
    let root = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;
    let id_map = shared_id_map(&state, &engine);
    let edge_maps = shared_edge_maps(&state, &engine);
    let (callers_of, callees_of) = edge_maps.as_ref();

    let node_json = |sym: &agentstatedeveloper_core::Symbol| {
        json!({
            "id": sym.symbol_id,
            "qname": sym.qname,
            "kind": kind_str(&sym.kind),
            "file": sym.file,
            "module": qname_module(&sym.qname),
        })
    };

    let mut nodes: Vec<serde_json::Value> = vec![node_json(&root)];
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    visited.insert(root.symbol_id.clone());
    let mut links: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut queue: std::collections::VecDeque<(String, usize)> = std::collections::VecDeque::new();
    queue.push_back((root.symbol_id.clone(), 0));
    let mut truncated = false;

    while let Some((sym_id, depth)) = queue.pop_front() {
        if depth >= hops {
            continue;
        }
        // (neighbor_id, link caller→callee)
        let mut neighbors: Vec<(String, (String, String))> = Vec::new();
        if want_callers {
            for caller in callers_of.get(&sym_id).into_iter().flatten() {
                let link = (caller.clone(), sym_id.clone());
                neighbors.push((caller.clone(), link));
            }
        }
        if want_callees {
            for callee in callees_of.get(&sym_id).into_iter().flatten() {
                let link = (sym_id.clone(), callee.clone());
                neighbors.push((callee.clone(), link));
            }
        }
        for (nbr_id, link) in neighbors {
            // Skip edges whose peer no longer resolves to an indexed symbol
            // (stale edge after a partial reindex) — a dangling link would
            // break the renderer.
            let Some(nbr_sym) = id_map.get(&nbr_id) else {
                continue;
            };
            if !visited.contains(&nbr_id) {
                if visited.len() >= GRAPH_NODE_CAP {
                    truncated = true;
                    continue;
                }
                visited.insert(nbr_id.clone());
                nodes.push(node_json(nbr_sym));
                queue.push_back((nbr_id, depth + 1));
            }
            links.insert(link);
        }
    }

    let links: Vec<serde_json::Value> = links
        .into_iter()
        .map(|(source, target)| json!({ "source": source, "target": target }))
        .collect();

    Ok(Json(json!({
        "root": root.qname,
        "hops": hops,
        "direction": direction,
        "nodes": nodes,
        "links": links,
        "truncated": truncated,
    })))
}

/// Parent path of a qname (`payments.charge_card` → `payments`,
/// `crate::mod::f` → `crate::mod`). `None` for top-level names.
fn qname_module(qname: &str) -> Option<String> {
    if let Some((module, _)) = qname.rsplit_once("::") {
        return Some(module.to_string());
    }
    qname.rsplit_once('.').map(|(m, _)| m.to_string())
}

#[derive(Debug, Deserialize)]
struct EffectsOverviewQuery {
    /// Maximum effect rows to return (default 50, max 500).
    #[serde(default)]
    limit: Option<usize>,
}

/// Top declaring symbols listed per effect row in `/effects/overview`.
const EFFECT_TOP_SYMBOLS: usize = 5;

/// `GET /api/v1/effects/overview?limit=…` — per effect category, the count
/// of symbols declaring it plus the top declarers ranked by transitive
/// blast radius. Radius comes from the stored transitive data written by
/// `propagate_transitive` at index time (walked via
/// `core::declared_effect_blast_radius`) — reachability is not recomputed
/// here. Backs DESIGN.md Plan I t-031.
async fn effects_overview(
    State(state): State<AppState>,
    Query(q): Query<EffectsOverviewQuery>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let limit = q.limit.unwrap_or(50).min(500);
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    let decls = list_all_effect_decls(&engine.repo, &ref_name).map_err(ApiError::from)?;
    let radius_by_effect = declared_effect_blast_radius(&decls);
    let index = AsgIndexStore::from_engine(&engine);
    let id_map = index.build_id_map(&engine);

    let mut rows: Vec<(String, usize, Vec<serde_json::Value>)> = radius_by_effect
        .into_iter()
        .map(|(cat, declarers)| {
            let symbol_count = declarers.len();
            let top_symbols: Vec<serde_json::Value> = declarers
                .iter()
                .take(EFFECT_TOP_SYMBOLS)
                .map(|(symbol_id, blast_radius)| {
                    json!({
                        // Fall back to the raw symbol_id if the declarer is
                        // no longer in the index (stale decl).
                        "qname": id_map
                            .get(symbol_id)
                            .map(|s| s.qname.clone())
                            .unwrap_or_else(|| symbol_id.clone()),
                        "blast_radius": blast_radius,
                    })
                })
                .collect();
            (cat.as_str().to_string(), symbol_count, top_symbols)
        })
        .collect();
    // Busiest effects first; alphabetical tie-break keeps output stable.
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(limit);

    Ok(Json(
        rows.into_iter()
            .map(|(effect, symbol_count, top_symbols)| {
                json!({
                    "effect": effect,
                    "symbol_count": symbol_count,
                    "top_symbols": top_symbols,
                })
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
struct TimelineQuery {
    /// Maximum entries to return (default 100, max 1000).
    #[serde(default)]
    limit: Option<usize>,
    /// Comma-separated `LedgerKind` names in snake_case (e.g.
    /// `hypothesis,decision`). Same semantics as `/ledger`'s `kind` filter.
    kinds: Option<String>,
}

/// `GET /api/v1/timeline?limit=…&kinds=…` — chronological (newest first)
/// merged feed of ledger entries across all symbols. Plan G thinking
/// entries (hypothesis / mental_model / open_question / failed_attempt)
/// ARE ledger kinds, so one walk covers both; use `kinds=` to slice
/// either family.
async fn list_timeline(
    State(state): State<AppState>,
    Query(q): Query<TimelineQuery>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let engine = state.engine.lock().await;

    let mut entries = all_ledger_entries(&engine);
    apply_kind_filter(&mut entries, q.kinds.as_deref());
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let limit = q.limit.unwrap_or(100).min(1000);
    entries.truncate(limit);

    let index = AsgIndexStore::from_engine(&engine);
    let id_map = index.build_id_map(&engine);
    let feed: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            json!({
                "at": e.created_at,
                // LedgerKind serializes snake_case — reuse that for the wire.
                "kind": serde_json::to_value(e.kind).unwrap_or(serde_json::Value::Null),
                "symbol_id": e.symbol_id,
                "qname": id_map.get(&e.symbol_id).map(|s| s.qname.clone()),
                "summary": e.summary,
                "entry_id": e.entry_id,
            })
        })
        .collect();
    Ok(Json(feed))
}

// -----------------------------------------------------------------------------
// Live activity stream (SSE) — see the `events` module for the poller.
// -----------------------------------------------------------------------------

/// `GET /api/v1/events` — Server-Sent Events stream of live repo activity:
/// ledger entries (all kinds, incl. Plan G thinking), effect declarations /
/// verifications, index runs, and audit events. Each `data:` payload is a
/// JSON object whose field names match `/api/v1/timeline`
/// (`{at, kind, qname, symbol_id, entry_id, summary}`) so the Lens "now"
/// feed can merge both sources. Events are detected by polling — worst-case
/// latency is one [`events::POLL_INTERVAL`].
async fn events_stream(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    use tokio_stream::StreamExt;
    let rx = state.events.subscribe().await;
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(data) => Some(Ok(SseEvent::default().data(data))),
        // Slow consumer fell > CHANNEL_CAPACITY events behind: skip the
        // gap and keep streaming rather than killing the connection.
        Err(_lagged) => None,
    });
    Sse::new(stream).keep_alive(
        // Comment frames so idle streams survive proxies/load-balancers
        // that reap quiet connections.
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keep-alive"),
    )
}
