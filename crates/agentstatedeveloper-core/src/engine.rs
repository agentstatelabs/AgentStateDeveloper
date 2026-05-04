use std::path::Path;
use std::sync::Arc;

use agentstategraph::Repository;
use agentstategraph_storage::SqliteStorage;

use crate::audit::{AuditSink, AuditEvent, NullSink, emit_audit, event_types};
use crate::error::{AsdError, Result};
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore, RatifyOps};
use crate::policy::{Decision, PermissivePolicyGate, PolicyGate, PolicyStoreGate, Situation, actions};
use crate::schema::{LedgerEntry, Symbol};
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
}

impl Engine {
    /// Open (or create) an ASD engine backed by a SQLite file.
    pub fn open_sqlite(db_path: &Path) -> Result<Self> {
        let storage = SqliteStorage::open(db_path).map_err(|e| AsdError::Other(e.to_string()))?;
        let repo = Repository::new(Box::new(storage));
        repo.init()?;
        Ok(Self {
            repo,
            policy: Arc::new(PermissivePolicyGate),
            audit: Arc::new(NullSink),
            ref_name: "main".to_string(),
            ratify: None,
        })
    }

    /// Open an in-memory engine — mostly for tests.
    pub fn open_in_memory() -> Result<Self> {
        let storage =
            SqliteStorage::in_memory().map_err(|e| AsdError::Other(e.to_string()))?;
        let repo = Repository::new(Box::new(storage));
        repo.init()?;
        Ok(Self {
            repo,
            policy: Arc::new(PermissivePolicyGate),
            audit: Arc::new(NullSink),
            ref_name: "main".to_string(),
            ratify: None,
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
        let decision = self.policy.evaluate(&situation, actions::LEDGER_APPEND, agent_id)?;
        let matched_policy = match &decision {
            Decision::Allow { matched_policy } => matched_policy.clone(),
            Decision::RequireApproval { matched_policy, .. } => Some(matched_policy.clone()),
            Decision::NoPolicyMatch => None,
            Decision::Deny { matched_policy, reason } => {
                return Err(AsdError::Other(format!(
                    "policy denied by {matched_policy}: {reason}"
                )));
            }
        };

        let store = AsgLedgerStore { repo: &self.repo };
        store.append_entry(&self.ref_name, entry, agent_id)?;

        let event = AuditEvent::new(
            event_types::LEDGER_APPEND,
            agent_id,
            "agent",
            "awaiting-approval",
        )
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
        let store = AsgIndexStore { repo: &self.repo };
        store.put_symbol(&self.ref_name, symbol, agent_id)
    }
}
