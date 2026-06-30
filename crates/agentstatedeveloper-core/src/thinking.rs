//! Plan G t-006: read-side helper for surfacing captured thinking
//! (Hypothesis / MentalModel / FailedAttempt / OpenQuestion) into
//! prepare-change and context-for responses.
//!
//! ExampleFlow refinement (2026-06-04): the helper now returns a
//! struct (`PriorThinking`) carrying BOTH the projected entries and
//! a `ThinkingSummary` metadata block. The summary always emits even
//! when no entries surface, so callers can distinguish:
//!
//!   - "thinking exists on the queried symbols but was filtered out"
//!     (e.g. all hypotheses below the confidence floor): `surfaced == 0`
//!     AND `by_kind_dropped` populated → action: lower the floor or
//!     run `asd think list <qname>` to inspect.
//!
//!   - "no thinking entries exist for the queried symbols": all-zero
//!     summary → action: maybe capture some via `asd think speculate/
//!     model/question/failed`.
//!
//!   - "thinking exists somewhere in the workspace but not on these
//!     symbols": `matched_for_query == 0` AND `entries_in_workspace > 0`
//!     → action: broaden the query, or this symbol cluster is unmapped.
//!
//! `entries_in_workspace` is the only field that requires a workspace-
//! wide scan; per the ExampleFlow design call (Craig, 2026-06-04) it's
//! computed ONLY when `matched_for_query == 0` so the typical hot-path
//! call pays nothing extra.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::LedgerKind;

/// Hypotheses with confidence below this are excluded from auto-surface
/// (still queryable via `asd think list`). Plan G t-001 picked 0.3 as
/// the default; callers may override.
pub const DEFAULT_CONFIDENCE_FLOOR: f64 = 0.3;

/// Result of `gather_prior_thinking` — entries projected to JSON +
/// metadata summary explaining what was scanned, what surfaced, and
/// what was filtered.
#[derive(Debug, Clone, Serialize)]
pub struct PriorThinking {
    /// The projected entries object (hypotheses/mental_models/
    /// open_questions/failed_attempts arrays). `Value::Null` when
    /// nothing surfaced — distinct from `{}`, lets callers omit the
    /// field cleanly. Use `summary.surfaced > 0` as the boolean check.
    pub entries: Value,
    /// Always-emitted metadata. Even when `entries` is Null, the
    /// summary tells the agent WHY (see module docs).
    pub summary: ThinkingSummary,
}

/// Metadata accompanying the prior_thinking projection. Parallel in
/// shape to `FeedbackMetrics` — same idea, different domain.
///
/// Token economy (1.0.78): every count field that's zero is skipped
/// during serialization, and the all-zero `by_kind` / `by_kind_dropped`
/// maps are skipped entirely. On a query where no thinking exists, the
/// serialized form collapses to `{"scanned_qnames": N}` (or empty if
/// even scanned_qnames is 0), saving ~200 chars/call. Non-zero counts
/// still emit — accuracy preserved.
#[derive(Debug, Clone, Serialize)]
pub struct ThinkingSummary {
    /// Number of qnames the caller passed in for scanning.
    /// Skipped when zero (no qnames provided → nothing to summarize).
    #[serde(skip_serializing_if = "crate::ser_helpers::is_zero_usize")]
    pub scanned_qnames: usize,
    /// Qnames that had at least one thinking entry (any kind).
    /// Skipped when zero; agent infers "no match" from absence.
    #[serde(skip_serializing_if = "crate::ser_helpers::is_zero_usize")]
    pub matched_for_query: usize,
    /// Entries kept after kind-filter + confidence-floor cuts.
    /// Skipped when zero; agent infers "no entries surfaced" from
    /// absence (and from `prior_thinking` being null/absent).
    #[serde(skip_serializing_if = "crate::ser_helpers::is_zero_usize")]
    pub surfaced: usize,
    /// Surfaced counts broken out by kind. Skipped entirely when
    /// every kind is zero. When emitted, ALL kinds appear (so the
    /// agent doesn't have to defensive-check) — this is the
    /// established `feedback_summary` pattern.
    #[serde(skip_serializing_if = "crate::ser_helpers::is_all_zero_string_usize_map")]
    pub by_kind: BTreeMap<String, usize>,
    /// Entries the kind/confidence filters dropped, broken out by kind.
    /// The load-bearing field for the "thinking exists but isn't
    /// showing" case — when `by_kind_dropped["hypothesis"] > 0` AND
    /// `by_kind["hypothesis"] == 0`, the agent knows hypotheses exist
    /// for these symbols but all fell below the confidence floor.
    /// Skipped when all-zero (most calls).
    #[serde(skip_serializing_if = "crate::ser_helpers::is_all_zero_string_usize_map")]
    pub by_kind_dropped: BTreeMap<String, usize>,
    /// Workspace-wide thinking count (across ALL indexed symbols).
    /// Only populated when `matched_for_query == 0` — gives the agent
    /// a signal whether to broaden the query (workspace has entries
    /// elsewhere) or capture new ones (workspace is genuinely empty).
    /// `None` otherwise to keep the typical hot path cheap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries_in_workspace: Option<usize>,
}

