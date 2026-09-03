//! Distilled-metrics API — the record ASD keeps of itself, made searchable.
//!
//! Two families live here.
//!
//! **1. What survives a prune.** ASG Plan A distills the commit chain into
//! `asg_history_commit_rollup` (one row per day × namespace × agent ×
//! intent) and `asg_history_milestone` (the named spine — each row pins a
//! `state_root` that Plan B's GC must keep reachable). Plan B's sweep then
//! reclaims the raw objects those rows were derived from. `/api/v1/history`
//! already charts the aggregate; the endpoints here expose the underlying
//! *records* with search, filters and facets, and put the raw commit chain
//! next to them so you can see what a sweep would drop against what the
//! distilled tables preserve.
//!
//! **2. How healthy that record is.** The scorecard and index-freshness
//! surfaces, until now reachable only from the CLI and MCP.
//!
//! All handlers are read-only. `refresh=1` — the default on every endpoint
//! that reads the distilled tables, which includes `/commits` (its `on_spine`
//! join and `distilled` total both come from them) — runs ASG's *incremental*
//! extractor first, so a call made after new commits landed sees them. It
//! no-ops once the cursor is caught up.

use std::collections::{BTreeMap, HashSet};

use agentstatedeveloper_core::{
    AsgFeedbackStore, Engine, FeedbackStore, SearchFtsDb, compute_index_consistency, format_age,
    scorecard as core_scorecard, stale_warning_classified,
};
use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiError, AppState};

/// Batch size for the incremental history extractor. Matches the value
/// `/api/v1/history` passes through `history_report(refresh = true)`.
const HISTORY_BATCH: usize = 5_000;

/// Upper bound on rows pulled out of the distilled tables before filtering.
/// Both are aggregates — a 5.7k-commit repo distills to ~40 rollup rows and
/// ~95 milestones — so this is a runaway guard, not a real ceiling.
const DISTILLED_SCAN_CAP: usize = 200_000;

/// Default number of raw commits walked by `/api/v1/commits` before
/// filtering. The chain is walked parent-by-parent, so this bounds the work
/// a single request can do on a large store. Raise per-request with `scan`.
const COMMIT_SCAN_DEFAULT: usize = 5_000;
const COMMIT_SCAN_MAX: usize = 100_000;

const PAGE_DEFAULT: usize = 100;
const PAGE_MAX: usize = 1_000;

// -----------------------------------------------------------------------------
// Shared helpers
// -----------------------------------------------------------------------------

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1") | Some("true") | Some("yes"))
}

/// `refresh` defaults to on: a Lens page that shows stale records is worse
/// than one that costs an incremental extract.
fn refresh_requested(v: Option<&str>) -> bool {
    match v {
        None => true,
        Some("0") | Some("false") | Some("no") => false,
        _ => true,
    }
}

fn page(limit: Option<usize>, offset: Option<usize>) -> (usize, usize) {
    (
        limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX),
        offset.unwrap_or(0),
    )
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(needle)
}

/// Case-insensitive exact match, used for the categorical filters so
/// `?agent=Craig Brown` and `?agent=craig brown` behave the same.
fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// `day` fields are `YYYY-MM-DD`, so an inclusive range is a lexical compare.
fn in_day_range(day: &str, from: Option<&str>, to: Option<&str>) -> bool {
    from.is_none_or(|f| day >= f) && to.is_none_or(|t| day <= t)
}

/// Rank a facet's values by count (descending), ties broken by name so the
/// UI's filter chips don't reshuffle between identical requests.
fn facet(counts: BTreeMap<String, usize>) -> Vec<Value> {
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter()
        .map(|(value, count)| json!({ "value": value, "count": count }))
        .collect()
}

fn bump(counts: &mut BTreeMap<String, usize>, key: &str, by: usize) {
    *counts.entry(key.to_string()).or_default() += by;
}

/// Run ASG's incremental history extractor. Non-fatal: a store whose engine
/// predates Plan A still serves whatever rows already exist rather than
/// failing the whole request.
fn refresh_history(engine: &Engine) {
    let _ = engine.repo.extract_history(HISTORY_BATCH);
}

