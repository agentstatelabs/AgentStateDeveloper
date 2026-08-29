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
    AsgEffectStore, AsgFeedbackStore, EffectStore, Engine, FeedbackStore, SearchFtsDb,
    compute_index_consistency, estimate_tokens, format_age, glob_match, resolve_scope,
    schema::{LedgerEntry, LedgerKind, Symbol, VerificationStatus},
    stale_warning_classified,
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

    // Breadth-first over every parent edge, deduped by id (a merge is
    // reachable from both sides). `capped` is set only when the frontier was
    // still non-empty at the cap.
    let head = engine
        .repo
        .head(&ref_name)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut seen: HashSet<agentstategraph_core::ObjectId> = HashSet::new();
    let mut frontier = std::collections::VecDeque::from([head]);
    let mut walked = Vec::with_capacity(scan.min(4_096));
    let mut capped = false;
    while let Some(id) = frontier.pop_front() {
        if walked.len() >= scan {
            capped = true;
            break;
        }
        if !seen.insert(id) {
            continue;
        }
        match engine.repo.get_commit(&id) {
            Ok(Some(c)) => {
                for p in &c.parents {
                    if !seen.contains(p) {
                        frontier.push_back(*p);
                    }
                }
                walked.push(c);
            }
            // A missing parent is a pruned commit, not an error — stop
            // descending that edge and keep walking the rest.
            _ => continue,
        }
    }
    let commits = &walked[..];
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
/// The arithmetic mirrors `asd scorecard --json`, minus its snapshot-history
/// side effect: an HTTP read should not write a trend file that the next CLI
/// run then compares against. `--trend` stays CLI-only for that reason.
///
/// NOTE: this is the third implementation of these scores — the others are
/// `agentstatedeveloper-cli/src/commands/scorecard.rs` and the `scorecard`
/// MCP tool in `mcp_server.rs`. Keep them in step until they are unified.
pub async fn scorecard(
    State(state): State<AppState>,
    Query(q): Query<ScorecardQuery>,
) -> Result<Json<Value>, ApiError> {
    let engine = state.engine.lock().await;
    Ok(Json(compute_scorecard(
        &engine,
        &state.db_path,
        q.scope.as_deref(),
        q.paths.as_deref(),
        q.drill_down.as_deref(),
        q.limit.unwrap_or(50).clamp(1, 2_000),
    )))
}

