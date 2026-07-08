use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agentstategraph::Repository;
use agentstategraph_storage::SqliteStorage;

use crate::audit::{AuditEvent, AuditSink, NullSink, emit_audit, event_types};
use crate::error::{AsdError, Result};
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore, RatifyOps};
use crate::policy::{
    Decision, PermissivePolicyGate, PolicyGate, PolicyStoreGate, Situation, actions,
};
use crate::schema::{LedgerEntry, Symbol};
use crate::search_fts::SearchFtsDb;
use crate::sidecar::hydrate_from_dir;
use serde_json::json;

/// The top-level ASD engine. Owns an ASG repository, a policy gate, an
/// audit sink, and optional commercial ratify operations. Cheap to
/// construct; shared across CLI, MCP, and library consumers.
///
/// # Field access
///
/// The fields are `pub` for compatibility with CLI/MCP surface code that
/// accesses `repo`, `ref_name`, `policy`, and `audit` directly. New callers
/// should prefer the operation methods (`append_ledger_entry`, etc.) which
/// automatically enforce the policy gate and emit audit events.
pub struct Engine {
    pub repo: Repository,
    pub policy: Arc<dyn PolicyGate>,
    pub audit: Arc<dyn AuditSink>,
    pub ref_name: String,
    /// Commercial ratify operations (Team tier). `None` in the OSS binary —
    /// ledger approve/reject/withdraw return a commercial-feature error.
    /// Set by `asd-pro` at startup via [`Engine::set_ratify_ops`].
    pub ratify: Option<Arc<dyn RatifyOps>>,
    /// Path to the SQLite backing store, when opened via `open_sqlite`.
    /// `None` for in-memory engines (tests, etc.).  Commands that bypass
    /// the git layer for SQLite-cached reads (symbol map, edges) use this.
    pub db_path: Option<std::path::PathBuf>,
    /// Single open connection to the ASD SQLite file.  Opened once in
    /// `open_sqlite` and shared (by borrow) across all stores for the
    /// lifetime of the command — eliminates per-call `Connection::open`.
    pub fts: Option<SearchFtsDb>,
}

impl Engine {
    /// Open (or create) an ASD engine backed by a SQLite file.
    ///
    /// If the repository has no symbols and a `.asd/v1/` sidecar exists next
    /// to the DB file, hydrates automatically — this covers the fresh-clone
    /// case without requiring a manual `asd hydrate` invocation.
    pub fn open_sqlite(db_path: &Path) -> Result<Self> {
        let storage = SqliteStorage::open(db_path).map_err(|e| AsdError::Other(e.to_string()))?;
        let repo = Repository::new(Box::new(storage));
        repo.init()?;

        // Open FTS connection once — reused by all stores for this engine's lifetime.
        let fts = SearchFtsDb::open(db_path).ok();

        let engine = Self {
            repo,
            policy: Arc::new(PermissivePolicyGate),
            audit: Arc::new(NullSink),
            ref_name: "main".to_string(),
            ratify: None,
            db_path: Some(db_path.to_path_buf()),
            fts,
        };

        // Auto-hydrate from sidecar when the DB is empty (fresh clone or
        // deleted DB). We check for the by-qname tree as a proxy for "has
        // been indexed before". Errors are silently ignored so a missing or
        // malformed sidecar never blocks normal operation.
        let project_root = db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let sidecar_root = project_root.join(".asd/v1");
        // Fast emptiness check: reuse the already-open FTS connection — avoids
        // a second `Connection::open` that the 0.9.72 code used.
        let sqlite_has_symbols = engine
            .fts
            .as_ref()
            .map(|fts| fts.symbols_cached_for(&engine.ref_name))
            .unwrap_or(false);
        let is_empty = {
            if sqlite_has_symbols {
                false // SQLite says there are symbols — definitely not empty.
            } else {
                // Cache absent (old version DB, fresh install, or blank slate).
                // Fall back to git tree walk.
                engine
                    .repo
                    .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
                    .ok()
                    .and_then(|v| v.as_object().map(|m| m.is_empty()))
                    .unwrap_or(true)
            }
        };
        if is_empty && sidecar_root.exists() {
            let _ = hydrate_from_dir(
                &engine.repo,
                &engine.ref_name,
                &project_root,
                "asd-auto-hydrate",
            );
        }

        // Plan T self-heal: the git trees have symbols but the SQLite symbol
        // cache is empty — a born-cold DB (hydrate never synced caches before
        // 1.2.1, or the auto-hydrate above just ran) or a past sync failure
        // (e.g. SQLITE_BUSY while a server held the DB). Without this, every
        // read on every subsequent run pays the slow git-walk path forever.
        // One authoritative walk (~2s at 10k symbols) repairs it here, which
        // covers CLI, MCP, and asd-serve in one place — they all construct
        // engines through `open_sqlite`. Safe under concurrent readers:
        // `sync_symbols`/`sync_call_edges` are transactional full-replaces
        // (WAL mode, 5s busy timeout) and idempotent, so a concurrent healer
        // just repeats the same work. In-memory engines (`fts: None`) skip
        // inside `warm_caches`; failures are logged and non-fatal — the next
        // open retries.
        if !sqlite_has_symbols {
            match engine.warm_caches() {
                Ok(w) if w.symbols_cached > 0 => {
                    eprintln!(
                        "asd: symbol cache was cold — rebuilt from index ({} symbols, {} edges{})",
                        w.symbols_cached,
                        w.edges_cached,
                        if w.fts_rebuilt { ", FTS rebuilt" } else { "" }
                    );
                }
                Ok(_) => {} // nothing indexed yet, or nothing to warm
                Err(e) => eprintln!("asd: symbol cache self-heal failed (non-fatal): {e}"),
            }
        }

        Ok(engine)
    }

