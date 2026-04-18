//! HTTP server for AgentStateDeveloper (ASD) — powers the M2 Lens consumer.
//!
//! Exposes a read-only JSON API over the ASG-backed ASD state. See
//! [`build_router`] for the full route surface. Binary entrypoint lives in
//! `src/bin/asd-serve.rs`.
//!
//! Also hosts the MCP stdio server module [`mcp_server`] — wired up by the
//! `asd-mcp` binary.

pub mod mcp_server;

pub use mcp_server::AsdMcpServer;

use std::path::PathBuf;
use std::sync::Arc;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, IndexStore, LedgerStore,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

/// Engine is shared across handlers. The underlying `Repository` holds a
/// `Box<dyn Storage>` trait object that is not guaranteed Send+Sync at the
/// trait-object level, so we serialize access through an async Mutex.
pub type SharedEngine = Arc<Mutex<Engine>>;

#[derive(Clone)]
pub struct AppState {
    pub engine: SharedEngine,
    pub db_path: PathBuf,
}

/// Build the axum router wired to the given engine + db path.
///
/// If `lens_dir` is Some and the directory exists, it's mounted as a static
/// fallback so the Lens SPA can be served alongside the API. Otherwise
/// unmatched non-API requests 404 gracefully.
pub fn build_router(
    engine: SharedEngine,
    db_path: PathBuf,
    lens_dir: Option<PathBuf>,
) -> Router {
    let state = AppState { engine, db_path };

    let api = Router::new()
        .route("/health", get(health))
        .route("/symbols", get(list_symbols))
        .route("/symbols/{qname}", get(get_symbol))
        .route("/symbols/{qname}/ledger", get(get_symbol_ledger))
        .route("/symbols/{qname}/effects", get(get_symbol_effects))
        .route("/symbols/{qname}/callers", get(get_symbol_callers))
        .route("/symbols/{qname}/callees", get(get_symbol_callees))
        .route("/ledger", get(list_ledger))
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
            router = router.fallback_service(
                ServeDir::new(&dir).fallback(ServeDir::new(dir.join("index.html"))),
            );
        }
    }

    router
        .with_state(state)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

// -----------------------------------------------------------------------------
// Error type
// -----------------------------------------------------------------------------

pub enum ApiError {
    NotFound(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
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
    let prefix = format!("{}/index/by-qname", agentstatedeveloper_core::ASD_PATH_PREFIX);
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

async fn list_symbols(
    State(state): State<AppState>,
) -> Result<Json<Vec<agentstatedeveloper_core::Symbol>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let prefix = format!("{}/index/by-qname", agentstatedeveloper_core::ASD_PATH_PREFIX);

    let qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => return Ok(Json(Vec::new())),
    };

    let index = AsgIndexStore { repo: &engine.repo };
    let mut symbols = Vec::new();
    for qname in qnames {
        if let Some(sym) = index
            .get_symbol_by_qname(&ref_name, &qname)
            .map_err(ApiError::from)?
        {
            symbols.push(sym);
        }
    }

    symbols.sort_by(|a, b| a.qname.cmp(&b.qname));
    Ok(Json(symbols))
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

    let index = AsgIndexStore { repo: &engine.repo };
    let symbol = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;

    let effects_store = AsgEffectStore { repo: &engine.repo };
    let effects = effects_store
        .get_effects(&ref_name, &symbol.symbol_id)
        .map_err(ApiError::from)?;

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
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

    let index = AsgIndexStore { repo: &engine.repo };
    let symbol = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
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
    let index = AsgIndexStore { repo: &engine.repo };
    let target = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;
    let ids = index
        .get_callers(&ref_name, &target.symbol_id)
        .map_err(ApiError::from)?;
    let syms = resolve_symbols_by_ids(&engine, &ids).map_err(ApiError::from)?;
    Ok(Json(syms))
}

async fn get_symbol_callees(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<Vec<agentstatedeveloper_core::Symbol>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let index = AsgIndexStore { repo: &engine.repo };
    let target = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;
    let ids = index
        .get_callees(&ref_name, &target.symbol_id)
        .map_err(ApiError::from)?;
    let syms = resolve_symbols_by_ids(&engine, &ids).map_err(ApiError::from)?;
    Ok(Json(syms))
}

/// Resolve a list of `symbol_id`s to full `Symbol` records by scanning the
/// qname index. O(N) per lookup; acceptable for M4 solo-dev scale.
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

#[derive(Debug, Deserialize)]
struct LedgerQuery {
    tag: Option<String>,
}

/// Flat cross-symbol ledger listing. Optional `?tag=<name>` filters to entries
/// carrying that tag (e.g. `awaiting-approval`). Walks the `/asd/v1/ledger`
/// subtree directly so we don't have to re-resolve every symbol_id.
async fn list_ledger(
    State(state): State<AppState>,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<Vec<agentstatedeveloper_core::LedgerEntry>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let prefix = format!("{}/ledger", agentstatedeveloper_core::ASD_PATH_PREFIX);

    let tree = match engine.repo.get_tree(&ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Ok(Json(Vec::new())),
    };

    let mut entries: Vec<agentstatedeveloper_core::LedgerEntry> = Vec::new();
    if let serde_json::Value::Object(by_symbol) = tree {
        for (_symbol_id, symbol_bucket) in by_symbol {
            if let serde_json::Value::Object(entry_map) = symbol_bucket {
                for (_entry_id, entry_val) in entry_map {
                    if let Ok(e) = serde_json::from_value::<
                        agentstatedeveloper_core::LedgerEntry,
                    >(entry_val)
                    {
                        entries.push(e);
                    }
                }
            }
        }
    }

    if let Some(tag) = q.tag.as_deref() {
        entries.retain(|e| e.tags.iter().any(|t| t == tag));
    }

    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(Json(entries))
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
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store
        .approve_entry(
            &ref_name,
            &entry_id,
            &body.approver,
            &body.approver_kind,
            body.message.as_deref(),
            &body.agent_id,
        )
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "status": if outcome.already_approved { "already-approved" } else { "approved" },
        "entry": outcome.entry,
    })))
}

async fn reject_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
    Json(body): Json<RejectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store
        .reject_entry(
            &ref_name,
            &entry_id,
            &body.reviewer,
            &body.reviewer_kind,
            &body.reason,
            &body.agent_id,
        )
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "status": if outcome.already_resolved { "already-rejected" } else { "rejected" },
        "entry": outcome.entry,
    })))
}

async fn withdraw_entry(
    State(state): State<AppState>,
    Path(entry_id): Path<String>,
    Json(body): Json<WithdrawBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store
        .withdraw_entry(&ref_name, &entry_id, &body.author_id, &body.agent_id)
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "status": if outcome.already_resolved { "already-withdrawn" } else { "withdrawn" },
        "entry": outcome.entry,
    })))
}

async fn get_symbol_effects(
    State(state): State<AppState>,
    Path(qname): Path<String>,
) -> Result<Json<Option<agentstatedeveloper_core::EffectDecl>>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    let index = AsgIndexStore { repo: &engine.repo };
    let symbol = index
        .get_symbol_by_qname(&ref_name, &qname)
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("symbol not found: {}", qname)))?;

    let effects_store = AsgEffectStore { repo: &engine.repo };
    let effects = effects_store
        .get_effects(&ref_name, &symbol.symbol_id)
        .map_err(ApiError::from)?;
    Ok(Json(effects))
}
