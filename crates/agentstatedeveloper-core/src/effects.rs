use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;

use crate::engine::Engine;
use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::{
    Effect, EffectDecl, Mismatch, Verification, VerificationSource, VerificationStatus,
};
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

/// What a re-index stores for a symbol, given what is stored for it now and
/// what its adapter inferred this run (`inferred` stamped with
/// [`Effect::adapter`]).
///
/// The index owns inferred effects and the static verification of them —
/// nothing anyone else recorded:
/// - A declaration made by hand (`effect_declare`; its effects carry no
///   `adapter`) stands. This run's inference is recorded against it as
///   verification mismatches, the same ones `verify-effects` reports.
/// - Otherwise `declared` becomes this run's inference.
/// - A static verification is replaced when its verdict changed — an
///   unchanged one keeps its time, so a re-index leaves an unchanged symbol's
///   record as it was. A runtime or test verification is evidence the index
///   cannot reproduce, and is kept. So are runtime confidence,
///   `confidence`, the matched policy, and the transitive effects (which the
///   transitive pass recomputes).
///
/// Effects inferred before inference was stamped carry no `adapter` either. A
/// list identical to this run's inference is taken as inferred; any other is
/// kept as declared — a stale inference then shows up as a mismatch rather
/// than a declaration being dropped.
pub fn merge_reindexed_effects(
    existing: Option<EffectDecl>,
    symbol_id: &str,
    inferred: Vec<Effect>,
    file: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> EffectDecl {
    let mut decl = existing.unwrap_or_else(|| EffectDecl {
        symbol_id: symbol_id.to_string(),
        declared: Vec::new(),
        transitive: Vec::new(),
        verification: None,
        confidence: None,
        runtime: None,
        matched_policy: None,
    });
    decl.symbol_id = symbol_id.to_string();

    let declared_by_hand = decl.declared.iter().any(|e| e.adapter.is_none())
        && !same_effects(&decl.declared, &inferred);

    let (status, mismatches) = if declared_by_hand {
        let mismatches = static_mismatches(&decl.declared, &inferred, file);
        let status = if mismatches.is_empty() {
            VerificationStatus::Ok
        } else {
            VerificationStatus::Mismatch
        };
        (status, mismatches)
    } else {
        // An empty inference means the adapter could not determine effects,
        // not that it confirmed purity — Unverified until a trace or a
        // declaration says more.
        let status = if inferred.is_empty() {
            VerificationStatus::Unverified
        } else {
            VerificationStatus::Ok
        };
        decl.declared = inferred;
        (status, Vec::new())
    };

    let fresh = Verification {
        by: VerificationSource::StaticChecker,
        at: now,
        status,
        mismatches,
    };
    match &decl.verification {
        None => decl.verification = Some(fresh),
        Some(v) if matches!(v.by, VerificationSource::StaticChecker) => {
            let verdict = |v: &Verification| serde_json::to_value((&v.status, &v.mismatches)).ok();
            if verdict(v) != verdict(&fresh) {
                decl.verification = Some(fresh);
            }
        }
        Some(_) => {}
    }
    decl
}

/// Same effects in any order, ignoring the provenance stamp and the
/// `verify-effects` flag.
fn same_effects(a: &[Effect], b: &[Effect]) -> bool {
    let key = |e: &Effect| {
        (
            e.effect.clone(),
            e.qualifiers.to_string(),
            e.note.clone(),
            e.source_pattern.clone(),
        )
    };
    let mut a: Vec<_> = a.iter().map(key).collect();
    let mut b: Vec<_> = b.iter().map(key).collect();
    a.sort();
    b.sort();
    a == b
}

/// Declared-vs-inferred mismatches by category, as `verify-effects` reports
/// them.
fn static_mismatches(declared: &[Effect], inferred: &[Effect], file: &str) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();
    for d in declared {
        if !inferred.iter().any(|i| i.effect == d.effect) {
            mismatches.push(Mismatch {
                kind: "declared_not_inferred".to_string(),
                effect: d.effect.clone(),
                detected_in: Some(file.to_string()),
                note: Some("declared but not found by static checker".to_string()),
            });
        }
    }
    for i in inferred {
        if !declared.iter().any(|d| d.effect == i.effect) {
            mismatches.push(Mismatch {
                kind: "inferred_not_declared".to_string(),
                effect: i.effect.clone(),
                detected_in: Some(file.to_string()),
                note: Some("found by static checker but not in declared effects".to_string()),
            });
        }
    }
    mismatches
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EffectCategory, RuntimeEvidence, TransitiveEffect};
    use chrono::Utc;

    fn inferred(category: EffectCategory) -> Effect {
        Effect {
            adapter: Some("python".into()),
            ..Effect::new(category)
        }
    }

    fn by_hand(category: EffectCategory, note: &str) -> Effect {
        Effect {
            note: Some(note.into()),
            ..Effect::new(category)
        }
    }

    fn decl(declared: Vec<Effect>) -> EffectDecl {
        EffectDecl {
            symbol_id: "sym".into(),
            declared,
            transitive: Vec::new(),
            verification: None,
            confidence: None,
            runtime: None,
            matched_policy: None,
        }
    }

    /// `Effect` has no `PartialEq`; compare what is stored.
    fn json(effects: &[Effect]) -> serde_json::Value {
        serde_json::to_value(effects).unwrap()
    }

    fn kinds(d: &EffectDecl) -> Vec<(String, String)> {
        let v = d.verification.as_ref().expect("verification");
        v.mismatches
            .iter()
            .map(|m| (m.kind.clone(), m.effect.as_str().to_string()))
            .collect()
    }

    #[test]
    fn a_new_symbol_gets_its_inferred_effects() {
        let d = merge_reindexed_effects(
            None,
            "sym",
            vec![inferred(EffectCategory::IoNetOut)],
            "a.py",
            Utc::now(),
        );
        assert_eq!(
            json(&d.declared),
            json(&[inferred(EffectCategory::IoNetOut)])
        );
        let v = d.verification.unwrap();
        assert!(matches!(v.by, VerificationSource::StaticChecker));
        assert!(matches!(v.status, VerificationStatus::Ok));

        let none = merge_reindexed_effects(None, "sym", Vec::new(), "a.py", Utc::now());
        assert!(matches!(
            none.verification.unwrap().status,
            VerificationStatus::Unverified
        ));
    }

    #[test]
    fn a_hand_declaration_survives_and_is_checked_against_the_inference() {
        let declared = vec![by_hand(EffectCategory::IoNetOut, "calls the rates API")];
        let d = merge_reindexed_effects(
            Some(decl(declared.clone())),
            "sym",
            vec![inferred(EffectCategory::IoFsRead)],
            "a.py",
            Utc::now(),
        );
        assert_eq!(json(&d.declared), json(&declared));
        assert!(matches!(
            d.verification.as_ref().unwrap().status,
            VerificationStatus::Mismatch
        ));
        assert_eq!(
            kinds(&d),
            vec![
                ("declared_not_inferred".into(), "io.net.out".into()),
                ("inferred_not_declared".into(), "io.fs.read".into()),
            ]
        );
    }

    #[test]
    fn a_previous_inference_is_replaced() {
        let d = merge_reindexed_effects(
            Some(decl(vec![inferred(EffectCategory::IoNetOut)])),
            "sym",
            vec![inferred(EffectCategory::IoFsRead)],
            "a.py",
            Utc::now(),
        );
        assert_eq!(
            json(&d.declared),
            json(&[inferred(EffectCategory::IoFsRead)])
        );
        assert!(kinds(&d).is_empty());
    }

    #[test]
    fn an_unstamped_list_equal_to_the_inference_is_taken_as_inferred() {
        // Stored before inferred effects were stamped with their adapter.
        let legacy = Effect::new(EffectCategory::IoNetOut);
        let first = merge_reindexed_effects(
            Some(decl(vec![legacy])),
            "sym",
            vec![inferred(EffectCategory::IoNetOut)],
            "a.py",
            Utc::now(),
        );
        assert_eq!(
            json(&first.declared),
            json(&[inferred(EffectCategory::IoNetOut)])
        );

        // Now stamped, so the next change to the code replaces it.
        let next = merge_reindexed_effects(
            Some(first),
            "sym",
            vec![inferred(EffectCategory::IoFsWrite)],
            "a.py",
            Utc::now(),
        );
        assert_eq!(
            json(&next.declared),
            json(&[inferred(EffectCategory::IoFsWrite)])
        );
    }

    #[test]
    fn runtime_evidence_confidence_policy_and_transitive_are_kept() {
        let traced = Verification {
            by: VerificationSource::RuntimeTracer,
            at: Utc::now(),
            status: VerificationStatus::Ok,
            mismatches: Vec::new(),
        };
        let runtime = RuntimeEvidence {
            confirmations: 3,
            contradictions: 0,
            prior: 0.5,
            last_trace_id: Some("trace-1".into()),
            last_observed_at: Utc::now(),
        };
        let mut existing = decl(vec![inferred(EffectCategory::IoNetOut)]);
        existing.verification = Some(traced.clone());
        existing.confidence = Some(0.9);
        existing.runtime = Some(runtime);
        existing.matched_policy = Some("net-ok".into());
        existing.transitive = vec![TransitiveEffect {
            effect: EffectCategory::IoFsRead,
            via: vec!["callee".into()],
            qualifiers: serde_json::Value::Null,
        }];

        let d = merge_reindexed_effects(
            Some(existing.clone()),
            "sym",
            vec![inferred(EffectCategory::IoNetOut)],
            "a.py",
            Utc::now(),
        );
        let v = d.verification.unwrap();
        assert!(matches!(v.by, VerificationSource::RuntimeTracer));
        assert_eq!(v.at, traced.at);
        assert_eq!(d.confidence, Some(0.9));
        assert_eq!(d.runtime.unwrap().confirmations, 3);
        assert_eq!(d.matched_policy.as_deref(), Some("net-ok"));
        assert_eq!(d.transitive.len(), 1);
    }

    #[test]
    fn a_static_verification_is_restamped_only_when_its_verdict_changes() {
        let then = Utc::now() - chrono::Duration::hours(1);
        let mut existing = decl(vec![inferred(EffectCategory::IoNetOut)]);
        existing.verification = Some(Verification {
            by: VerificationSource::StaticChecker,
            at: then,
            status: VerificationStatus::Ok,
            mismatches: Vec::new(),
        });
        let now = Utc::now();
        let same = merge_reindexed_effects(
            Some(existing.clone()),
            "sym",
            vec![inferred(EffectCategory::IoNetOut)],
            "a.py",
            now,
        );
        assert_eq!(
            serde_json::to_value(&same).unwrap(),
            serde_json::to_value(&existing).unwrap(),
            "an unchanged symbol's record must come back byte-identical"
        );

        let changed = merge_reindexed_effects(Some(existing), "sym", Vec::new(), "a.py", now);
        let v = changed.verification.unwrap();
        assert_eq!(v.at, now);
        assert!(matches!(v.status, VerificationStatus::Unverified));
    }
}