impl ThinkingSummary {
    /// True when the summary carries no signal — useful for the
    /// caller's "emit at all?" decision per the sentinel rule
    /// (emit only when prior_thinking is null AND scanned_qnames > 0).
    pub fn is_quiet(&self) -> bool {
        self.scanned_qnames == 0
            && self.matched_for_query == 0
            && self.surfaced == 0
            && self.by_kind.values().all(|v| *v == 0)
            && self.by_kind_dropped.values().all(|v| *v == 0)
            && self.entries_in_workspace.is_none()
    }
}

impl ThinkingSummary {
    fn empty(scanned_qnames: usize) -> Self {
        Self {
            scanned_qnames,
            matched_for_query: 0,
            surfaced: 0,
            by_kind: kind_counter_zero(),
            by_kind_dropped: kind_counter_zero(),
            entries_in_workspace: None,
        }
    }
}

fn kind_counter_zero() -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    m.insert("hypothesis".into(), 0);
    m.insert("mental_model".into(), 0);
    m.insert("open_question".into(), 0);
    m.insert("failed_attempt".into(), 0);
    m
}

/// Walk the given qnames, collect Plan G thinking entries, project to
/// the compact `prior_thinking` JSON shape AND a metadata summary.
/// Hypotheses below `min_confidence` are dropped (and counted into
/// `summary.by_kind_dropped`).
///
/// `entries` is `Value::Null` when nothing surfaces; `summary` is
/// always populated.
pub fn gather_prior_thinking(
    engine: &Engine,
    qnames: &[String],
    min_confidence: f64,
) -> PriorThinking {
    let index = AsgIndexStore::from_engine(engine);
    let ledger = AsgLedgerStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    let mut hypotheses: Vec<Value> = Vec::new();
    let mut mental_models: Vec<Value> = Vec::new();
    let mut open_questions: Vec<Value> = Vec::new();
    let mut failed_attempts: Vec<Value> = Vec::new();

    let mut by_kind = kind_counter_zero();
    let mut by_kind_dropped = kind_counter_zero();
    let mut matched_for_query = 0usize;

    for qn in qnames {
        let sym = match index.get_symbol_by_qname(&ref_name, qn) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let entries = ledger
            .list_entries(&ref_name, &sym.symbol_id)
            .unwrap_or_default();
        let mut had_thinking_entry = false;
        for entry in entries {
            match entry.kind {
                LedgerKind::Hypothesis => {
                    had_thinking_entry = true;
                    let conf = entry.confidence.unwrap_or(0.0);
                    if conf < min_confidence {
                        *by_kind_dropped.get_mut("hypothesis").unwrap() += 1;
                        continue;
                    }
                    hypotheses.push(json!({
                        "qname": qn,
                        "summary": entry.summary,
                        "confidence": conf,
                    }));
                    *by_kind.get_mut("hypothesis").unwrap() += 1;
                }
                LedgerKind::MentalModel => {
                    had_thinking_entry = true;
                    let body_v: Option<Value> = entry
                        .body
                        .as_deref()
                        .and_then(|b| serde_json::from_str(b).ok());
                    let symbols: Vec<String> = body_v
                        .as_ref()
                        .and_then(|v| v.get("symbols"))
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let name = body_v
                        .as_ref()
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    mental_models.push(json!({
                        "qname": qn,
                        "name": name,
                        "summary": entry.summary,
                        "symbols": symbols,
                    }));
                    *by_kind.get_mut("mental_model").unwrap() += 1;
                }
                LedgerKind::OpenQuestion => {
                    had_thinking_entry = true;
                    open_questions.push(json!({
                        "qname": qn,
                        "question": entry.summary,
                    }));
                    *by_kind.get_mut("open_question").unwrap() += 1;
                }
                LedgerKind::FailedAttempt => {
                    had_thinking_entry = true;
                    let body_v: Option<Value> = entry
                        .body
                        .as_deref()
                        .and_then(|b| serde_json::from_str(b).ok());
                    let tried = body_v
                        .as_ref()
                        .and_then(|v| v.get("tried"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let because = body_v
                        .as_ref()
                        .and_then(|v| v.get("because"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    failed_attempts.push(json!({
                        "qname": qn,
                        "summary": entry.summary,
                        "tried": tried,
                        "because": because,
                    }));
                    *by_kind.get_mut("failed_attempt").unwrap() += 1;
                }
                _ => {}
            }
        }
        if had_thinking_entry {
            matched_for_query += 1;
        }
    }

    let surfaced = by_kind.values().sum::<usize>();

    // Lazy workspace count: only walk when nothing matched the query —
    // that's when the agent might want to know "is there ANY thinking
    // in this workspace, or should I capture some?"
    let entries_in_workspace = if matched_for_query == 0 {
        Some(count_workspace_thinking(engine))
    } else {
        None
    };

    let summary = ThinkingSummary {
        scanned_qnames: qnames.len(),
        matched_for_query,
        surfaced,
        by_kind,
        by_kind_dropped,
        entries_in_workspace,
    };

    let entries = if surfaced == 0 {
        Value::Null
    } else {
        let mut out = serde_json::Map::new();
        if !hypotheses.is_empty() {
            out.insert("hypotheses".into(), json!(hypotheses));
        }
        if !mental_models.is_empty() {
            out.insert("mental_models".into(), json!(mental_models));
        }
        if !open_questions.is_empty() {
            out.insert("open_questions".into(), json!(open_questions));
        }
        if !failed_attempts.is_empty() {
            out.insert("failed_attempts".into(), json!(failed_attempts));
        }
        Value::Object(out)
    };

    PriorThinking { entries, summary }
}

/// Workspace-wide count of thinking entries across all indexed symbols.
/// Walks the qname tree once and counts ledger entries of the four
/// thinking kinds. Used as a fallback signal when prior_thinking finds
/// nothing on the queried symbols.
///
/// Cost: O(symbols) — on a 10k-symbol repo this is ~150-300ms typical.
/// Only called when `matched_for_query == 0` (see gather_prior_thinking).
pub fn count_workspace_thinking(engine: &Engine) -> usize {
    let ledger = AsgLedgerStore::from_engine(engine);
    let prefix = format!("{}/index/by-qname", crate::paths::ASD_ROOT);
    let qnames: Vec<String> = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(Value::Object(map)) => map.keys().cloned().collect(),
        _ => return 0,
    };
    let index = AsgIndexStore::from_engine(engine);
    let mut total = 0usize;
    for qn in &qnames {
        let sym = match index.get_symbol_by_qname(&engine.ref_name, qn) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let entries = ledger
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();
        total += entries
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    LedgerKind::Hypothesis
                        | LedgerKind::MentalModel
                        | LedgerKind::OpenQuestion
                        | LedgerKind::FailedAttempt
                )
            })
            .count();
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Author, AuthorKind, LedgerEntry, Position, Symbol, SymbolKind};

    fn seed() -> (Engine, String) {
        let engine = Engine::open_in_memory().unwrap();
        let index = AsgIndexStore::from_engine(&engine);
        let sym = Symbol {
            symbol_id: "sym_x".into(),
            symbol_fp: "fp_x".into(),
            qname: "pkg.target".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/target.py".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym, "test").unwrap();
        (engine, "pkg.target".into())
    }

    fn append(
        engine: &Engine,
        sym_id: &str,
        kind: LedgerKind,
        summary: &str,
        conf: Option<f64>,
        body: Option<&str>,
    ) {
        let ledger = AsgLedgerStore::from_engine(engine);
        let mut entry = LedgerEntry::new(
            sym_id,
            kind,
            summary,
            Author {
                kind: AuthorKind::Agent,
                id: "t".into(),
            },
        );
        entry.confidence = conf;
        entry.body = body.map(str::to_string);
        ledger.append_entry(&engine.ref_name, &entry, "t").unwrap();
    }

    #[test]
    fn returns_null_entries_when_no_thinking() {
        let (engine, qn) = seed();
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        assert_eq!(pt.entries, Value::Null);
        assert_eq!(pt.summary.scanned_qnames, 1);
        assert_eq!(pt.summary.matched_for_query, 0);
        assert_eq!(pt.summary.surfaced, 0);
        // matched_for_query == 0 → entries_in_workspace populated
        assert_eq!(
            pt.summary.entries_in_workspace,
            Some(0),
            "workspace count must be populated when query matched nothing"
        );
    }

    #[test]
    fn surfaces_high_confidence_hypothesis() {
        let (engine, qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::Hypothesis,
            "X causes Y",
            Some(0.7),
            None,
        );
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let o = pt.entries.as_object().unwrap();
        let hyps = o["hypotheses"].as_array().unwrap();
        assert_eq!(hyps.len(), 1);
        assert_eq!(hyps[0]["confidence"].as_f64(), Some(0.7));
        assert_eq!(pt.summary.by_kind["hypothesis"], 1);
        assert_eq!(pt.summary.surfaced, 1);
        assert_eq!(pt.summary.matched_for_query, 1);
        // matched > 0 → skip workspace count
        assert_eq!(pt.summary.entries_in_workspace, None);
    }

    #[test]
    fn excludes_below_confidence_floor_and_records_dropped() {
        let (engine, qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::Hypothesis,
            "weak guess",
            Some(0.1),
            None,
        );
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        assert_eq!(pt.entries, Value::Null);
        assert_eq!(pt.summary.surfaced, 0);
        // The load-bearing signal: dropped > 0 even though surfaced == 0
        assert_eq!(
            pt.summary.by_kind_dropped["hypothesis"], 1,
            "filtered-out entries must increment by_kind_dropped"
        );
        // matched_for_query counts qnames that HAD an entry, even
        // if dropped — the entry existed, it was filtered.
        assert_eq!(
            pt.summary.matched_for_query, 1,
            "qname had a thinking entry (even if dropped) → matched"
        );
    }

    #[test]
    fn surfaces_mental_model_with_symbols_array() {
        let (engine, qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::MentalModel,
            "audio-pipeline: input → mix → out",
            None,
            Some(r#"{"symbols":["a.b","c.d"],"name":"audio-pipeline"}"#),
        );
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let mm = pt.entries["mental_models"].as_array().unwrap();
        assert_eq!(mm.len(), 1);
        assert_eq!(mm[0]["name"].as_str(), Some("audio-pipeline"));
        assert_eq!(pt.summary.by_kind["mental_model"], 1);
    }

    #[test]
    fn surfaces_failed_attempt_with_tried_because() {
        let (engine, qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::FailedAttempt,
            "failed: caching — broke under reload",
            None,
            Some(r#"{"tried":"caching","because":"broke under reload"}"#),
        );
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let fa = pt.entries["failed_attempts"].as_array().unwrap();
        assert_eq!(fa[0]["tried"].as_str(), Some("caching"));
        assert_eq!(fa[0]["because"].as_str(), Some("broke under reload"));
        assert_eq!(pt.summary.by_kind["failed_attempt"], 1);
    }

    #[test]
    fn surfaces_open_question() {
        let (engine, qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::OpenQuestion,
            "what does 4096 mean?",
            None,
            None,
        );
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let oq = pt.entries["open_questions"].as_array().unwrap();
        assert_eq!(oq[0]["question"].as_str(), Some("what does 4096 mean?"));
        assert_eq!(pt.summary.by_kind["open_question"], 1);
    }

    #[test]
    fn excludes_non_thinking_kinds() {
        let (engine, qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::Decision,
            "decided X",
            None,
            None,
        );
        append(
            &engine,
            "sym_x",
            LedgerKind::Constraint,
            "must Y",
            None,
            None,
        );
        append(
            &engine,
            "sym_x",
            LedgerKind::Mapping,
            "covers Z",
            None,
            None,
        );
        let pt = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        assert_eq!(pt.entries, Value::Null);
        // Non-thinking kinds don't even register as a match
        assert_eq!(pt.summary.matched_for_query, 0);
    }

    #[test]
    fn workspace_count_lazy_only_when_query_matched_nothing() {
        // Seed a hypothesis on a DIFFERENT symbol than we query.
        // matched_for_query stays 0, so entries_in_workspace must
        // be populated AND > 0.
        let (engine, _qn) = seed();
        append(
            &engine,
            "sym_x",
            LedgerKind::Hypothesis,
            "ws hyp",
            Some(0.8),
            None,
        );
        // Query a non-existent qname.
        let pt = gather_prior_thinking(
            &engine,
            &["nonexistent.qname".into()],
            DEFAULT_CONFIDENCE_FLOOR,
        );
        assert_eq!(pt.summary.matched_for_query, 0);
        assert_eq!(
            pt.summary.entries_in_workspace,
            Some(1),
            "workspace count must find the orphan hypothesis"
        );
    }
}
