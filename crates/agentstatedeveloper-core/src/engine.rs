use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agentstategraph::Repository;
use agentstategraph_storage::SqliteStorage;

use crate::adapter::LanguageAdapter;
use crate::audit::{AuditSink, JsonlFileSink, NullSink};
use crate::error::{AsdError, Result};
use crate::policy::{FilePolicyGate, PermissivePolicyGate, PolicyGate};

/// The top-level ASD engine. Owns an ASG repository, registered language
/// adapters, and a policy gate. Cheap to construct; shared across CLI,
/// MCP, and library consumers.
pub struct Engine {
    pub repo: Repository,
    pub adapters: HashMap<String, Arc<dyn LanguageAdapter>>,
    pub policy: Arc<dyn PolicyGate>,
    pub audit: Arc<dyn AuditSink>,
    pub ref_name: String,
}

impl Engine {
    /// Open (or create) an ASD engine backed by a SQLite file.
    pub fn open_sqlite(db_path: &Path) -> Result<Self> {
        let storage = SqliteStorage::open(db_path).map_err(|e| AsdError::Other(e.to_string()))?;
        let repo = Repository::new(Box::new(storage));
        repo.init()?;
        Ok(Self {
            repo,
            adapters: HashMap::new(),
            policy: Arc::new(PermissivePolicyGate),
            audit: Arc::new(NullSink),
            ref_name: "main".to_string(),
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
            adapters: HashMap::new(),
            policy: Arc::new(PermissivePolicyGate),
            audit: Arc::new(NullSink),
            ref_name: "main".to_string(),
        })
    }

    pub fn register_adapter(&mut self, adapter: Arc<dyn LanguageAdapter>) {
        self.adapters
            .insert(adapter.language().to_string(), adapter);
    }

    pub fn adapter_for(&self, language: &str) -> Result<Arc<dyn LanguageAdapter>> {
        self.adapters
            .get(language)
            .cloned()
            .ok_or_else(|| AsdError::UnknownLanguage(language.into()))
    }

    pub fn set_policy(&mut self, policy: Arc<dyn PolicyGate>) {
        self.policy = policy;
    }

    /// Convenience: load a file-based policy gate from disk and swap it in.
    pub fn load_policy_file(&mut self, path: &Path) -> Result<()> {
        let gate = FilePolicyGate::from_file(path)?;
        self.policy = Arc::new(gate);
        Ok(())
    }

    pub fn set_audit_sink(&mut self, sink: Arc<dyn AuditSink>) {
        self.audit = sink;
    }

    /// Convenience: wire a JSONL-file audit sink at `path`.
    pub fn set_audit_log_file(&mut self, path: &Path) -> Result<()> {
        self.audit = Arc::new(JsonlFileSink::new(path.to_path_buf()));
        Ok(())
    }
}
