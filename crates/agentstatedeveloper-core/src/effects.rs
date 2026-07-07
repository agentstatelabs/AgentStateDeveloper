use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::engine::Engine;
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
    /// Borrowed FTS connection from the owning `Engine`.  When `Some`,
    /// enables the SQLite write-through cache without a per-call
    /// `Connection::open`.
    pub fts: Option<&'a SearchFtsDb>,
}

impl<'a> AsgEffectStore<'a> {
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo, fts: None }
    }
    /// Convenience: borrow the FTS connection already open in `engine`.
    pub fn from_engine(engine: &'a Engine) -> Self {
        Self {
            repo: &engine.repo,
            fts: engine.fts.as_ref(),
        }
    }
}

/// List every stored `EffectDecl` in the workspace as `(symbol_id, decl)`
/// pairs by walking the `/asd/v1/effects` subtree. Read-only. Used by
/// overview-style consumers (e.g. `asd-serve`'s `/api/v1/effects/overview`)
/// that need the whole distribution rather than one symbol's decl — the
/// per-symbol path stays `EffectStore::get_effects`.
pub fn list_all_effect_decls(
    repo: &Repository,
    ref_name: &str,
) -> Result<Vec<(String, EffectDecl)>> {
    let prefix = format!("{}/effects", paths::ASD_ROOT);
    let tree = match repo.get_tree(ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out: Vec<(String, EffectDecl)> = Vec::new();
    if let serde_json::Value::Object(map) = tree {
        for (symbol_id, value) in map {
            if let Ok(decl) = serde_json::from_value::<EffectDecl>(value) {
                out.push((symbol_id, decl));
            }
        }
    }
    Ok(out)
}

impl<'a> EffectStore for AsgEffectStore<'a> {
    fn get_effects(&self, ref_name: &str, symbol_id: &str) -> Result<Option<EffectDecl>> {
        // Fast path: SQLite cache.
        if let Some(fts) = self.fts {
            if fts.effects_cached_for(symbol_id, ref_name) {
                if let Ok(opt) = fts.get_effects_for(symbol_id, ref_name) {
                    return Ok(opt);
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
            if let Some(fts) = self.fts {
                let _ = fts.upsert_effects(symbol_id, ref_name, decl);
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
        if let Some(fts) = self.fts {
            let _ = fts.upsert_effects(symbol_id, ref_name, decl);
        }
        Ok(())
    }
}
