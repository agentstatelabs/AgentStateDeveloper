//! Live activity stream for asd-serve — backs `GET /api/v1/events` (SSE).
//!
//! asd-serve is a READ-ONLY server over the shared SQLite/ASG db; writes
//! come from OTHER processes (CLI, MCP server, git hooks). There is no
//! in-process write path to hook, so change detection is polling-based.
//!
//! ## Change-signal choice
//!
//! Three candidate signals were considered:
//!
//! 1. **db file mtime** — cheap, but second-granularity, noisy under WAL
//!    checkpoints, and nonexistent for in-memory engines (every test).
//! 2. **max(created_at) over ledger entries** — requires walking the whole
//!    `/asd/v1/ledger` subtree every tick, and misses non-ledger writes
//!    (effects, index runs).
//! 3. **the ASG ref head** (`repo.head(ref_name)`) — every ASD write of
//!    any kind (ledger/thinking, effect decls, index commits) is an ASG
//!    commit that moves the ref, and `SqliteStorage::get_ref` is a
//!    single-row indexed query with no in-process caching, so commits
//!    landed by other processes are visible on the next poll.
//!
//! We use (3): it is the cheapest signal that is also *complete* (fires
//! exactly when a commit lands, for every write kind) and behaves
//! identically for file-backed and in-memory engines. The audit log is a
//! separate JSONL file (not in the db), so it gets its own cheap gate:
//! file size via `fs::metadata`, with the parsed event count as cursor.
//!
//! ## Architecture
//!
//! One poller task per server process feeds a `tokio::sync::broadcast`
//! channel; each SSE subscriber holds a `Receiver`. The poller is spawned
//! **lazily on the first subscriber** (not at router build) so that
//! deployments that never open the events stream — and the dozens of
//! `build_router` calls in unit tests — pay zero background cost. Once
//! spawned, the loop wakes every [`POLL_INTERVAL`] but skips ALL db/fs
//! work while `receiver_count() == 0`, and re-snapshots its baseline when
//! a subscriber reconnects — stream semantics are "changes while you are
//! connected", not a replay of history missed while nobody watched.
//!
//! The task holds only a `Weak` reference to the engine, so dropping the
//! router (test teardown, shutdown) lets the poller exit on its next tick.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, EffectStore, Engine, LedgerEntry, paths, read_jsonl,
};
use agentstategraph_core::{Commit, ObjectId};
use serde_json::json;
use tokio::sync::{Mutex, OnceCell, broadcast};

/// How often the poller checks for changes. Also the worst-case latency
/// between a write landing in the db and the SSE event being emitted.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How many commits `repo.log` walks back per tick when the head moved.
/// A burst larger than this (e.g. a full reindex between two polls) is
/// coalesced into a single `resync` event instead of a flood.
const LOG_WINDOW: usize = 512;

/// Broadcast buffer per subscriber. Slow consumers that fall more than
/// this many events behind see a `Lagged` gap (skipped, not fatal).
const CHANNEL_CAPACITY: usize = 256;

/// Verification writes reuse the same `put_effects` path (and commit
/// description) as declarations. A decl whose `verification.at` is within
/// this window of the commit timestamp is classified `effect_verified`.
const VERIFY_WINDOW_SECS: i64 = 10;

/// Shared hub: lazily spawns the poller and hands out broadcast receivers.
pub struct EventHub {
    tx: broadcast::Sender<String>,
    started: OnceCell<()>,
    engine: Weak<Mutex<Engine>>,
    audit_log_path: Option<PathBuf>,
}

impl EventHub {
    /// Build a hub bound (weakly) to the engine. Cheap — no task is
    /// spawned until the first [`subscribe`](Self::subscribe).
    pub fn new(engine: &Arc<Mutex<Engine>>, audit_log_path: Option<PathBuf>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(Self {
            tx,
            started: OnceCell::new(),
            engine: Arc::downgrade(engine),
            audit_log_path,
        })
    }

    /// Register a subscriber. On the first call, snapshots the baseline
    /// (current ref head + audit cursor) BEFORE spawning the poller, so an
    /// event written immediately after this returns can never race past
    /// the baseline and be missed.
    pub async fn subscribe(&self) -> broadcast::Receiver<String> {
        self.started
            .get_or_init(|| async {
                let baseline =
                    snapshot_baseline(&self.engine, self.audit_log_path.as_deref()).await;
                tokio::spawn(poll_loop(
                    self.engine.clone(),
                    self.tx.clone(),
                    self.audit_log_path.clone(),
                    baseline,
                ));
            })
            .await;
        self.tx.subscribe()
    }
}

/// Poller cursor: last seen ref head + audit-log position.
struct Baseline {
    head: Option<ObjectId>,
    audit_size: u64,
    audit_count: usize,
}

