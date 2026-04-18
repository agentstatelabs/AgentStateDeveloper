//! Policy gate.
//!
//! Two implementations ship today:
//! - [`PermissivePolicyGate`] — always Allow; the solo-dev default.
//! - [`FilePolicyGate`] — loads a JSON rule set from disk and evaluates
//!   incoming (action, situation) pairs against it. This is the
//!   interim enforcement path until the planned `agentstategraph-policy`
//!   sibling crate ships (see `strategy/POLICY_V1.md`). Schema is a
//!   strict subset of POLICY_V1 so migrating later is a rename.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

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

#[derive(Debug, Clone)]
pub struct Situation {
    pub description: String,
    pub qualifiers: serde_json::Value,
}

pub trait PolicyGate: Send + Sync {
    fn evaluate(&self, situation: &Situation, action: &str, agent_id: &str) -> Result<Decision>;
}

/// Solo-dev default: always Allow.
pub struct PermissivePolicyGate;

impl PolicyGate for PermissivePolicyGate {
    fn evaluate(
        &self,
        _situation: &Situation,
        _action: &str,
        _agent_id: &str,
    ) -> Result<Decision> {
        Ok(Decision::Allow {
            matched_policy: None,
        })
    }
}

// ---------------------------------------------------------------------------
// File-backed policy gate
// ---------------------------------------------------------------------------

/// A single rule. Matches actions either exactly or by prefix (when
/// `match_action` ends with `.*`). First-match wins; order matters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Policy path — reported back in `matched_policy`. Usually of the
    /// form `/policies/<domain>/<slug>`.
    pub path: String,
    /// Integer version — stamped into `matched_policy` as `<path>@<version>`.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Human-readable description (not evaluated).
    #[serde(default)]
    pub description: Option<String>,
    /// Action to match. Either exactly (`asd.ledger.append.hazard`)
    /// or prefixed (`asd.ledger.append.*` matches any subaction).
    pub match_action: String,
    /// Decision shape. If both `deny: true` and `require_approval` is
    /// non-empty, `require_approval` wins (approval implies not a hard
    /// deny).
    #[serde(default)]
    pub deny: bool,
    /// When non-empty, the decision is RequireApproval with these
    /// approver labels (e.g., ["human"], ["senior_agent", "human"]).
    #[serde(default)]
    pub require_approval: Vec<String>,
    /// Optional reason string surfaced to the caller on Deny / Approval.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional condition on the agent_id. When set, rule fires only if
    /// the caller's agent_id matches. Useful for "this agent can do X,
    /// others can't." Simple equality for M7.
    #[serde(default)]
    pub agent_id: Option<String>,
}

fn default_version() -> u32 {
    1
}

/// Top-level policy file shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub policies: Vec<PolicyRule>,
    /// When true (and nothing matches), evaluation returns `NoPolicyMatch`
    /// and the *caller* decides the fail-safe behavior. When false (the
    /// default), `NoPolicyMatch` → Allow so solo-dev flows aren't
    /// surprise-denied.
    #[serde(default)]
    pub strict: bool,
}

impl PolicyFile {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let pf: PolicyFile = serde_json::from_str(&raw)?;
        Ok(pf)
    }
}

/// Evaluates each incoming (action, agent_id, situation) against a
/// preloaded policy file. First matching rule wins.
pub struct FilePolicyGate {
    file: PolicyFile,
    source: String,
}

impl FilePolicyGate {
    pub fn from_file(path: &Path) -> Result<Self> {
        let file = PolicyFile::load(path)?;
        Ok(Self {
            file,
            source: path.display().to_string(),
        })
    }

