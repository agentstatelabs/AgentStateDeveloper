//! Policy gate.
//!
//! Three implementations:
//! - [`PermissivePolicyGate`] — always Allow; the solo-dev default.
//! - [`FilePolicyGate`] — loads a JSON rule set from disk and evaluates
//!   in-process. Kept for tests and backward compat.
//! - [`PolicyStoreGate`] — wraps `agentstategraph_policy::PolicyStore`;
//!   imports the JSON policy file into the ASG repo at startup and
//!   delegates all evaluation to the real policy engine. This is the
//!   production path when `--policy` / `ASD_POLICY` is set.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use agentstategraph::Repository;
use agentstategraph_storage::SqliteStorage;
use agentstategraph_policy::{
    PolicyStore,
    Situation as AsgSituation,
    types::{ApprovalRule, AuthorizedAction, FallbackAction, Policy},
    selector::Selector,
};

use crate::error::Result;
use crate::AsdError;

// ---------------------------------------------------------------------------
// ASD-local Decision + Situation + PolicyGate trait
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Decision {
    Allow {
        matched_policy: Option<String>,
    },
    Deny {
        matched_policy: String,
        reason: String,
    },
    RequireApproval {
        matched_policy: String,
        approvers: Vec<String>,
        reason: Option<String>,
    },
    NoPolicyMatch,
}

impl Decision {
    pub fn matched_policy(&self) -> Option<String> {
        match self {
            Decision::Allow { matched_policy } => matched_policy.clone(),
            Decision::Deny { matched_policy, .. } => Some(matched_policy.clone()),
            Decision::RequireApproval { matched_policy, .. } => Some(matched_policy.clone()),
            Decision::NoPolicyMatch => None,
        }
    }
}

/// Context passed to every policy evaluation call. Keys are populated by
/// the call site so selectors can key on symbol_id, agent_id, etc.
#[derive(Debug, Clone, Default)]
pub struct Situation {
    pub description: String,
    pub qualifiers: serde_json::Value,
}

impl Situation {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            qualifiers: serde_json::Value::Null,
        }
    }

    pub fn with_qualifier(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if !self.qualifiers.is_object() {
            self.qualifiers = serde_json::Value::Object(serde_json::Map::new());
        }
        self.qualifiers
            .as_object_mut()
            .unwrap()
            .insert(key.into(), serde_json::Value::String(value.into()));
        self
    }
}

pub trait PolicyGate: Send + Sync {
    fn evaluate(&self, situation: &Situation, action: &str, agent_id: &str) -> Result<Decision>;
}

// ---------------------------------------------------------------------------
// PermissivePolicyGate
// ---------------------------------------------------------------------------

pub struct PermissivePolicyGate;

impl PolicyGate for PermissivePolicyGate {
    fn evaluate(&self, _: &Situation, _: &str, _: &str) -> Result<Decision> {
        Ok(Decision::Allow { matched_policy: None })
    }
}

// ---------------------------------------------------------------------------
// PolicyStoreGate — production path (wraps agentstategraph-policy)
// ---------------------------------------------------------------------------

/// Wraps an `agentstategraph_policy::PolicyStore`. Rules from the JSON policy
/// file are imported into a dedicated in-memory ASG repo at construction;
/// all evaluation is then delegated to the real `PolicyStore` engine.
///
/// Using a separate in-memory repo (rather than the main ASD repo) keeps
/// policy storage isolated and avoids requiring `Repository: Clone`.
pub struct PolicyStoreGate {
    store: PolicyStore,
    ref_name: String,
}

