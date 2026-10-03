//! Writing a whole subtree in one speculation without reverting anyone.
//!
//! A nested `spec_set_json` per item copies every map on the path, so a
//! batch of N items stores N copies of the enclosing maps. The pattern here
//! instead forks a speculation, reads the subtree *at the commit it forked
//! from*, changes it in memory and writes it once: the speculation then
//! differs from its base by exactly the changed keys, and committing it — a
//! merge onto the current head — keeps every write that landed meanwhile.

use serde_json::Value;

use agentstategraph::Repository;

use crate::error::{AsdError, Result};

/// Fork a speculation from `ref_name` and return it with the commit it forked
/// from (as a ref string), so the caller can read exactly the state the
/// speculation starts from. A write landing between the two head reads leaves
/// the fork point unknown, so that attempt is discarded and retried.
pub(crate) fn speculate_at_head(
    repo: &Repository,
    ref_name: &str,
    label: &str,
) -> Result<(agentstategraph::SpecHandle, String)> {
    for _ in 0..8 {
        let before = repo
            .head(ref_name)
            .map_err(|e| AsdError::Other(e.to_string()))?;
        let spec = repo
            .speculate(ref_name, Some(label.into()))
            .map_err(|e| AsdError::Other(e.to_string()))?;
        match repo.head(ref_name) {
            Ok(after) if after == before => return Ok((spec, before.to_hex())),
            Ok(_) => {
                let _ = repo.discard_speculation(spec);
            }
            Err(e) => {
                let _ = repo.discard_speculation(spec);
                return Err(AsdError::Other(e.to_string()));
            }
        }
    }
    Err(AsdError::Other(format!(
        "{ref_name} moved on every attempt to fork a speculation from it"
    )))
}

/// Read a subtree as a JSON map, as the base a flush writes on top of. A
/// subtree that does not exist yet (a first index) is empty; any other read
/// error fails the index — an empty seed written back would replace the whole
/// subtree.
pub(crate) fn read_seed(
    repo: &Repository,
    at: &str,
    path: &str,
) -> Result<serde_json::Map<String, Value>> {
    match repo.get_tree(at, path) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Ok(serde_json::Map::new()),
        Err(agentstategraph::RepoError::Tree(agentstategraph::tree::TreeError::PathNotFound(
            _,
        ))) => Ok(serde_json::Map::new()),
        Err(e) => Err(AsdError::Other(format!("read {path}: {e}"))),
    }
}