    /// Open an in-memory engine — mostly for tests.
    pub fn open_in_memory() -> Result<Self> {
        let storage = SqliteStorage::in_memory().map_err(|e| AsdError::Other(e.to_string()))?;
        let repo = Repository::new(Box::new(storage));
        repo.init()?;
        Ok(Self {
            repo,
            policy: Arc::new(PermissivePolicyGate),
            audit: Arc::new(NullSink),
            ref_name: "main".to_string(),
            ratify: None,
            db_path: None,
            fts: None,
        })
    }

    pub fn set_policy(&mut self, policy: Arc<dyn PolicyGate>) {
        self.policy = policy;
    }

    /// Load a policy file and install the real `PolicyStoreGate` backed by
    /// `agentstategraph-policy`. Rules are imported into an isolated
    /// in-memory repo at startup; all evaluation is delegated to `PolicyStore`.
    pub fn load_policy_file(&mut self, path: &Path) -> Result<()> {
        let gate = PolicyStoreGate::from_file(path)?;
        self.policy = Arc::new(gate);
        Ok(())
    }

    pub fn set_audit_sink(&mut self, sink: Arc<dyn AuditSink>) {
        self.audit = sink;
    }

    /// Install the commercial ratify implementation (Team tier).
    /// Called by `asd-pro` at startup before any subcommand dispatch.
    pub fn set_ratify_ops(&mut self, ratify: Arc<dyn RatifyOps>) {
        self.ratify = Some(ratify);
    }

    // -----------------------------------------------------------------------
    // High-level operation methods — preferred for new callers.
    // These enforce the policy gate and emit audit events automatically.
    // -----------------------------------------------------------------------

    /// Append a ledger entry. Evaluates the configured policy gate first;
    /// returns an error if the policy denies the operation. On success,
    /// emits a `ledger.append` audit event.
    pub fn append_ledger_entry(&self, entry: &LedgerEntry, agent_id: &str) -> Result<()> {
        let situation = Situation::new("append ledger entry")
            .with_qualifier("symbol_id", &entry.symbol_id)
            .with_qualifier("kind", entry.kind.as_str());
        let decision = self
            .policy
            .evaluate(&situation, actions::LEDGER_APPEND, agent_id)?;
        let (matched_policy, audit_outcome) = match &decision {
            Decision::Allow { matched_policy } => (matched_policy.clone(), "allowed"),
            Decision::RequireApproval { matched_policy, .. } => {
                (Some(matched_policy.clone()), "awaiting-approval")
            }
            Decision::NoPolicyMatch => (None, "allowed"),
            Decision::Deny {
                matched_policy,
                reason,
            } => {
                return Err(AsdError::Other(format!(
                    "policy denied by {matched_policy}: {reason}"
                )));
            }
        };

        let store = AsgLedgerStore {
            repo: &self.repo,
            fts: self.fts.as_ref(),
        };
        store.append_entry(&self.ref_name, entry, agent_id)?;

        let event = AuditEvent::new(event_types::LEDGER_APPEND, agent_id, "agent", audit_outcome)
            .with_subject(&entry.entry_id)
            .with_secondary(&entry.symbol_id)
            .with_matched_policy(matched_policy)
            .with_payload(json!({ "kind": entry.kind.as_str(), "tags": entry.tags }));
        emit_audit(self.audit.as_ref(), event);
        Ok(())
    }

    /// Write a symbol to the index. Policy-neutral (no gate); emits no audit
    /// event — indexing is a bulk background operation, not a user action.
    pub fn put_symbol(&self, symbol: &Symbol, agent_id: &str) -> Result<()> {
        let store = AsgIndexStore::new(&self.repo);
        store.put_symbol(&self.ref_name, symbol, agent_id)
    }

