//! Audit-log event stream.
//!
//! Every user-visible mutation (ledger append/approve/reject/withdraw/
//! supersede, effect_declare) and every policy decision is emitted as
//! an [`AuditEvent`] to a configurable [`AuditSink`]. Solo-dev flows
//! default to [`NullSink`]; enterprise / forensic flows enable
//! [`JsonlFileSink`] via `--audit-log <path>` (CLI) or `ASD_AUDIT_LOG`
//! env (MCP + HTTP binaries).
//!
//! The file sink is intentionally simple — append-only JSONL, one event
//! per line. This format is directly ingestible by every mainstream
//! SIEM/log aggregator (Splunk, Datadog, Loki, etc.) without any
//! transformation.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AsdError, Result};

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

/// One audit entry. Flat, JSON-serializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// `evt_<uuid-simple>` — stable id for dedup / ordering.
    pub event_id: String,
    /// One of [`event_types`], or a custom extension string.
    pub event_type: String,
    /// Primary subject (usually entry_id for ledger ops, symbol_id for
    /// effects). `None` when the op has no persistent subject (rare).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    /// Secondary subject (e.g., symbol_id when subject_id is an entry_id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary_id: Option<String>,
    /// Who initiated the action. For CLI: the value of `--author-id`
    /// or similar; for MCP: the tool's `author_id` param; for the
    /// engine itself: `"system"`.
    pub actor_id: String,
    /// `"agent" | "human" | "system"`.
    pub actor_kind: String,
    pub timestamp: DateTime<Utc>,
    /// `"success" | "awaiting-approval" | "denied" | "error" |
    /// "already-resolved" | "unauthorized"`.
    pub outcome: String,
    /// Set when a policy rule matched. Format: `<path>@<version>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_policy: Option<String>,
    /// Human-readable explanation. Used for deny reasons, error
    /// messages, approval notes, rejection reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Op-specific extras. Kept schemaless to avoid coupling.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: serde_json::Value,
}

impl AuditEvent {
    /// Construct a new event with a freshly-minted id + current timestamp.
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
        }
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
// Sink trait + implementations
// ---------------------------------------------------------------------------

pub trait AuditSink: Send + Sync {
    fn emit(&self, event: &AuditEvent) -> Result<()>;

    /// Optional bulk emit. Default implementation emits one-by-one.
    fn emit_all(&self, events: &[AuditEvent]) -> Result<()> {
        for e in events {
            self.emit(e)?;
        }
        Ok(())
    }
}

/// Swallows every event. The solo-dev default.
pub struct NullSink;

impl AuditSink for NullSink {
    fn emit(&self, _event: &AuditEvent) -> Result<()> {
        Ok(())
    }
}

/// Append-only JSONL file sink. One event per line, UTF-8 JSON.
///
/// The file is opened fresh on each `emit` call so consumers can
/// rotate/tail/truncate out-of-band without breaking the writer. At
/// solo-dev rates this is cheap; enterprise scale would swap in a
/// bulk/batched sink.
pub struct JsonlFileSink {
    path: PathBuf,
}

impl JsonlFileSink {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AuditSink for JsonlFileSink {
    fn emit(&self, event: &AuditEvent) -> Result<()> {
        let line = serde_json::to_string(event)?;
        // Create parent dirs if needed — friendlier than surfacing ENOENT.
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AsdError::Io(e))?;
            }
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

/// Read back events from a JSONL file. Used by `asd audit tail`.
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
        assert_eq!(parsed.outcome, "awaiting-approval");
        assert_eq!(parsed.matched_policy.as_deref(), Some("/policies/code/hazard@1"));
    }

    #[test]
    fn jsonl_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let sink = JsonlFileSink::new(path.clone());

        let a = AuditEvent::new(event_types::LEDGER_APPEND, "alice", "human", "success")
            .with_subject("led_1");
        let b = AuditEvent::new(event_types::LEDGER_APPROVE, "bob", "human", "success")
            .with_subject("led_1");
        sink.emit(&a).unwrap();
        sink.emit(&b).unwrap();

        let events = read_jsonl(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id, a.event_id);
        assert_eq!(events[1].event_id, b.event_id);
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
