//! Policy gate — M1 stub. Real enforcement lands when
//! `agentstategraph-policy` ships (see POLICY_V1.md).

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

/// Default M1 gate: always Allow. Swap for an agentstategraph-policy-backed
/// implementation when the crate ships.
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

/// Canonical ASD action vocabulary for policy queries. Published as a
/// reference taxonomy for any policy crate consumer.
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
}