    pub fn from_policy_file(file: PolicyFile, source: impl Into<String>) -> Self {
        Self {
            file,
            source: source.into(),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn rule_count(&self) -> usize {
        self.file.policies.len()
    }

    fn matches(rule: &PolicyRule, action: &str, agent_id: &str) -> bool {
        if let Some(pinned) = &rule.agent_id {
            if pinned != agent_id {
                return false;
            }
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
                reason: rule
                    .reason
                    .clone()
                    .unwrap_or_else(|| "policy deny".to_string()),
            }
        } else {
            Decision::Allow {
                matched_policy: Some(matched),
            }
        }
    }
}

impl PolicyGate for FilePolicyGate {
    fn evaluate(
        &self,
        _situation: &Situation,
        action: &str,
        agent_id: &str,
    ) -> Result<Decision> {
        for rule in &self.file.policies {
            if Self::matches(rule, action, agent_id) {
                return Ok(Self::decision_for(rule));
            }
        }
        if self.file.strict {
            Ok(Decision::NoPolicyMatch)
        } else {
            Ok(Decision::Allow {
                matched_policy: None,
            })
        }
    }
}

impl Decision {
    /// Canonical string rendering of `matched_policy` suitable for
    /// stamping into ledger / effect records. Returns None for
    /// unmatched permissive allows.
    pub fn matched_policy(&self) -> Option<String> {
        match self {
            Decision::Allow { matched_policy } => matched_policy.clone(),
            Decision::Deny { matched_policy, .. } => Some(matched_policy.clone()),
            Decision::RequireApproval { matched_policy, .. } => Some(matched_policy.clone()),
            Decision::NoPolicyMatch => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical ASD action vocabulary.
// ---------------------------------------------------------------------------

pub mod actions {
    pub const LEDGER_APPEND: &str = "asd.ledger.append";
    pub const LEDGER_APPEND_HAZARD: &str = "asd.ledger.append.hazard";
    pub const LEDGER_SUPERSEDE: &str = "asd.ledger.supersede";
    pub const EFFECT_DECLARE: &str = "asd.effect.declare";
    pub const EFFECT_DECLARE_BROADENS: &str = "asd.effect.declare.broadens";
    pub const CODE_READ: &str = "asd.code.read";
    pub const CODE_COMMIT: &str = "asd.code.commit";
    pub const MERGE_BRANCH_TO_MAIN: &str = "asd.merge.branch_to_main";
    pub const RENAME_SYMBOL: &str = "asd.rename.symbol";
    pub const RENAME_FILE: &str = "asd.rename.file";

    /// Build the specific action name for a ledger append given the kind.
    /// e.g. `ledger_append_action("hazard") -> "asd.ledger.append.hazard"`.
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
        FilePolicyGate::from_policy_file(
            PolicyFile {
                policies: rules,
                strict,
            },
            "test",
        )
    }

    fn sit() -> Situation {
        Situation {
            description: "t".into(),
            qualifiers: serde_json::Value::Null,
        }
    }

    #[test]
    fn exact_match_require_approval() {
        let gate = make_file(
            vec![PolicyRule {
                path: "/policies/code/hazard".into(),
                version: 1,
                description: None,
                match_action: actions::LEDGER_APPEND_HAZARD.into(),
                deny: false,
                require_approval: vec!["human".into()],
                reason: Some("audit".into()),
                agent_id: None,
            }],
            false,
        );
        let d = gate
            .evaluate(&sit(), actions::LEDGER_APPEND_HAZARD, "asd-mcp")
            .unwrap();
        match d {
            Decision::RequireApproval {
                matched_policy,
                approvers,
                reason,
            } => {
                assert_eq!(matched_policy, "/policies/code/hazard@1");
                assert_eq!(approvers, vec!["human"]);
                assert_eq!(reason.as_deref(), Some("audit"));
            }
            other => panic!("expected RequireApproval, got {:?}", other),
        }
    }

    #[test]
    fn prefix_wildcard_matches_suffix() {
        let gate = make_file(
            vec![PolicyRule {
                path: "/p/any-ledger".into(),
                version: 1,
                description: None,
                match_action: "asd.ledger.*".into(),
                deny: true,
                require_approval: vec![],
                reason: Some("paused".into()),
                agent_id: None,
            }],
            false,
        );
        let d = gate
            .evaluate(&sit(), "asd.ledger.append.decision", "whoever")
            .unwrap();
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn no_match_non_strict_is_allow() {
        let gate = make_file(vec![], false);
        let d = gate.evaluate(&sit(), "asd.anything", "x").unwrap();
        assert!(matches!(
            d,
            Decision::Allow {
                matched_policy: None
            }
        ));
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
        // Matching agent: deny.
        let d1 = gate.evaluate(&sit(), "asd.effect.declare", "bot-v1").unwrap();
        assert!(matches!(d1, Decision::Deny { .. }));
        // Other agent: allow (no rule matches, default allow).
        let d2 = gate.evaluate(&sit(), "asd.effect.declare", "bot-v2").unwrap();
        assert!(matches!(d2, Decision::Allow { .. }));
    }
}