async fn snapshot_baseline(engine: &Weak<Mutex<Engine>>, audit: Option<&Path>) -> Baseline {
    let head = match engine.upgrade() {
        Some(arc) => {
            let g = arc.lock().await;
            g.repo.head(&g.ref_name).ok()
        }
        None => None,
    };
    let (audit_size, audit_count) = audit.map(audit_cursor).unwrap_or((0, 0));
    Baseline {
        head,
        audit_size,
        audit_count,
    }
}

/// (file size, parsed event count) for the audit JSONL. Missing file → (0, 0).
fn audit_cursor(path: &Path) -> (u64, usize) {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let count = read_jsonl(path).map(|v| v.len()).unwrap_or(0);
    (size, count)
}

async fn poll_loop(
    engine: Weak<Mutex<Engine>>,
    tx: broadcast::Sender<String>,
    audit_path: Option<PathBuf>,
    mut baseline: Baseline,
) {
    // The spawn was triggered by a live subscriber, so start "hot".
    let mut had_subscribers = true;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        // Engine gone → server shut down (or test router dropped): exit.
        let Some(engine_arc) = engine.upgrade() else {
            return;
        };
        if tx.receiver_count() == 0 {
            // Idle: one timer wakeup per tick, zero db/fs access.
            had_subscribers = false;
            continue;
        }
        if !had_subscribers {
            // 0 → >0 transition: re-snapshot instead of replaying the
            // backlog accumulated while nobody was connected.
            baseline = snapshot_baseline(&engine, audit_path.as_deref()).await;
            had_subscribers = true;
            continue;
        }

        let mut events: Vec<serde_json::Value> = Vec::new();
        {
            let g = engine_arc.lock().await;
            if let Ok(head) = g.repo.head(&g.ref_name) {
                if baseline.head.as_ref() != Some(&head) {
                    events.extend(commit_events(&g, baseline.head.as_ref()));
                    baseline.head = Some(head);
                }
            }
        }

        if let Some(path) = audit_path.as_deref() {
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if size != baseline.audit_size {
                if let Ok(all) = read_jsonl(path) {
                    for e in all.iter().skip(baseline.audit_count) {
                        events.push(json!({
                            "at": e.timestamp,
                            "kind": "audit",
                            "qname": serde_json::Value::Null,
                            "symbol_id": serde_json::Value::Null,
                            "entry_id": e.subject_id,
                            "summary": match &e.reason {
                                Some(r) => format!("{} → {} ({})", e.event_type, e.outcome, r),
                                None => format!("{} → {}", e.event_type, e.outcome),
                            },
                        }));
                    }
                    baseline.audit_count = all.len();
                }
                baseline.audit_size = size;
            }
        }

        for e in events {
            if let Ok(s) = serde_json::to_string(&e) {
                // Send fails only when every receiver is gone — harmless.
                let _ = tx.send(s);
            }
        }
    }
}

