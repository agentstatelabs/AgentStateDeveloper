use std::path::Path;

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::EffectDecl;
use crate::search_fts::SearchFtsDb;

pub trait EffectStore {
    fn get_effects(&self, ref_name: &str, symbol_id: &str) -> Result<Option<EffectDecl>>;
    fn put_effects(
        &self,
        ref_name: &str,
        symbol_id: &str,
        decl: &EffectDecl,
        agent_id: &str,
    ) -> Result<()>;
}

pub struct AsgEffectStore<'a> {
    pub repo: &'a Repository,
    /// When `Some`, enables the SQLite write-through cache.
    pub db_path: Option<&'a Path>,
}

impl<'a> AsgEffectStore<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo, db_path: None }
    }
    pub fn with_cache(repo: &'a Repository, db_path: &'a Path) -> Self {
        Self { repo, db_path: Some(db_path) }
    }
}

impl<'a> EffectStore for AsgEffectStore<'a> {
    fn get_effects(&self, ref_name: &str, symbol_id: &str) -> Result<Option<EffectDecl>> {
        // Fast path: SQLite cache.
        if let Some(db) = self.db_path {
            if let Ok(fts) = SearchFtsDb::open(db) {
                if fts.effects_cached_for(symbol_id, ref_name) {
                    if let Ok(opt) = fts.get_effects_for(symbol_id, ref_name) {
                        return Ok(opt);
                    }
                }
            }
        }

        // Authoritative git path + populate cache as side effect.
        let path = paths::effects_path(symbol_id);
        let result = match self.repo.get_json(ref_name, &path) {
            Ok(value) => Ok(Some(serde_json::from_value::<EffectDecl>(value)?)),
            Err(agentstategraph::RepoError::Tree(_)) => Ok(None),
            Err(e) => Err(AsdError::Repo(e)),
        };
        if let Ok(Some(ref decl)) = result {
            if let Some(db) = self.db_path {
                if let Ok(fts) = SearchFtsDb::open(db) {
                    let _ = fts.upsert_effects(symbol_id, ref_name, decl);
                }
            }
        }
        result
    }

    fn put_effects(
        &self,
        ref_name: &str,
        symbol_id: &str,
        decl: &EffectDecl,
        agent_id: &str,
    ) -> Result<()> {
        // Git is authoritative — always write there first.
        let path = paths::effects_path(symbol_id);
        let value = serde_json::to_value(decl)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("declare effects for {}", symbol_id),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;
        // Best-effort SQLite write-through.
        if let Some(db) = self.db_path {
            if let Ok(fts) = SearchFtsDb::open(db) {
                let _ = fts.upsert_effects(symbol_id, ref_name, decl);
            }
        }
        Ok(())
    }
}
