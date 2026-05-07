use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::EffectDecl;

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
}

impl<'a> EffectStore for AsgEffectStore<'a> {
    fn get_effects(&self, ref_name: &str, symbol_id: &str) -> Result<Option<EffectDecl>> {
        let path = paths::effects_path(symbol_id);
        match self.repo.get_json(ref_name, &path) {
            Ok(value) => Ok(Some(serde_json::from_value(value)?)),
            Err(agentstategraph::RepoError::Tree(_)) => Ok(None),
            Err(e) => Err(AsdError::Repo(e)),
        }
    }

    fn put_effects(
        &self,
        ref_name: &str,
        symbol_id: &str,
        decl: &EffectDecl,
        agent_id: &str,
    ) -> Result<()> {
        let path = paths::effects_path(symbol_id);
        let value = serde_json::to_value(decl)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!("declare effects for {}", symbol_id),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;
        Ok(())
    }
}