fn compute_scorecard(
    engine: &Engine,
    db_path: &std::path::Path,
    scope: Option<&str>,
    paths: Option<&str>,
    drill_down: Option<&str>,
    drill_limit: usize,
) -> Value {
    let ref_name = engine.ref_name.clone();
    let effect_store = AsgEffectStore::from_engine(engine);
    let feedback_store = AsgFeedbackStore::from_engine(engine);

    let mut paths_filter: Vec<String> = Vec::new();
    if let Some(s) = scope {
        paths_filter.extend(resolve_scope(s, db_path));
    }
    if let Some(p) = paths {
        paths_filter.extend(
            p.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty()),
        );
    }
    let scoped = !paths_filter.is_empty();

    let all_syms: Vec<Symbol> = {
        let tree = engine
            .repo
            .get_tree(&ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(Value::Object(Default::default()));
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
            .filter(|s| paths_filter.iter().any(|p| glob_match(p, &s.file)))
            .collect()
    } else {
        all_syms.iter().collect()
    };
    let total_symbols = scored_syms.len();

    if total_symbols == 0 {
        let note = if scoped {
            "no symbols matched the path filter — try broadening scope/paths"
        } else {
            "no symbols indexed — run `asd index` first"
        };
        return json!({
            "note": note,
            "capability_scores": { "truth": 0, "feedback": 0, "change": 0, "uncertainty": 0, "workflow": 0, "overall": 0 },
            "scores": { "truth": 0, "feedback": 0, "change": 0, "uncertainty": 0, "workflow": 0, "overall": 0 },
        });
    }

    // One tree read for the whole ledger instead of N per-symbol reads.
    let ledger_by_sym: std::collections::HashMap<String, Vec<LedgerEntry>> = {
        let tree = engine
            .repo
            .get_tree(&ref_name, "/asd/v1/ledger")
            .unwrap_or(Value::Object(Default::default()));
        let mut map: std::collections::HashMap<String, Vec<LedgerEntry>> = Default::default();
        if let Value::Object(by_symbol) = tree {
            for (sym_id, per_symbol) in by_symbol {
                if let Value::Object(entries_map) = per_symbol {
                    let mut entries: Vec<LedgerEntry> = entries_map
                        .values()
                        .filter_map(|v| serde_json::from_value::<LedgerEntry>(v.clone()).ok())
                        .collect();
                    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                    let superseded: HashSet<String> = entries
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

    let drill = drill_down.unwrap_or("").to_lowercase();
    let need_drill = !drill.is_empty();
    let mut drill_rows: Vec<Value> = Vec::new();

    let mut verified_count = 0usize;
    let mut owned_count = 0usize;
    let mut has_invariant = 0usize;
    let mut has_validation = 0usize;
    let mut total_ledger_entries = 0usize;
    let mut ctx_tagged_entries = 0usize;

    // Token economy: ASD's structured per-symbol cost against the cost of
    // reading the source files those symbols live in.
    let mut structured_tokens = 0usize;
    let mut file_max_line: std::collections::HashMap<&str, u32> = Default::default();

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

        let record = format!(
            "{} {} {}",
            sym.qname,
            sym.signature.as_deref().unwrap_or(""),
            sym.doc
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        );
        structured_tokens += estimate_tokens(&record);
        let f = file_max_line.entry(sym.file.as_str()).or_insert(0);
        *f = (*f).max(sym.end.line);

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
            has_invariant += 1;
        }
        if sym_vs {
            has_validation += 1;
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
                drill_rows.push(json!({
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

    let total = total_symbols as f64;
    let truth =
        ((verified_count as f64 / total + owned_count as f64 / total) / 2.0 * 100.0).min(100.0);
    let feedback_score = (feedback_count as f64 / 50.0 * 100.0).min(100.0);
    let change =
        ((has_invariant as f64 / total + has_validation as f64 / total) / 2.0 * 100.0).min(100.0);
    let uncertainty = {
        let effect_rate = verified_count as f64 / total;
        let volume_score = (total / 500.0).min(1.0);
        ((effect_rate + volume_score) / 2.0 * 100.0).min(100.0)
    };
    let workflow = {
        let density = (total_ledger_entries as f64 / total / 2.0).min(1.0);
        let ctx_adoption = if total_ledger_entries == 0 {
            0.0
        } else {
            (ctx_tagged_entries as f64 / total_ledger_entries as f64).min(1.0)
        };
        ((density * 0.6 + ctx_adoption * 0.4) * 100.0).min(100.0)
    };
    let overall = (truth + feedback_score + change + uncertainty + workflow) / 5.0;
    let scores = json!({
        "truth": truth.round() as u64,
        "feedback": feedback_score.round() as u64,
        "change": change.round() as u64,
        "uncertainty": uncertainty.round() as u64,
        "workflow": workflow.round() as u64,
        "overall": overall.round() as u64,
    });

    let ledger_density = total_ledger_entries as f64 / total;
    let sparse_db = ledger_density < 0.5;
    let with_ledger = scored_syms
        .iter()
        .filter(|s| ledger_by_sym.contains_key(&s.symbol_id))
        .count();

    const TOKENS_PER_LINE: usize = 9;
    let source_read_tokens: usize = file_max_line
        .values()
        .map(|&l| l as usize * TOKENS_PER_LINE)
        .sum();
    let reduction_pct = if source_read_tokens > 0 {
        (1.0 - structured_tokens as f64 / source_read_tokens as f64) * 100.0
    } else {
        0.0
    };
    let ratio_x = if structured_tokens > 0 {
        source_read_tokens as f64 / structured_tokens as f64
    } else {
        0.0
    };

    let mut out = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "capability_scores": scores,
        "scores": scores,
        "data_quality": {
            "ledger_density": ledger_density,
            "symbols_scored": total_symbols,
            "symbols_with_any_ledger": with_ledger,
            "coverage_pct": (with_ledger as f64 / total * 100.0).round(),
            "sparse_db": sparse_db,
            "note": if sparse_db {
                format!(
                    "sparse ledger ({total_ledger_entries} entries across {total_symbols} symbols, \
                     {ledger_density:.2} avg) — run 'asd sync' + 'asd hydrate' to populate; \
                     scores reflect data density, not workflow quality"
                )
            } else {
                "ledger density is adequate".to_string()
            },
            "scope": if scoped { json!(paths_filter) } else { Value::Null },
        },
        "details": {
            "total_symbols": total_symbols,
            "verified_effects": verified_count,
            "owned_symbols": owned_count,
            "invariant_symbols": has_invariant,
            "validation_symbols": has_validation,
            "feedback_entries": feedback_count,
            "total_ledger_entries": total_ledger_entries,
            "ctx_tagged_ledger_entries": ctx_tagged_entries,
        },
        "token_economy": {
            "note": "Internal estimate — NOT a published benchmark and NOT measured per query. \
                     Compares ASD's structured per-symbol index cost (qname + signature + first doc \
                     line) against reading the source files those symbols live in (file length \
                     estimated from symbol line spans).",
            "structured_tokens": structured_tokens,
            "source_read_tokens_est": source_read_tokens,
            "reduction_pct": (reduction_pct * 10.0).round() / 10.0,
            "ratio_x": (ratio_x * 10.0).round() / 10.0,
        },
    });

    if need_drill {
        let total_gaps = drill_rows.len();
        let shown: Vec<_> = drill_rows.into_iter().take(drill_limit).collect();
        let omitted = total_gaps.saturating_sub(shown.len());
        out.as_object_mut().unwrap().insert(
            "drill_down".into(),
            json!({
                "dimension": drill,
                "total_gaps": total_gaps,
                "shown": shown.len(),
                "omitted": omitted,
                "gap_symbols": shown,
            }),
        );
    }

    out
}