    /// Plan T: populate the SQLite fast-read caches (`asd_symbols_cache`,
    /// `asd_call_edges`, and — only if empty — the FTS search table) from the
    /// authoritative git trees.
    ///
    /// Reads git directly rather than going through `AsgIndexStore` so a
    /// stale cache can never be used as its own source. Called from two
    /// places:
    ///
    /// - `open_sqlite` self-heal, when git has symbols but the cache is cold
    ///   (born-cold hydrate DBs, or a past sync failure such as SQLITE_BUSY);
    /// - `asd hydrate`, right after loading the sidecar, so hydrate-created
    ///   DBs are warm before the command exits.
    ///
    /// Concurrency: `sync_symbols`/`sync_call_edges` are transactional
    /// full-replaces on a WAL connection with a busy timeout, and the
    /// operation is idempotent — concurrent callers converge on the same
    /// rows. In-memory engines (`fts: None`) return `skipped: true` and do
    /// nothing.
    pub fn warm_caches(&self) -> Result<CacheWarmSummary> {
        let Some(fts) = self.fts.as_ref() else {
            return Ok(CacheWarmSummary {
                skipped: true,
                ..Default::default()
            });
        };

        // Authoritative symbol read: one by-qname tree walk (qname-unique by
        // construction, matching `rebuild_refs`' dedup semantics).
        let symbols: Vec<Symbol> = self
            .repo
            .get_tree(&self.ref_name, "/asd/v1/index/by-qname")
            .ok()
            .and_then(|v| v.as_object().cloned())
            .map(|m| {
                m.into_values()
                    .filter_map(|v| serde_json::from_value::<Symbol>(v).ok())
                    .collect()
            })
            .unwrap_or_default();
        if symbols.is_empty() {
            // Nothing indexed yet — leave the caches alone so
            // `symbols_cached_for` keeps routing reads to git.
            return Ok(CacheWarmSummary::default());
        }

        // Authoritative edge read: one tree walk per direction. Shape:
        //   /asd/v1/index/callers/{symbol_id} = {"callers": [id, …]}
        //   /asd/v1/index/callees/{symbol_id} = {"callees": [id, …]}
        let read_direction = |dir: &str, field: &str| -> HashMap<String, Vec<String>> {
            let prefix = format!("{}/index/{}", crate::paths::ASD_ROOT, dir);
            match self.repo.get_tree(&self.ref_name, &prefix) {
                Ok(serde_json::Value::Object(map)) => map
                    .into_iter()
                    .map(|(symbol_id, v)| {
                        let ids = v
                            .get(field)
                            .and_then(|a| a.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (symbol_id, ids)
                    })
                    .collect(),
                _ => HashMap::new(),
            }
        };
        let callers_of = read_direction("callers", "callers");
        let callees_of = read_direction("callees", "callees");

        let sym_refs: Vec<&Symbol> = symbols.iter().collect();
        fts.sync_symbols(&sym_refs, &self.ref_name)
            .map_err(|e| AsdError::Other(format!("symbol cache sync failed: {e}")))?;
        fts.sync_call_edges(&callees_of, &callers_of, &self.ref_name)
            .map_err(|e| AsdError::Other(format!("edge cache sync failed: {e}")))?;

        // FTS search table: rebuild ONLY when empty. A populated table came
        // from `asd index` and already carries the ledger_text/ledger_flags
        // denormalizations; rebuilding it here would be a pure no-op cost.
        // An empty one (fresh hydrate DB) gets the same rebuild `asd index`
        // would produce — ledger data comes from the hydrated ledger tree.
        let mut fts_rebuilt = false;
        if fts.fts_symbol_row_count() == 0 {
            let ledger_data =
                crate::index_pipeline::build_ledger_fts_data(&self.repo, &self.ref_name);
            let mut deduped = sym_refs.clone();
            deduped.sort_by(|a, b| a.qname.cmp(&b.qname));
            match fts.rebuild_refs(&deduped, &ledger_data) {
                Ok(()) => fts_rebuilt = true,
                Err(e) => eprintln!("asd: FTS rebuild during cache warm failed: {e}"),
            }
        }

        let edges_cached = callers_of.values().map(Vec::len).sum::<usize>()
            + callees_of.values().map(Vec::len).sum::<usize>();
        Ok(CacheWarmSummary {
            symbols_cached: symbols.len(),
            edges_cached,
            fts_rebuilt,
            skipped: false,
        })
    }
}

/// Result of [`Engine::warm_caches`].
#[derive(Debug, Clone, Default)]
pub struct CacheWarmSummary {
    /// Rows written to `asd_symbols_cache` (0 when git had no symbols).
    pub symbols_cached: usize,
    /// Directed edge rows written to `asd_call_edges` (both directions).
    pub edges_cached: usize,
    /// Whether the FTS search table was empty and got rebuilt from git.
    pub fts_rebuilt: bool,
    /// `true` when the engine has no SQLite connection (in-memory engines) —
    /// there are no caches to warm.
    pub skipped: bool,
}