impl PolicyStoreGate {
    /// Load `path` as a [`PolicyFile`], import every rule into a fresh
    /// in-memory ASG repo, and return a gate ready for evaluation.
    pub fn from_file(path: &Path) -> Result<Self> {
        let file = PolicyFile::load(path)?;

        let storage = SqliteStorage::in_memory()
            .map_err(|e| AsdError::Other(e.to_string()))?;
        let repo = Repository::new(Box::new(storage));
        repo.init()?;
        let ref_name = "main";

        let store = PolicyStore::new(Arc::new(repo), "/asd/policies", "asd-policy-import");

        for rule in &file.policies {
            let policy = rule_to_policy(rule, file.strict);
            let handle = match store.propose(ref_name, policy) {
                Ok(h) => h,
                Err(agentstategraph_policy::PolicyError::AlreadyExists(_)) => continue,
                Err(e) => return Err(AsdError::Other(e.to_string())),
            };
            // `handle` is "path@version" — ratify takes just the path.
            let policy_path = handle.rsplitn(2, '@').nth(1).unwrap_or(&handle);
            store
                .ratify(ref_name, policy_path, "asd-policy-import", "imported from policy file")
                .map_err(|e| AsdError::Other(e.to_string()))?;
        }

        Ok(Self { store, ref_name: ref_name.to_string() })
    }
}

impl PolicyGate for PolicyStoreGate {
    fn evaluate(&self, situation: &Situation, action: &str, agent_id: &str) -> Result<Decision> {
        let asg_sit = asd_situation_to_asg(situation, agent_id);
        let decision = self
            .store
            .evaluate(&self.ref_name, &asg_sit, action, agent_id)
            .map_err(|e| AsdError::Other(e.to_string()))?;
        Ok(asg_decision_to_asd(decision))
    }
}

// ---------------------------------------------------------------------------
// FilePolicyGate — kept for tests and backward compat
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    pub path: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub description: Option<String>,
    pub match_action: String,
    #[serde(default)]
    pub deny: bool,
    #[serde(default)]
    pub require_approval: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

fn default_version() -> u32 { 1 }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub policies: Vec<PolicyRule>,
    #[serde(default)]
    pub strict: bool,
}

impl PolicyFile {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }
}

pub struct FilePolicyGate {
    file: PolicyFile,
    source: String,
}

impl FilePolicyGate {
    pub fn from_file(path: &Path) -> Result<Self> {
        Ok(Self {
            file: PolicyFile::load(path)?,
            source: path.display().to_string(),
        })
    }

    pub fn from_policy_file(file: PolicyFile, source: impl Into<String>) -> Self {
        Self { file, source: source.into() }
    }

    pub fn source(&self) -> &str { &self.source }
    pub fn rule_count(&self) -> usize { self.file.policies.len() }

    fn matches(rule: &PolicyRule, action: &str, agent_id: &str) -> bool {
        if let Some(pinned) = &rule.agent_id {
            if pinned != agent_id { return false; }
        }
        if let Some(prefix) = rule.match_action.strip_suffix(".*") {
            action == prefix || action.starts_with(&format!("{}.", prefix))
        } else {
            rule.match_action == action
        }
    }

    fn decision_for(rule: &PolicyRule) -> Decision {
        let matched = format!("{}@{}", rule.path, rule.version);
        if !rule.require_approval.is_empty() {
            Decision::RequireApproval {
                matched_policy: matched,
                approvers: rule.require_approval.clone(),
                reason: rule.reason.clone(),
            }
        } else if rule.deny {
            Decision::Deny {
                matched_policy: matched,
                reason: rule.reason.clone().unwrap_or_else(|| "policy deny".to_string()),
            }
        } else {
            Decision::Allow { matched_policy: Some(matched) }
        }
    }
}

