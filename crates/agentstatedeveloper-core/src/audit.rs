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
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AsdError, Result};

/// Prefix for blake3-based event hashes. Versioned so we can change
/// the algorithm later without ambiguity.
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
    /// Hash of the previous chained event, or `None` for chain start.
    /// Enables tamper-evident verification across the whole log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_event_hash: Option<String>,
    /// Hash of THIS event's canonical bytes (with `event_hash` itself
    /// zeroed before hashing). Empty string means the event is
    /// unsigned — legacy pre-M15 events, or sinks that opt out of
    /// chaining. See [`verify_chain`].
    #[serde(default)]
    pub event_hash: String,
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
            prev_event_hash: None,
            event_hash: String::new(),
        }
    }

    /// Canonical bytes used to compute [`Self::event_hash`]. The
    /// `event_hash` field is cleared before serialization so the hash
    /// is self-consistent (recomputable without circularity).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut clone = self.clone();
        clone.event_hash = String::new();
        Ok(serde_json::to_vec(&clone)?)
    }

    /// Compute the chain hash for this event. Returns `b3:<64-hex>`.
    pub fn compute_hash(&self) -> Result<String> {
        let bytes = self.canonical_bytes()?;
        let h = blake3::hash(&bytes);
        Ok(format!("{}{}", HASH_PREFIX, h.to_hex()))
    }

    /// True iff the event carries a chain hash.
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
    /// Cache of the most recent event's hash so we can chain into it
    /// without re-reading the file on every emit. Lazily initialized
    /// from the file's tail on first emit; updated in-process from
    /// then on.
    last_hash: Mutex<Option<Option<String>>>,
}

impl JsonlFileSink {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_hash: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file (if any) and return the last event's hash, or
    /// None if the file is empty or the last event is unsigned.
    fn load_last_hash_from_disk(&self) -> Result<Option<String>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let events = read_jsonl(&self.path)?;
        Ok(events
            .iter()
            .rev()
            .find(|e| e.is_signed())
            .map(|e| e.event_hash.clone()))
    }
}

impl AuditSink for JsonlFileSink {
    fn emit(&self, event: &AuditEvent) -> Result<()> {
        // Fill chain fields on a clone — caller's event is left alone.
        let mut chained = event.clone();

        let mut guard = self
            .last_hash
            .lock()
            .map_err(|_| AsdError::Other("audit sink mutex poisoned".into()))?;

        // Lazy init: on first emit per process lifetime, scan the
        // existing file to pick up wherever the chain left off.
        if guard.is_none() {
            *guard = Some(self.load_last_hash_from_disk()?);
        }

        // Chain into the previous event (if any) and hash this one.
        chained.prev_event_hash = guard.as_ref().and_then(|h| h.clone());
        chained.event_hash = chained.compute_hash()?;

        // Create parent dirs if needed — friendlier than surfacing ENOENT.
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AsdError::Io(e))?;
            }
        }
        let line = serde_json::to_string(&chained)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;

        // Only advance the cache after a successful write.
        *guard = Some(Some(chained.event_hash));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Chain verification
// ---------------------------------------------------------------------------

/// One detected break in the chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainBreak {
    /// Zero-based position of the offending event in the input slice.
    pub index: usize,
    pub event_id: String,
    /// One of: `"event-hash-mismatch"` (payload tampered),
    /// `"prev-hash-mismatch"` (preceding event removed / reordered),
    /// `"missing-prev-hash"` (event says it chains but lacks the
    /// back-link), `"unexpected-prev-hash"` (first signed event
    /// references a predecessor that isn't present).
    pub reason: String,
}

/// Report returned by [`verify_chain`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub total_events: usize,
    pub signed_events: usize,
    pub unsigned_events: usize,
    pub chain_breaks: Vec<ChainBreak>,
    /// True iff at least one event is signed AND no breaks were
    /// detected. Unsigned-only logs are neither verified nor broken
    /// — they're legacy.
    pub verified: bool,
}

