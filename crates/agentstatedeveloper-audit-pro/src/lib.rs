//! `agentstatedeveloper-audit-pro` — Enterprise-tier audit sink.
//!
//! Provides [`JsonlFileSink`]: an append-only JSONL file sink that
//! hash-chains every event with blake3 for tamper-evident verification.
//! Also provides [`verify_chain`] and the supporting [`VerifyReport`] /
//! [`ChainBreak`] types.
//!
//! `asd-pro` installs a `JsonlFileSink` at startup when
//! `--audit-log` / `ASD_AUDIT_LOG` is configured. The OSS `asd` binary
//! ships only `NullSink` — it can read logs written by this sink via the
//! shared `read_jsonl` in `agentstatedeveloper-core`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentstatedeveloper_core::error::Result;
use agentstatedeveloper_core::read_jsonl;
use agentstatedeveloper_core::{AsdError, AuditEvent, AuditSink, HASH_PREFIX};

// ---------------------------------------------------------------------------
// Hash helpers (moved out of OSS AuditEvent — live here to keep blake3 dep
// off the OSS binary's critical path)
// ---------------------------------------------------------------------------

/// Serialize `event` with `event_hash` zeroed so the hash is self-consistent.
pub fn canonical_bytes(event: &AuditEvent) -> Result<Vec<u8>> {
    let mut clone = event.clone();
    clone.event_hash = String::new();
    Ok(serde_json::to_vec(&clone)?)
}

/// Compute `b3:<64-hex>` for `event`. The `event_hash` field is cleared
/// before hashing (see [`canonical_bytes`]).
pub fn compute_hash(event: &AuditEvent) -> Result<String> {
    let bytes = canonical_bytes(event)?;
    let h = blake3::hash(&bytes);
    Ok(format!("{}{}", HASH_PREFIX, h.to_hex()))
}

// ---------------------------------------------------------------------------
// JsonlFileSink
// ---------------------------------------------------------------------------

/// Append-only JSONL sink with blake3 hash-chaining.
///
/// Each emitted event gets:
/// - `prev_event_hash` — the previous signed event's `event_hash` (or `None`
///   at chain start)
/// - `event_hash` — `b3:<hex>` of the canonical serialization (with
///   `event_hash` itself zeroed before hashing)
///
/// The in-process `last_hash` cache is lazily seeded from the tail of the
/// file on first emit, so the chain survives process restarts.
pub struct JsonlFileSink {
    path: PathBuf,
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
        let mut chained = event.clone();

        let mut guard = self
            .last_hash
            .lock()
            .map_err(|_| AsdError::Other("audit sink mutex poisoned".into()))?;

        if guard.is_none() {
            *guard = Some(self.load_last_hash_from_disk()?);
        }

        chained.prev_event_hash = guard.as_ref().and_then(|h| h.clone());
        chained.event_hash = compute_hash(&chained)?;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(AsdError::Io)?;
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(AsdError::Io)?;

        let mut line = serde_json::to_string(&chained)?;
        line.push('\n');
        file.write_all(line.as_bytes()).map_err(AsdError::Io)?;

        *guard = Some(Some(chained.event_hash.clone()));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Chain verification
// ---------------------------------------------------------------------------

/// A single detected break in the event chain.
#[derive(Debug, Clone)]
pub struct ChainBreak {
    /// Zero-based index in the events slice.
    pub index: usize,
    pub event_id: String,
    /// Short machine-readable reason:
    /// `event-hash-mismatch | missing-prev-hash | unexpected-prev-hash | prev-hash-mismatch`
    pub reason: String,
}

/// Summary produced by [`verify_chain`].
#[derive(Debug)]
pub struct VerifyReport {
    pub total_events: usize,
    pub signed_events: usize,
    pub unsigned_events: usize,
    pub chain_breaks: Vec<ChainBreak>,
    /// True iff at least one signed event exists and no breaks were detected.
    /// An unsigned-only log is neither verified nor broken — it's legacy.
    pub verified: bool,
}

/// Walk `events` in order, recompute each signed event's hash, and confirm
/// `prev_event_hash` back-links form one unbroken chain.
///
/// Unsigned events (empty `event_hash`) are skipped but reset the expected
/// prev-link, so a signed event that tries to link across an unsigned gap is
/// flagged.
pub fn verify_chain(events: &[AuditEvent]) -> VerifyReport {
    let mut breaks: Vec<ChainBreak> = Vec::new();
    let mut signed = 0usize;
    let mut unsigned = 0usize;
    let mut prev_signed_hash: Option<String> = None;

    for (i, ev) in events.iter().enumerate() {
        if !ev.is_signed() {
            unsigned += 1;
            prev_signed_hash = None;
            continue;
        }
        signed += 1;

        match compute_hash(ev) {
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

        match (&prev_signed_hash, &ev.prev_event_hash) {
            (None, None) => {}
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
            (Some(expected), Some(actual)) if expected == actual => {}
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentstatedeveloper_core::event_types;

    #[test]
    fn sink_writes_chained_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let sink = JsonlFileSink::new(path.clone());

        for i in 0..3 {
            let e = AuditEvent::new(event_types::LEDGER_APPEND, "alice", "human", "success")
                .with_subject(format!("led_{}", i));
            sink.emit(&e).unwrap();
        }

        let events = read_jsonl(&path).unwrap();
        assert_eq!(events.len(), 3);
        let report = verify_chain(&events);
        assert!(report.verified, "breaks: {:?}", report.chain_breaks);
        assert_eq!(report.signed_events, 3);
        assert_eq!(report.unsigned_events, 0);
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
        events[1].reason = Some("sneaky edit".into());

        let report = verify_chain(&events);
        assert!(!report.verified);
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
        let tampered = vec![events[0].clone(), events[2].clone()];
        let report = verify_chain(&tampered);
        assert!(!report.verified);
        assert_eq!(report.chain_breaks[0].index, 1);
        assert_eq!(report.chain_breaks[0].reason, "prev-hash-mismatch");
    }

    #[test]
    fn verify_chain_unsigned_only_is_not_broken() {
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
    fn sink_resumes_chain_after_process_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

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