impl PolicyGate for FilePolicyGate {
    fn evaluate(&self, _: &Situation, action: &str, agent_id: &str) -> Result<Decision> {
        for rule in &self.file.policies {
            if Self::matches(rule, action, agent_id) {
                return Ok(Self::decision_for(rule));
            }
        }
        if self.file.strict {
            Ok(Decision::NoPolicyMatch)
        } else {
            Ok(Decision::Allow { matched_policy: None })
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Convert an ASD `Situation` to an ASG `Situation` (HashMap<String,String>).
/// Flat qualifiers are extracted; `description` and `agent_id` are added as facts.
fn asd_situation_to_asg(situation: &Situation, agent_id: &str) -> AsgSituation {
    let mut s = AsgSituation::new()
        .with("description", &situation.description)
        .with("agent_id", agent_id);
    if let serde_json::Value::Object(map) = &situation.qualifiers {
        for (k, v) in map {
            if let Some(sv) = v.as_str() {
                s = s.with(k.clone(), sv);
            } else {
                s = s.with(k.clone(), v.to_string());
            }
        }
    }
    s
}

/// Map an ASG `Decision` back to ASD's `Decision`.
fn asg_decision_to_asd(d: agentstategraph_policy::Decision) -> Decision {
    use agentstategraph_policy::Decision as D;
    match d {
        D::Allow { matched_policy, .. } => Decision::Allow {
            matched_policy: Some(matched_policy),
        },
        D::Deny { matched_policy, reason } => Decision::Deny {
            matched_policy,
            reason,
        },
        D::RequireApproval { matched_policy, approvers, .. } => Decision::RequireApproval {
            matched_policy,
            approvers,
            reason: None,
        },
        D::NoPolicyMatch => Decision::NoPolicyMatch,
    }
}

/// Convert an ASD `PolicyRule` to an ASG `Policy` ready for `propose`.
fn rule_to_policy(rule: &PolicyRule, _strict: bool) -> Policy {
    let situation_selector = match &rule.agent_id {
        Some(id) => Selector::Eq { key: "agent_id".to_string(), value: id.clone() },
        None => Selector::Always,
    };

    let mut allow = Vec::new();
    let mut deny = Vec::new();
    let mut require_approval = Vec::new();

    if !rule.require_approval.is_empty() {
        require_approval.push(ApprovalRule {
            action: rule.match_action.clone(),
            approvers: rule.require_approval.clone(),
            timeout: None,
            fallback: FallbackAction::Block,
        });
    } else if rule.deny {
        deny.push(AuthorizedAction {
            action: rule.match_action.clone(),
            condition: rule.reason.clone(),
            preconditions: Vec::new(),
        });
    } else {
        allow.push(AuthorizedAction {
            action: rule.match_action.clone(),
            condition: None,
            preconditions: Vec::new(),
        });
    }

    Policy {
        path: rule.path.trim_start_matches('/').to_string(),
        version: 1,
        situation: rule.description.clone().unwrap_or_else(|| rule.path.clone()),
        situation_selector,
        allow,
        deny,
        require_approval,
        procedure: None,
        triggers: Vec::new(),
        required_fields: Vec::new(),
        severity: Default::default(),
        proposed_by: "asd-policy-import".to_string(),
        proposed_at: Utc::now(),
        ratified_by: None,
        ratified_at: None,
        ratification_reasoning: None,
        active_from: Utc::now(),
        expires_at: None,
        supersedes: None,
        tenant_id: None,
        external_evaluator: None,
        signature: None,
    }
}

// ---------------------------------------------------------------------------
// Canonical ASD action vocabulary
// ---------------------------------------------------------------------------

pub mod actions {
    pub const LEDGER_APPEND: &str = "asd.ledger.append";
    pub const LEDGER_APPEND_HAZARD: &str = "asd.ledger.append.hazard";
    pub const LEDGER_SUPERSEDE: &str = "asd.ledger.supersede";
    pub const LEDGER_APPROVE: &str = "asd.ledger.approve";
    pub const LEDGER_REJECT: &str = "asd.ledger.reject";
    pub const LEDGER_WITHDRAW: &str = "asd.ledger.withdraw";
    pub const LEDGER_REBIND: &str = "asd.ledger.rebind";
    pub const EFFECT_DECLARE: &str = "asd.effect.declare";
    pub const EFFECT_DECLARE_BROADENS: &str = "asd.effect.declare.broadens";
    pub const CODE_READ: &str = "asd.code.read";
    pub const CODE_COMMIT: &str = "asd.code.commit";
    pub const MERGE_BRANCH_TO_MAIN: &str = "asd.merge.branch_to_main";
    pub const RENAME_SYMBOL: &str = "asd.rename.symbol";
    pub const RENAME_FILE: &str = "asd.rename.file";

    pub fn ledger_append_action(kind: &str) -> String {
        format!("{}.{}", LEDGER_APPEND, kind)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_file(rules: Vec<PolicyRule>, strict: bool) -> FilePolicyGate {
        FilePolicyGate::from_policy_file(PolicyFile { policies: rules, strict }, "test")
    }

    fn sit() -> Situation {
        Situation::new("test")
    }

    #[test]
    fn exact_match_require_approval() {
        let gate = make_file(vec![PolicyRule {
            path: "/policies/code/hazard".into(),
            version: 1,
            description: None,
            match_action: actions::LEDGER_APPEND_HAZARD.into(),
            deny: false,
            require_approval: vec!["human".into()],
            reason: Some("audit".into()),
            agent_id: None,
        }], false);
        let d = gate.evaluate(&sit(), actions::LEDGER_APPEND_HAZARD, "asd-mcp").unwrap();
        match d {
            Decision::RequireApproval { matched_policy, approvers, reason } => {
                assert_eq!(matched_policy, "/policies/code/hazard@1");
                assert_eq!(approvers, vec!["human"]);
                assert_eq!(reason.as_deref(), Some("audit"));
            }
            other => panic!("expected RequireApproval, got {:?}", other),
        }
    }

    #[test]
    fn prefix_wildcard_matches_suffix() {
        let gate = make_file(vec![PolicyRule {
            path: "/p/any-ledger".into(),
            version: 1,
            description: None,
            match_action: "asd.ledger.*".into(),
            deny: true,
            require_approval: vec![],
            reason: Some("paused".into()),
            agent_id: None,
        }], false);
        let d = gate.evaluate(&sit(), "asd.ledger.append.decision", "whoever").unwrap();
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn no_match_non_strict_is_allow() {
        let gate = make_file(vec![], false);
        let d = gate.evaluate(&sit(), "asd.anything", "x").unwrap();
        assert!(matches!(d, Decision::Allow { matched_policy: None }));
    }

    #[test]
    fn no_match_strict_is_no_policy_match() {
        let gate = make_file(vec![], true);
        let d = gate.evaluate(&sit(), "asd.anything", "x").unwrap();
        assert!(matches!(d, Decision::NoPolicyMatch));
    }

    #[test]
    fn agent_id_gated_rule_only_fires_for_matching_agent() {
        let rule = PolicyRule {
            path: "/p/restricted".into(),
            version: 1,
            description: None,
            match_action: "asd.effect.declare".into(),
            deny: true,
            require_approval: vec![],
            reason: None,
            agent_id: Some("bot-v1".into()),
        };
        let gate = make_file(vec![rule], false);
        let d1 = gate.evaluate(&sit(), "asd.effect.declare", "bot-v1").unwrap();
        assert!(matches!(d1, Decision::Deny { .. }));
        let d2 = gate.evaluate(&sit(), "asd.effect.declare", "bot-v2").unwrap();
        assert!(matches!(d2, Decision::Allow { .. }));
    }

    #[test]
    fn policy_store_gate_deny() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, r#"{{"policies":[{{"path":"/policies/test/no-tradeoffs","match_action":"asd.ledger.append.tradeoff","deny":true,"reason":"disabled"}}],"strict":false}}"#).unwrap();
        let gate = PolicyStoreGate::from_file(f.path()).expect("from_file");
        let sit = Situation::new("test");
        let d = gate.evaluate(&sit, "asd.ledger.append.tradeoff", "agent").unwrap();
        assert!(matches!(d, Decision::Deny { .. }), "expected Deny, got {:?}", d);
    }

    #[test]
    fn policy_store_gate_allow_other_action() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, r#"{{"policies":[{{"path":"/policies/test/no-tradeoffs","match_action":"asd.ledger.append.tradeoff","deny":true,"reason":"disabled"}}],"strict":false}}"#).unwrap();
        let gate = PolicyStoreGate::from_file(f.path()).expect("from_file");
        let sit = Situation::new("test");
        let d = gate.evaluate(&sit, "asd.ledger.append.decision", "agent").unwrap();
        assert!(
            matches!(d, Decision::Allow { .. } | Decision::NoPolicyMatch),
            "expected Allow/NoPolicyMatch, got {:?}",
            d
        );
    }
}