/// Walk `events` in order, recompute each signed event's canonical
/// hash, and confirm the `prev_event_hash` back-links form one
/// unbroken chain.
///
/// Unsigned events (empty `event_hash`) are allowed in the log — they
/// break the chain locally but don't count as tampering. Downstream
/// consumers can treat "unsigned gaps" as suspicious if needed.
pub fn verify_chain(events: &[AuditEvent]) -> VerifyReport {
    let mut breaks: Vec<ChainBreak> = Vec::new();
    let mut signed = 0usize;
    let mut unsigned = 0usize;
    let mut prev_signed_hash: Option<String> = None;

    for (i, ev) in events.iter().enumerate() {
        if !ev.is_signed() {
            unsigned += 1;
            // Unsigned event severs chain continuity; reset so a later
            // signed event that links back to `prev_signed_hash` is
            // flagged as attempting to skip the gap.
            prev_signed_hash = None;
            continue;
        }
        signed += 1;

        // Re-hash the event content and compare.
        match ev.compute_hash() {
            Ok(expected) if expected == ev.event_hash => {}
            Ok(_) => breaks.push(ChainBreak {
                index: i,
                event_id: ev.event_id.clone(),
                reason: "event-hash-mismatch".into(),
            }),
            Err(e) => breaks.push(ChainBreak {
                index: i,
                event_id: ev.event_id.clone(),
                reason: format!("hash-compute-error: {}", e),
            }),
        }

        // Check back-link.
        match (&prev_signed_hash, &ev.prev_event_hash) {
            (None, None) => {
                // Both none: valid chain start.
            }
            (None, Some(_)) => breaks.push(ChainBreak {
                index: i,
                event_id: ev.event_id.clone(),
                reason: "unexpected-prev-hash".into(),
            }),
            (Some(_), None) => breaks.push(ChainBreak {
                index: i,
                event_id: ev.event_id.clone(),
                reason: "missing-prev-hash".into(),
            }),
            (Some(expected), Some(actual)) if expected == actual => {
                // Valid link.
            }
            (Some(_), Some(_)) => breaks.push(ChainBreak {
                index: i,
                event_id: ev.event_id.clone(),
                reason: "prev-hash-mismatch".into(),
            }),
        }

        prev_signed_hash = Some(ev.event_hash.clone());
    }

    let verified = signed > 0 && breaks.is_empty();
    VerifyReport {
        total_events: events.len(),
        signed_events: signed,
        unsigned_events: unsigned,
        chain_breaks: breaks,
        verified,
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

    // -- Hash chain tests --

    #[test]
    fn jsonl_sink_signs_each_emit_with_linked_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let sink = JsonlFileSink::new(path.clone());

        let a = AuditEvent::new(event_types::LEDGER_APPEND, "alice", "human", "success")
            .with_subject("led_1");
        let b = AuditEvent::new(event_types::LEDGER_APPROVE, "bob", "human", "success")
            .with_subject("led_1");
        let c = AuditEvent::new(event_types::LEDGER_REJECT, "carol", "human", "rejected")
            .with_subject("led_1");
        sink.emit(&a).unwrap();
        sink.emit(&b).unwrap();
        sink.emit(&c).unwrap();

        let events = read_jsonl(&path).unwrap();
        assert_eq!(events.len(), 3);
        // All signed.
        assert!(events.iter().all(|e| e.is_signed()));
        // First has no prev.
        assert_eq!(events[0].prev_event_hash, None);
        // Chain is linked.
        assert_eq!(events[1].prev_event_hash.as_ref(), Some(&events[0].event_hash));
        assert_eq!(events[2].prev_event_hash.as_ref(), Some(&events[1].event_hash));
        // Hash prefix.
        for e in &events {
            assert!(e.event_hash.starts_with(HASH_PREFIX));
        }

        let report = verify_chain(&events);
        assert!(report.verified);
        assert_eq!(report.signed_events, 3);
        assert!(report.chain_breaks.is_empty());
    }

    #[test]
    fn verify_chain_catches_payload_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let sink = JsonlFileSink::new(path.clone());

        for i in 0..3 {
            let e = AuditEvent::new(event_types::LEDGER_APPEND, "alice", "human", "success")
                .with_subject(format!("led_{}", i));
            sink.emit(&e).unwrap();
        }

        let mut events = read_jsonl(&path).unwrap();
        // Mutate the middle event's reason — any field change should
        // produce a content mismatch.
        events[1].reason = Some("sneaky edit".into());

        let report = verify_chain(&events);
        assert!(!report.verified);
        assert!(!report.chain_breaks.is_empty());
        assert_eq!(report.chain_breaks[0].index, 1);
        assert_eq!(report.chain_breaks[0].reason, "event-hash-mismatch");
    }

    #[test]
    fn verify_chain_catches_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let sink = JsonlFileSink::new(path.clone());

        for i in 0..3 {
            let e = AuditEvent::new(event_types::LEDGER_APPEND, "alice", "human", "success")
                .with_subject(format!("led_{}", i));
            sink.emit(&e).unwrap();
        }

        let events = read_jsonl(&path).unwrap();
        // Drop the middle event.
        let tampered: Vec<AuditEvent> = vec![events[0].clone(), events[2].clone()];
        let report = verify_chain(&tampered);
        assert!(!report.verified);
        // Third (now at index 1) expected events[1]'s hash; gets events[0]'s.
        assert_eq!(report.chain_breaks[0].index, 1);
        assert_eq!(report.chain_breaks[0].reason, "prev-hash-mismatch");
    }

    #[test]
    fn verify_chain_treats_legacy_unsigned_as_unverified_but_not_broken() {
        // Simulate a log where every event is unsigned (pre-M15).
        let events: Vec<AuditEvent> = (0..2)
            .map(|i| {
                AuditEvent::new(event_types::LEDGER_APPEND, "alice", "human", "success")
                    .with_subject(format!("led_{}", i))
            })
            .collect();
        let report = verify_chain(&events);
        assert!(!report.verified);
        assert_eq!(report.signed_events, 0);
        assert_eq!(report.unsigned_events, 2);
        assert!(report.chain_breaks.is_empty());
    }

    #[test]
    fn jsonl_sink_resumes_chain_after_process_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        // Session 1 writes two events.
        {
            let sink1 = JsonlFileSink::new(path.clone());
            sink1
                .emit(&AuditEvent::new(
                    event_types::LEDGER_APPEND,
                    "alice",
                    "human",
                    "success",
                ))
                .unwrap();
            sink1
                .emit(&AuditEvent::new(
                    event_types::LEDGER_APPROVE,
                    "alice",
                    "human",
                    "success",
                ))
                .unwrap();
        }
        // Fresh sink — process "restart". Should pick up the chain
        // from disk on first emit.
        let sink2 = JsonlFileSink::new(path.clone());
        sink2
            .emit(&AuditEvent::new(
                event_types::LEDGER_REJECT,
                "bob",
                "human",
                "rejected",
            ))
            .unwrap();

        let events = read_jsonl(&path).unwrap();
        assert_eq!(events.len(), 3);
        let report = verify_chain(&events);
        assert!(
            report.verified,
            "chain should survive process restart, breaks: {:?}",
            report.chain_breaks
        );
    }
}