/// Diff the commit chain since `last_head` and classify each new commit
/// into a timeline-shaped event. Emitted oldest-first.
///
/// Field names ({at, kind, qname, symbol_id, entry_id, summary}) match
/// `/api/v1/timeline` so the Lens "now" feed can merge the two sources.
/// `kind` is a `LedgerKind` snake_case name for ledger/thinking entries,
/// or one of the stream-only kinds: `effect_declared`, `effect_verified`,
/// `index_run`, `audit`, `commit`, `resync`.
fn commit_events(engine: &Engine, last_head: Option<&ObjectId>) -> Vec<serde_json::Value> {
    let log = match engine.repo.log(&engine.ref_name, LOG_WINDOW) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    // `log` is newest-first. Take commits until we hit the old head.
    let cut = last_head.and_then(|h| log.iter().position(|c| &c.id == h));
    let new_commits: Vec<&Commit> = match cut {
        Some(i) => log[..i].iter().collect(),
        None => {
            // Old head not in the window: either > LOG_WINDOW commits
            // landed since the last tick (full reindex) or history was
            // rewritten. Coalesce into one honest event and resync.
            return vec![json!({
                "at": chrono::Utc::now(),
                "kind": "resync",
                "qname": serde_json::Value::Null,
                "symbol_id": serde_json::Value::Null,
                "entry_id": serde_json::Value::Null,
                "summary": format!(
                    "more than {} commits since last poll — event log truncated, state resynced",
                    LOG_WINDOW
                ),
            })];
        }
    };
    if new_commits.is_empty() {
        return Vec::new();
    }

    // qname resolution map, built at most once per changed batch.
    let mut id_map: Option<std::collections::HashMap<String, agentstatedeveloper_core::Symbol>> =
        None;
    let mut qname_of = |engine: &Engine, symbol_id: &str| -> serde_json::Value {
        let map =
            id_map.get_or_insert_with(|| AsgIndexStore::from_engine(engine).build_id_map(engine));
        map.get(symbol_id)
            .map(|s| serde_json::Value::String(s.qname.clone()))
            .unwrap_or(serde_json::Value::Null)
    };

    let mut events: Vec<serde_json::Value> = Vec::new();
    // Index-family commits (symbol/qname/edges/hydrate) are bulk noise —
    // coalesce them into a single index_run event per batch.
    let mut index_commits: usize = 0;
    let mut index_last: Option<(chrono::DateTime<chrono::Utc>, String)> = None;

    for commit in new_commits.iter().rev() {
        let desc = commit.intent.description.as_str();
        if let Some(entry_id) = desc.strip_prefix("ledger-idx ") {
            // The reverse-index commit carries the entry_id; resolve it to
            // the full entry for a high-fidelity event. (The sibling
            // "ledger <kind> for <sym>" commit is skipped below.)
            if let Some(entry) = load_ledger_entry(engine, entry_id) {
                let qname = qname_of(engine, &entry.symbol_id);
                events.push(json!({
                    "at": entry.created_at,
                    "kind": serde_json::to_value(entry.kind).unwrap_or(serde_json::Value::Null),
                    "qname": qname,
                    "symbol_id": entry.symbol_id,
                    "entry_id": entry.entry_id,
                    "summary": entry.summary,
                }));
            }
        } else if desc.starts_with("ledger ") {
            // Covered by the paired ledger-idx commit above.
        } else if let Some(symbol_id) = desc.strip_prefix("declare effects for ") {
            let qname = qname_of(engine, symbol_id);
            let decl = AsgEffectStore::from_engine(engine)
                .get_effects(&engine.ref_name, symbol_id)
                .ok()
                .flatten();
            // Verification reuses put_effects (same commit description);
            // a verification stamp written in the same breath as this
            // commit means "verified", otherwise it's a (re)declaration.
            let verified = decl
                .as_ref()
                .and_then(|d| d.verification.as_ref())
                .filter(|v| (commit.timestamp - v.at).num_seconds().abs() <= VERIFY_WINDOW_SECS);
            let (kind, summary) = match (verified, &decl) {
                (Some(v), _) => (
                    "effect_verified",
                    format!(
                        "effects verified: {}",
                        serde_json::to_value(v.status)
                            .ok()
                            .and_then(|x| x.as_str().map(String::from))
                            .unwrap_or_else(|| "unknown".into())
                    ),
                ),
                (None, Some(d)) => {
                    let cats: Vec<&str> = d.declared.iter().map(|e| e.effect.as_str()).collect();
                    (
                        "effect_declared",
                        if cats.is_empty() {
                            "declares no effects".to_string()
                        } else {
                            format!("declares {}", cats.join(", "))
                        },
                    )
                }
                (None, None) => ("effect_declared", desc.to_string()),
            };
            events.push(json!({
                "at": commit.timestamp,
                "kind": kind,
                "qname": qname,
                "symbol_id": symbol_id,
                "entry_id": serde_json::Value::Null,
                "summary": summary,
            }));
        } else if desc.starts_with("index symbol ")
            || desc.starts_with("qname index ")
            || desc.starts_with("asd index: ")
            || desc.starts_with("asd hydrate: ")
        {
            index_commits += 1;
            // Prefer the pipeline's own summary commits ("asd index: N call
            // edges") over per-symbol ones when labelling the run.
            let informative = desc.starts_with("asd ");
            if informative || index_last.is_none() {
                index_last = Some((commit.timestamp, desc.to_string()));
            }
        } else {
            // Anything else (scratch writes, rebind repairs, approvals from
            // the pro crate, …): surface generically rather than dropping.
            events.push(json!({
                "at": commit.timestamp,
                "kind": "commit",
                "qname": serde_json::Value::Null,
                "symbol_id": serde_json::Value::Null,
                "entry_id": serde_json::Value::Null,
                "summary": desc,
            }));
        }
    }

    if let Some((at, last_desc)) = index_last {
        events.push(json!({
            "at": at,
            "kind": "index_run",
            "qname": serde_json::Value::Null,
            "symbol_id": serde_json::Value::Null,
            "entry_id": serde_json::Value::Null,
            "summary": format!("{} ({} commits)", last_desc, index_commits),
        }));
    }

    events
}

/// entry_id → LedgerEntry via the `/asd/v1/ledger-idx/` reverse index.
fn load_ledger_entry(engine: &Engine, entry_id: &str) -> Option<LedgerEntry> {
    let idx = engine
        .repo
        .get_json(&engine.ref_name, &paths::ledger_entry_index_path(entry_id))
        .ok()?;
    let symbol_id = idx.as_str()?.to_string();
    let val = engine
        .repo
        .get_json(
            &engine.ref_name,
            &paths::ledger_entry_path(&symbol_id, entry_id),
        )
        .ok()?;
    serde_json::from_value(val).ok()
}
