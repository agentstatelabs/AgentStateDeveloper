//! Confidence for call-graph edges (Plan competitive-harvest t-013).
//!
//! Static call edges are stored as bare `symbol_id` lists with no confidence.
//! This adds a sidecar layer: each edge accumulates runtime confirmations (the
//! `asd` tracer observing the caller actually invoke the callee) on top of a
//! static prior, and a confidence is derived the same way effect confidence is
//! in t-001 — reusing [`RuntimeEvidence::derive_confidence`].
//!
//! Two provenances:
//!   - **static-known** — the edge is in the static call graph (prior
//!     [`STATIC_PRIOR`]); runtime confirmation raises it, absence never lowers
//!     it (a path simply not exercised is not evidence the edge is wrong).
//!   - **runtime-only** — the tracer observed a call the static walker missed
//!     (dynamic dispatch, reflection, callbacks). It definitely happened, so it
//!     starts at [`RUNTIME_ONLY_PRIOR`] and confirmations push it up fast. These
//!     are the high-value rows: real edges absent from the static graph.
//!
//! The sidecar lives at `/asd/v1/index/edge-evidence/<from>/<to>`, leaving the
//! hot-path `callees`/`callers` lists untouched.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::schema::RuntimeEvidence;

/// Prior confidence for an edge present in the static call graph.
pub const STATIC_PRIOR: f64 = 0.7;
/// Prior for an edge seen only at runtime (missed by static resolution).
pub const RUNTIME_ONLY_PRIOR: f64 = 0.5;

/// Accumulated evidence for one directed call edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeEvidence {
    pub from_symbol_id: String,
    pub to_symbol_id: String,
    /// Whether the static call graph contains this edge.
    pub static_known: bool,
    /// Runtime observations of the call.
    pub confirmations: u64,
    /// Frozen prior the confirmations update.
    pub prior: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at: Option<DateTime<Utc>>,
}

impl EdgeEvidence {
    /// A fresh record for a static-graph edge (no runtime evidence yet).
    pub fn static_edge(from: &str, to: &str) -> Self {
        Self {
            from_symbol_id: from.to_string(),
            to_symbol_id: to.to_string(),
            static_known: true,
            confirmations: 0,
            prior: STATIC_PRIOR,
            last_trace_id: None,
            last_observed_at: None,
        }
    }

    /// A fresh record for an edge first seen at runtime (missed by static).
    pub fn runtime_only_edge(from: &str, to: &str) -> Self {
        Self {
            from_symbol_id: from.to_string(),
            to_symbol_id: to.to_string(),
            static_known: false,
            confirmations: 0,
            prior: RUNTIME_ONLY_PRIOR,
            last_trace_id: None,
            last_observed_at: None,
        }
    }

    /// Record one runtime observation of this edge.
    pub fn confirm(&mut self, trace_id: &str, at: DateTime<Utc>) {
        self.confirmations += 1;
        self.last_trace_id = Some(trace_id.to_string());
        self.last_observed_at = Some(at);
    }

    /// If a static edge is later confirmed at runtime, mark it known (a
    /// runtime-only edge that also turns out to be static).
    pub fn mark_static_known(&mut self) {
        if !self.static_known {
            self.static_known = true;
            // Lift the prior to the static baseline if it was the lower
            // runtime-only prior, but never lower an already-higher prior.
            self.prior = self.prior.max(STATIC_PRIOR);
        }
    }

    /// Derived confidence in `[0, 1]` — the same Laplace/Beta derivation as
    /// effect confidence (edges only ever accumulate confirmations, never
    /// contradictions, since absence is not evidence against an edge).
    pub fn confidence(&self) -> f64 {
        RuntimeEvidence::derive_confidence(self.prior, self.confirmations, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-29T00:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn fresh_static_edge_sits_at_static_prior() {
        let e = EdgeEvidence::static_edge("a", "b");
        assert!(e.static_known);
        assert!((e.confidence() - STATIC_PRIOR).abs() < 1e-9);
    }

    #[test]
    fn runtime_only_edge_starts_lower_and_is_flagged() {
        let e = EdgeEvidence::runtime_only_edge("a", "b");
        assert!(!e.static_known);
        assert!((e.confidence() - RUNTIME_ONLY_PRIOR).abs() < 1e-9);
    }

    #[test]
    fn confirmations_raise_confidence_monotonically() {
        let mut e = EdgeEvidence::static_edge("a", "b");
        let base = e.confidence();
        e.confirm("trc_1", ts());
        let one = e.confidence();
        e.confirm("trc_2", ts());
        let two = e.confidence();
        assert!(one > base && two > one, "{base} {one} {two}");
        assert_eq!(e.confirmations, 2);
        assert_eq!(e.last_trace_id.as_deref(), Some("trc_2"));
    }

    #[test]
    fn confidence_never_exceeds_one() {
        let mut e = EdgeEvidence::runtime_only_edge("a", "b");
        for _ in 0..10_000 {
            e.confirm("t", ts());
        }
        assert!(e.confidence() < 1.0 && e.confidence() > 0.99);
    }

    #[test]
    fn mark_static_known_lifts_prior_without_lowering() {
        let mut e = EdgeEvidence::runtime_only_edge("a", "b");
        e.mark_static_known();
        assert!(e.static_known);
        assert!((e.prior - STATIC_PRIOR).abs() < 1e-9);
        // Idempotent and never lowers a higher prior.
        let mut high = EdgeEvidence::static_edge("a", "b");
        high.prior = 0.9;
        high.mark_static_known();
        assert!((high.prior - 0.9).abs() < 1e-9);
    }
}
