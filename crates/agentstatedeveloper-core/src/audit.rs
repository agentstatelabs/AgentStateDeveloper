//! Audit-log event stream — OSS data format.
//!
//! Every user-visible mutation (ledger append/approve/reject/withdraw/
//! supersede, effect_declare) and every policy decision is emitted as
//! an [`AuditEvent`] to a configurable [`AuditSink`]. The OSS binary
//! ships only [`NullSink`] (no-op) — the hash-chained file sink with
//! tamper-evident verification lives in the commercial
//! `agentstatedeveloper-audit-pro` crate (Enterprise tier).
//!
//! The schema here is shared between OSS and Pro: an OSS `asd` binary
//! can read a log written by `asd-pro` (forward-compatible degrade),
//! it just can't write chain-signed events or verify the chain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;
use std::path::Path;

/// Prefix for blake3-based event hashes. Versioned so we can change
/// the algorithm later without ambiguity. Shared between OSS readers
/// and Pro writers.
pub const HASH_PREFIX: &str = "b3:";

// ---------------------------------------------------------------------------
// Event vocabulary
// ---------------------------------------------------------------------------

/// Canonical event type strings. Stable — downstream SIEM rules key on these.
pub mod event_types {
    pub const LEDGER_APPEND: &str = "ledger.append";
    pub const LEDGER_APPROVE: &str = "ledger.approve";
    pub const LEDGER_REJECT: &str = "ledger.reject";
    pub const LEDGER_WITHDRAW: &str = "ledger.withdraw";
    pub const LEDGER_SUPERSEDE: &str = "ledger.supersede";
    pub const EFFECT_DECLARE: &str = "effect.declare";
}

/// One audit entry. Flat, JSON-serializable. Shared data format —
/// `event_hash` / `prev_event_hash` are empty strings / None when the
/// OSS binary emits (via `NullSink`) and filled in by the Pro sink.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// `evt_<uuid-simple>` — stable id for dedup / ordering.
    pub event_id: String,
    /// One of [`event_types`], or a custom extension string.
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary_id: Option<String>,
    pub actor_id: String,
    pub actor_kind: String,
    pub timestamp: DateTime<Utc>,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: serde_json::Value,
    /// Hash of the previous chained event, or `None` for chain start.
    /// Pro writers populate this; OSS writers leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_event_hash: Option<String>,
    /// Hash of THIS event's canonical bytes. Empty string means the
    /// event is unsigned — Pro writers fill it in, OSS writers don't.
    #[serde(default)]
    pub event_hash: String,
}

impl AuditEvent {
    pub fn new(
        event_type: impl Into<String>,
        actor_id: impl Into<String>,
        actor_kind: impl Into<String>,
        outcome: impl Into<String>,
    ) -> Self {
        Self {
            event_id: format!("evt_{}", Uuid::new_v4().simple()),
            event_type: event_type.into(),
            subject_id: None,
            secondary_id: None,
            actor_id: actor_id.into(),
            actor_kind: actor_kind.into(),
            timestamp: Utc::now(),
            outcome: outcome.into(),
            matched_policy: None,
            reason: None,
            payload: serde_json::Value::Null,
            prev_event_hash: None,
            event_hash: String::new(),
        }
    }

    /// True iff the event carries a chain hash. OSS readers can use
    /// this to distinguish signed (Pro-written) events from unsigned
    /// ones, even though OSS can't verify the chain.
    pub fn is_signed(&self) -> bool {
        !self.event_hash.is_empty()
    }

    pub fn with_subject(mut self, subject_id: impl Into<String>) -> Self {
        self.subject_id = Some(subject_id.into());
        self
    }

    pub fn with_secondary(mut self, secondary_id: impl Into<String>) -> Self {
        self.secondary_id = Some(secondary_id.into());
        self
    }

    pub fn with_matched_policy(mut self, matched_policy: Option<String>) -> Self {
        self.matched_policy = matched_policy;
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }
}

// ---------------------------------------------------------------------------
// Sink trait + OSS default
// ---------------------------------------------------------------------------

pub trait AuditSink: Send + Sync {
    fn emit(&self, event: &AuditEvent) -> Result<()>;

    fn emit_all(&self, events: &[AuditEvent]) -> Result<()> {
        for e in events {
            self.emit(e)?;
        }
        Ok(())
    }
}

/// Swallows every event. The OSS default — tamper-evident file-backed
/// audit is a commercial feature (see `agentstatedeveloper-audit-pro`).
pub struct NullSink;

impl AuditSink for NullSink {
    fn emit(&self, _event: &AuditEvent) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// JSONL reader — used by OSS `audit tail` AND by Pro `audit verify`
// ---------------------------------------------------------------------------

/// Read back events from a JSONL file. Shared by OSS read-only
/// consumers (`asd audit tail`) and the Pro `verify_chain` code path.
pub fn read_jsonl(path: &Path) -> Result<Vec<AuditEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: AuditEvent = serde_json::from_str(line)?;
        out.push(event);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialization_round_trip() {
        let e = AuditEvent::new(
            event_types::LEDGER_APPEND,
            "alice",
            "human",
            "awaiting-approval",
        )
        .with_subject("led_abc123")
        .with_secondary("sym_xyz")
        .with_matched_policy(Some("/policies/code/hazard@1".into()))
        .with_reason("requires human approval")
        .with_payload(serde_json::json!({"kind": "hazard", "qname": "payments.x"}));

        let json = serde_json::to_string(&e).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_id, e.event_id);
        assert_eq!(parsed.event_type, event_types::LEDGER_APPEND);
        assert_eq!(parsed.actor_id, "alice");
    }

    #[test]
    fn null_sink_accepts_everything() {
        let sink = NullSink;
        let e = AuditEvent::new("x", "y", "z", "success");
        sink.emit(&e).unwrap();
    }

    #[test]
    fn read_empty_path_returns_empty() {
        let path = std::path::PathBuf::from("/tmp/does-not-exist-asd-audit-test.jsonl");
        if path.exists() {
            std::fs::remove_file(&path).ok();
        }
        let events = read_jsonl(&path).unwrap();
        assert!(events.is_empty());
    }
}