// -----------------------------------------------------------------------------
// GET /api/v1/history/milestones — the named spine, searchable
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct MilestoneQuery {
    /// Free text over description, commit id, kind, agent and namespace.
    q: Option<String>,
    kind: Option<String>,
    namespace: Option<String>,
    agent: Option<String>,
    /// Inclusive `YYYY-MM-DD` bounds on the milestone's day.
    from: Option<String>,
    to: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    refresh: Option<String>,
}

/// GET /api/v1/history/milestones — every milestone ASG distilled out of the
/// commit chain, filtered and paginated.
///
/// A milestone is the unit that *names* a state a prune must preserve:
/// `state_root` is the retention hook Plan B's GC keeps reachable. Rows
/// written before Plan A t-005 have `state_root: null` and are reported as
/// `pins_state: false` — they survive as a description but no longer name a
/// snapshot, which is exactly the case an operator wants to see before
/// running a sweep.
///
/// Facets are computed over the set matching `q` + the date range but *not*
/// the categorical filters, so the chip counts stay meaningful while you
/// toggle kind/namespace/agent.
pub async fn list_milestones(
    State(state): State<AppState>,
    Query(q): Query<MilestoneQuery>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    if refresh_requested(q.refresh.as_deref()) {
        refresh_history(&engine);
    }

    let rows = engine
        .repo
        .history_milestones(DISTILLED_SCAN_CAP)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let scanned = rows.len();

    let needle =
        q.q.as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
    let (limit, offset) = page(q.limit, q.offset);

    // Pass 1: text + date. This is the set the facets describe.
    let base: Vec<_> = rows
        .into_iter()
        .filter(|m| in_day_range(&m.day, q.from.as_deref(), q.to.as_deref()))
        .filter(|m| {
            needle.as_deref().is_none_or(|n| {
                contains_ci(&m.description, n)
                    || contains_ci(&m.commit_id.short(), n)
                    || contains_ci(&m.kind, n)
                    || contains_ci(&m.agent_id, n)
                    || contains_ci(&m.namespace, n)
            })
        })
        .collect();

    let mut kinds = BTreeMap::new();
    let mut namespaces = BTreeMap::new();
    let mut agents = BTreeMap::new();
    for m in &base {
        bump(&mut kinds, &m.kind, 1);
        bump(&mut namespaces, &m.namespace, 1);
        bump(&mut agents, &m.agent_id, 1);
    }

    // Pass 2: categorical filters, then page.
    let mut matched: Vec<_> = base
        .into_iter()
        .filter(|m| q.kind.as_deref().is_none_or(|k| eq_ci(&m.kind, k)))
        .filter(|m| {
            q.namespace
                .as_deref()
                .is_none_or(|n| eq_ci(&m.namespace, n))
        })
        .filter(|m| q.agent.as_deref().is_none_or(|a| eq_ci(&m.agent_id, a)))
        .collect();
    // Newest first — the spine reads backwards from now.
    matched.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let total = matched.len();
    let unpinned = matched.iter().filter(|m| m.state_root.is_none()).count();
    let items: Vec<Value> = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|m| {
            json!({
                "commit": m.commit_id.short(),
                "commit_id": m.commit_id.to_string(),
                "kind": m.kind,
                "timestamp": m.timestamp,
                "day": m.day,
                "namespace": m.namespace,
                "agent": m.agent_id,
                "description": m.description,
                "state_root": m.state_root.as_ref().map(|s| s.short()),
                "pins_state": m.state_root.is_some(),
            })
        })
        .collect();

    Ok(Json(json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "scanned": scanned,
        "unpinned": unpinned,
        "items": items,
        "facets": {
            "kinds": facet(kinds),
            "namespaces": facet(namespaces),
            "agents": facet(agents),
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /api/v1/history/rollup — the distilled commit rollup, searchable
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RollupQuery {
    q: Option<String>,
    namespace: Option<String>,
    agent: Option<String>,
    intent: Option<String>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    refresh: Option<String>,
}

/// GET /api/v1/history/rollup — the rows behind the `/history` charts.
///
/// One row per day × namespace × agent × intent category. This is what the
/// velocity series, intent mix and authorship breakdown are summed from, and
/// what remains after a sweep has reclaimed the commits themselves.
pub async fn list_rollup(
    State(state): State<AppState>,
    Query(q): Query<RollupQuery>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    if refresh_requested(q.refresh.as_deref()) {
        refresh_history(&engine);
    }

    let rows = engine
        .repo
        .history_rollup()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let scanned = rows.len();

    let needle =
        q.q.as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
    let (limit, offset) = page(q.limit, q.offset);

    let base: Vec<_> = rows
        .into_iter()
        .filter(|r| in_day_range(&r.day, q.from.as_deref(), q.to.as_deref()))
        .filter(|r| {
            needle.as_deref().is_none_or(|n| {
                contains_ci(&r.day, n)
                    || contains_ci(&r.agent_id, n)
                    || contains_ci(&r.intent_category, n)
                    || contains_ci(&r.namespace, n)
            })
        })
        .collect();

    let mut namespaces = BTreeMap::new();
    let mut agents = BTreeMap::new();
    let mut intents = BTreeMap::new();
    for r in &base {
        let n = r.commit_count.max(0) as usize;
        bump(&mut namespaces, &r.namespace, n);
        bump(&mut agents, &r.agent_id, n);
        bump(&mut intents, &r.intent_category, n);
    }

    let mut matched: Vec<_> = base
        .into_iter()
        .filter(|r| {
            q.namespace
                .as_deref()
                .is_none_or(|n| eq_ci(&r.namespace, n))
        })
        .filter(|r| q.agent.as_deref().is_none_or(|a| eq_ci(&r.agent_id, a)))
        .filter(|r| {
            q.intent
                .as_deref()
                .is_none_or(|i| eq_ci(&r.intent_category, i))
        })
        .collect();
    matched.sort_by(|a, b| {
        b.day
            .cmp(&a.day)
            .then_with(|| b.commit_count.cmp(&a.commit_count))
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });

    // Totals describe the whole filtered set, not just the visible page —
    // a page footer that only summed 100 of 4,000 rows would be a lie.
    let total = matched.len();
    let commits: i64 = matched.iter().map(|r| r.commit_count).sum();
    let days: HashSet<&str> = matched.iter().map(|r| r.day.as_str()).collect();
    let day_span = {
        let mut d: Vec<&str> = days.iter().copied().collect();
        d.sort_unstable();
        json!({ "first": d.first(), "last": d.last(), "count": d.len() })
    };

    let items: Vec<Value> = matched
        .iter()
        .skip(offset)
        .take(limit)
        .map(|r| {
            json!({
                "day": r.day,
                "namespace": r.namespace,
                "agent": r.agent_id,
                "intent": r.intent_category,
                "commits": r.commit_count,
                "first_ts": r.first_ts,
                "last_ts": r.last_ts,
            })
        })
        .collect();

    Ok(Json(json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "scanned": scanned,
        "totals": { "commits": commits, "days": day_span },
        "items": items,
        "facets": {
            "namespaces": facet(namespaces),
            "agents": facet(agents),
            "intents": facet(intents),
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /api/v1/commits — the raw chain a sweep would reclaim
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CommitQuery {
    q: Option<String>,
    agent: Option<String>,
    intent: Option<String>,
    from: Option<String>,
    to: Option<String>,
    /// Only commits that are (`1`) / are not (`0`) on the milestone spine.
    milestone: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    /// How far back along the parent chain to walk before filtering.
    scan: Option<usize>,
    refresh: Option<String>,
}

/// GET /api/v1/commits — the raw commit chain, searchable, each row marked
/// with whether the distilled tables preserve it.
///
/// `on_spine` is the join against `asg_history_milestone`: true means a
/// milestone names this commit and pins its `state_root`, so a sweep keeps
/// it reachable. False means the commit's objects are candidates for
/// reclamation once retention lets them go — its contribution to the record
/// survives only as a `+1` in the rollup.
///
/// The walk covers the full parent DAG, not just first parents. This
/// matters: `Repository::log` follows `parents.first()` only, which on this
/// repo reaches 4,268 of 5,896 commits — and the 1,628 it skips are merge
/// second-parents, exactly the population most likely to fall out of
/// reachability and be reclaimed. A page about what a prune would take
/// cannot be the one that hides them.
///
/// The response reports `scanned` against `distilled` (the rollup's own
/// commit total). A `distilled` larger than `scanned` means the store holds
/// commits no longer reachable from the ref head — already-garbage that a
/// sweep would drop. `capped: true` means the walk hit `scan` first, so the
/// counts describe a window rather than the whole store.
pub async fn list_commits(
    State(state): State<AppState>,
    Query(q): Query<CommitQuery>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    // This endpoint reads the distilled tables too — `on_spine` and
    // `distilled` both come from them. Without the refresh, a store whose
    // extractor has never run reports every commit as unpinned, which is
    // exactly backwards from the truth it's meant to show.
    if refresh_requested(q.refresh.as_deref()) {
        refresh_history(&engine);
    }

    let scan = q
        .scan
        .unwrap_or(COMMIT_SCAN_DEFAULT)
        .clamp(1, COMMIT_SCAN_MAX);

    // The full-parent-DAG walk now lives in the engine (ASG v1.2.0,
    // `Repository::log_dag`). This used to be a hand-rolled BFS here because
    // `log` follows first parents only; the engine does the same walk with
    // the same semantics — deduped by id, a pruned parent ending that edge
    // rather than erroring — so the workaround is gone.
    let walk = engine
        .repo
        .log_dag(&ref_name, scan)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let capped = walk.truncated;
    let commits = &walk.commits[..];
    let scanned = commits.len();

    // The rollup's own total. Larger than `scanned` ⇒ commits exist in the
    // store that the ref head no longer reaches.
    let distilled: i64 = engine
        .repo
        .history_rollup()
        .map(|rows| rows.iter().map(|r| r.commit_count).sum())
        .unwrap_or(0);

    // The spine, keyed by full id so the join can't collide on a short hash.
    let spine: HashSet<String> = engine
        .repo
        .history_milestones(DISTILLED_SCAN_CAP)
        .map(|rows| rows.into_iter().map(|m| m.commit_id.to_string()).collect())
        .unwrap_or_default();

    let needle =
        q.q.as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
    let (limit, offset) = page(q.limit, q.offset);
    let want_milestone = q.milestone.as_deref().map(|v| truthy(Some(v)));

    struct Row {
        id: String,
        short: String,
        day: String,
        timestamp: String,
        agent: String,
        intent: String,
        description: String,
        reasoning: Option<String>,
        confidence: Option<f64>,
        parents: usize,
        state_root: String,
        on_spine: bool,
    }

    let rows: Vec<Row> = commits
        .iter()
        .map(|c| {
            let id = c.id.to_string();
            let on_spine = spine.contains(&id);
            Row {
                short: c.id.short(),
                day: c.timestamp.format("%Y-%m-%d").to_string(),
                timestamp: c.timestamp.to_rfc3339(),
                agent: c.agent_id.clone(),
                intent: format!("{:?}", c.intent.category),
                description: c.intent.description.clone(),
                reasoning: c.reasoning.clone(),
                confidence: c.confidence,
                parents: c.parents.len(),
                state_root: c.state_root.short(),
                on_spine,
                id,
            }
        })
        .collect();

    let base: Vec<&Row> = rows
        .iter()
        .filter(|r| in_day_range(&r.day, q.from.as_deref(), q.to.as_deref()))
        .filter(|r| {
            needle.as_deref().is_none_or(|n| {
                contains_ci(&r.description, n)
                    || contains_ci(&r.short, n)
                    || contains_ci(&r.agent, n)
                    || contains_ci(&r.intent, n)
                    || r.reasoning.as_deref().is_some_and(|s| contains_ci(s, n))
            })
        })
        .collect();

    let mut agents = BTreeMap::new();
    let mut intents = BTreeMap::new();
    for r in &base {
        bump(&mut agents, &r.agent, 1);
        bump(&mut intents, &r.intent, 1);
    }

    let matched: Vec<&&Row> = base
        .iter()
        .filter(|r| q.agent.as_deref().is_none_or(|a| eq_ci(&r.agent, a)))
        .filter(|r| q.intent.as_deref().is_none_or(|i| eq_ci(&r.intent, i)))
        .filter(|r| want_milestone.is_none_or(|want| r.on_spine == want))
        .collect();

    let total = matched.len();
    // Counted over the fully-filtered set, so `on_spine` and `total` are the
    // same population — a summary line pairing one against the other is only
    // honest if they were filtered the same way.
    let on_spine_count = matched.iter().filter(|r| r.on_spine).count();
    let items: Vec<Value> = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|r| {
            json!({
                "commit": r.short,
                "commit_id": r.id,
                "timestamp": r.timestamp,
                "day": r.day,
                "agent": r.agent,
                "intent": r.intent,
                "description": r.description,
                "reasoning": r.reasoning,
                "confidence": r.confidence,
                "parents": r.parents,
                "state_root": r.state_root,
                "on_spine": r.on_spine,
            })
        })
        .collect();

    Ok(Json(json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "scanned": scanned,
        "capped": capped,
        "scan": scan,
        "distilled": distilled,
        "on_spine": on_spine_count,
        "items": items,
        "facets": {
            "agents": facet(agents),
            "intents": facet(intents),
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /api/v1/feedback — recorded search verdicts
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FeedbackQuery {
    /// Free text over query, symbol qname, author and note.
    q: Option<String>,
    verdict: Option<String>,
    author: Option<String>,
    /// Substring match on the symbol's qname.
    symbol: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// GET /api/v1/feedback — every recorded (query, symbol) verdict.
///
/// This is the corrective signal `apply_feedback_adjustments` reads when
/// ranking search results, so being able to search it is how you find out
/// *why* a query ranks the way it does. Expired entries (Plan J t-014) are
/// listed with `expired: true` rather than hidden — an expired verdict still
/// explains a past ranking.
pub async fn list_feedback(
    State(state): State<AppState>,
    Query(q): Query<FeedbackQuery>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    let store = AsgFeedbackStore::from_engine(&engine);
    let entries = store
        .list_all(&engine.ref_name)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let scanned = entries.len();

    let needle =
        q.q.as_deref()
            .map(str::to_lowercase)
            .filter(|s| !s.is_empty());
    let (limit, offset) = page(q.limit, q.offset);
    let now = chrono::Utc::now();

    let verdict_of = |e: &agentstatedeveloper_core::FeedbackEntry| format!("{:?}", e.verdict);

    let base: Vec<_> = entries
        .into_iter()
        .filter(|e| {
            needle.as_deref().is_none_or(|n| {
                contains_ci(&e.query, n)
                    || contains_ci(&e.symbol_qname, n)
                    || contains_ci(&e.author, n)
                    || e.note.as_deref().is_some_and(|s| contains_ci(s, n))
            })
        })
        .collect();

    let mut verdicts = BTreeMap::new();
    let mut authors = BTreeMap::new();
    for e in &base {
        bump(&mut verdicts, &verdict_of(e), 1);
        bump(&mut authors, &e.author, 1);
    }

    let mut matched: Vec<_> = base
        .into_iter()
        .filter(|e| {
            q.verdict
                .as_deref()
                .is_none_or(|v| eq_ci(&verdict_of(e), v))
        })
        .filter(|e| q.author.as_deref().is_none_or(|a| eq_ci(&e.author, a)))
        .filter(|e| {
            q.symbol
                .as_deref()
                .is_none_or(|s| contains_ci(&e.symbol_qname, &s.to_lowercase()))
        })
        .collect();
    matched.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let total = matched.len();
    let items: Vec<Value> = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|e| {
            json!({
                "entry_id": e.entry_id,
                "symbol_id": e.symbol_id,
                "symbol_qname": e.symbol_qname,
                "query": e.query,
                "verdict": verdict_of(&e),
                "author": e.author,
                "created_at": e.created_at,
                "note": e.note,
                "file_scope": e.file_scope,
                "expires_at": e.expires_at,
                "expired": e.expires_at.is_some_and(|t| t < now),
                "withdrawn_at": e.withdrawn_at,
                "withdrawn_by": e.withdrawn_by,
                "withdrawn_reason": e.withdrawn_reason,
                "withdrawn": e.is_withdrawn(),
                // The single question a caller actually has: is this still
                // shaping search results? Derived here so the UI does not
                // reimplement the predicate and drift from `flat_verdicts`.
                "inert": e.is_inert(),
            })
        })
        .collect();

    Ok(Json(json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "scanned": scanned,
        "items": items,
        "facets": {
            "verdicts": facet(verdicts),
            "authors": facet(authors),
        },
    })))
}

// -----------------------------------------------------------------------------
// GET /api/v1/index-health — is the record current?
// -----------------------------------------------------------------------------

/// GET /api/v1/index-health — index freshness and the ASG↔FTS consistency
/// check, i.e. whether the metrics on the other endpoints describe the
/// codebase as it stands now.
///
/// Mirrors what `asd status` prints. `consistency` is `null` when the two
/// indexes agree — `compute_index_consistency` deliberately returns nothing
/// to say "fine", so absence is the healthy case.
pub async fn index_health(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    let ref_name = engine.ref_name.clone();

    let asg_symbols = {
        let prefix = format!(
            "{}/index/by-qname",
            agentstatedeveloper_core::ASD_PATH_PREFIX
        );
        match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(Value::Object(map)) => map.len(),
            _ => 0,
        }
    };

    let fts = SearchFtsDb::open(&state.db_path).ok();
    let fts_symbols = fts.as_ref().map(|f| f.symbol_count()).unwrap_or(0);
    let fts_rows = fts.as_ref().map(|f| f.fts_symbol_row_count()).unwrap_or(0);
    let feedback_entries = fts.as_ref().map(|f| f.feedback_count()).unwrap_or(0);
    let annotated = fts
        .as_ref()
        .map(|f| f.annotated_symbol_count(&ref_name))
        .unwrap_or(0);
    let indexed_at = fts.as_ref().and_then(|f| f.last_indexed_at());

    let stale = stale_warning_classified(&state.db_path, 3600);
    let db_bytes = std::fs::metadata(&state.db_path).map(|m| m.len()).ok();

    Ok(Json(json!({
        "db_path": state.db_path.display().to_string(),
        "db_bytes": db_bytes,
        "ref_name": ref_name,
        "indexed_at": indexed_at,
        "indexed_age": indexed_at.map(|t| {
            let age = chrono::Utc::now().timestamp().saturating_sub(t).max(0) as u64;
            // `format_age` takes the timestamp, not the elapsed seconds.
            json!({ "secs": age, "human": format_age(t) })
        }),
        "symbols": {
            "asg": asg_symbols,
            "fts": fts_symbols,
            "fts_rows": fts_rows,
            "annotated": annotated,
        },
        "feedback_entries": feedback_entries,
        "consistency": compute_index_consistency(asg_symbols, fts_symbols),
        "stale": stale.map(|w| json!({
            "message": w.message,
            "severity": w.severity,
            "age_secs": w.age_secs,
        })),
    })))
}

// -----------------------------------------------------------------------------
// GET /api/v1/scorecard — the five-dimension benchmark
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ScorecardQuery {
    /// Named scope alias from `.asd/scopes.toml`.
    scope: Option<String>,
    /// Comma-separated glob patterns restricting which files are scored.
    paths: Option<String>,
    /// Per-symbol gap listing for one dimension: truth / change / workflow /
    /// uncertainty.
    drill_down: Option<String>,
    /// Cap on the drill-down rows returned.
    limit: Option<usize>,
}

/// GET /api/v1/scorecard — truth / feedback / change / uncertainty /
/// workflow, each 0-100, plus the data-quality and token-economy blocks.
///
/// The arithmetic lives in `core::scorecard`, shared with `asd scorecard`
/// and the `scorecard` MCP tool. What differs here is the envelope: no
/// snapshot-history side effect, because an HTTP read must not write the
/// trend file the next CLI run compares against — which is why `--trend`
/// stays CLI-only.
pub async fn scorecard(
    State(state): State<AppState>,
    Query(q): Query<ScorecardQuery>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    let card = core_scorecard::compute(
        &engine,
        &state.db_path,
        &core_scorecard::ScorecardOptions {
            scope: q.scope.as_deref(),
            paths: q.paths.as_deref(),
            drill_down: q.drill_down.as_deref(),
            drill_limit: q.limit.unwrap_or(50).clamp(1, 2_000),
        },
    );

    let mut out = card.to_json();
    let obj = out.as_object_mut().expect("to_json built an object");
    obj.insert("timestamp".into(), json!(chrono::Utc::now().to_rfc3339()));
    if card.matched_nothing {
        // Phrased for an API consumer: the CLI's "try broadening
        // --scope/--paths" names flags that do not exist over HTTP.
        obj.insert(
            "note".into(),
            json!(if card.scoped {
                "no symbols matched the paths filter"
            } else {
                "no symbols indexed — run `asd index` first"
            }),
        );
    }
    Ok(Json(out))
}

// -----------------------------------------------------------------------------
// Feedback lifecycle actions (plan feedback-lifecycle t-006)
// -----------------------------------------------------------------------------

/// Body for the two lifecycle POSTs. Both fields optional so a caller can
/// `POST` an empty object and get sensible defaults.
#[derive(Deserialize, Default)]
pub struct FeedbackActionBody {
    /// Who performed it. Recorded on the entry for withdrawal.
    #[serde(default)]
    pub by: Option<String>,
    /// Why. Stored with a withdrawal; ignored for expiry, which has no
    /// judgement attached to it.
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /api/v1/feedback/{entry_id}/withdraw — retract a wrong verdict.
///
/// Returns the updated entry, matching the approvals endpoints' shape, so the
/// UI can patch one row rather than refetching the list.
///
/// Deliberately NOT paired with a purge endpoint. Purge hard-deletes from an
/// otherwise append-only store and exists for data that must not persist
/// (a secret pasted into a note); that belongs behind a CLI `--yes`, not one
/// click in a browser. Withdrawal is the reversible-in-spirit action and the
/// one worth having where you notice the problem.
pub async fn withdraw_feedback(
    State(state): State<AppState>,
    axum::extract::Path(entry_id): axum::extract::Path<String>,
    body: Option<Json<FeedbackActionBody>>,
) -> Result<Json<Value>, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let engine = state.engine.lock().await;
    let store = AsgFeedbackStore::from_engine(&engine);
    let by = body.by.as_deref().unwrap_or("asd-lens");

    match store.withdraw(&engine.ref_name, &entry_id, by, body.reason.as_deref()) {
        Ok(Some(e)) => Ok(Json(feedback_json(&e))),
        Ok(None) => Err(ApiError::NotFound(format!("no feedback entry {entry_id}"))),
        Err(e) => Err(ApiError::Internal(e.to_string())),
    }
}

/// POST /api/v1/feedback/{entry_id}/expire — lapse a verdict that was right
/// but is no longer relevant.
///
/// Sets `expires_at` to now via the same re-record path the CLI uses, so both
/// the store and the SQLite cache update together.
pub async fn expire_feedback(
    State(state): State<AppState>,
    axum::extract::Path(entry_id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    let store = AsgFeedbackStore::from_engine(&engine);

    let Some(existing) = store
        .list_all(&engine.ref_name)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .into_iter()
        .find(|e| e.entry_id == entry_id)
    else {
        return Err(ApiError::NotFound(format!("no feedback entry {entry_id}")));
    };
    // Idempotent, same as the CLI: re-expiring must not push the timestamp
    // forward.
    if existing.is_expired() {
        return Ok(Json(feedback_json(&existing)));
    }
    let mut lapsed = existing;
    lapsed.expires_at = Some(chrono::Utc::now());
    store
        .record(&engine.ref_name, &lapsed, &lapsed.author.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(feedback_json(&lapsed)))
}

/// One entry in the shape the Feedback tab consumes. Shared by the list and
/// both actions so a patched row cannot drift from a listed one.
fn feedback_json(e: &agentstatedeveloper_core::FeedbackEntry) -> Value {
    json!({
        "entry_id": e.entry_id,
        "symbol_id": e.symbol_id,
        "symbol_qname": e.symbol_qname,
        "query": e.query,
        "verdict": format!("{:?}", e.verdict),
        "author": e.author,
        "created_at": e.created_at,
        "note": e.note,
        "file_scope": e.file_scope,
        "expires_at": e.expires_at,
        "expired": e.is_expired(),
        "withdrawn_at": e.withdrawn_at,
        "withdrawn_by": e.withdrawn_by,
        "withdrawn_reason": e.withdrawn_reason,
        "withdrawn": e.is_withdrawn(),
        "inert": e.is_inert(),
    })
}
