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
use crate::schema::{LedgerEntry, LedgerKind};

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
/// Shared accumulator for the two gather entry points — field-for-field
/// the aggregates the original inline loop built.
struct GatherAcc {
    hypotheses: Vec<Value>,
    mental_models: Vec<Value>,
    open_questions: Vec<Value>,
    failed_attempts: Vec<Value>,
    by_kind: BTreeMap<String, usize>,
    by_kind_dropped: BTreeMap<String, usize>,
    matched_for_query: usize,
}

impl GatherAcc {
    fn new() -> Self {
        Self {
            hypotheses: Vec::new(),
            mental_models: Vec::new(),
            open_questions: Vec::new(),
            failed_attempts: Vec::new(),
            by_kind: kind_counter_zero(),
            by_kind_dropped: kind_counter_zero(),
            matched_for_query: 0,
        }
    }

    /// Classify one symbol's live ledger entries into the projection.
    fn accumulate(&mut self, qn: &str, entries: &[LedgerEntry], min_confidence: f64) {
        let mut had_thinking_entry = false;
        for entry in entries {
            match entry.kind {
                LedgerKind::Hypothesis => {
                    had_thinking_entry = true;
                    let conf = entry.confidence.unwrap_or(0.0);
                    if conf < min_confidence {
                        *self.by_kind_dropped.get_mut("hypothesis").unwrap() += 1;
                        continue;
                    }
                    self.hypotheses.push(json!({
                        "qname": qn,
                        "summary": entry.summary,
                        "confidence": conf,
                    }));
                    *self.by_kind.get_mut("hypothesis").unwrap() += 1;
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
                    self.mental_models.push(json!({
                        "qname": qn,
                        "name": name,
                        "summary": entry.summary,
                        "symbols": symbols,
                    }));
                    *self.by_kind.get_mut("mental_model").unwrap() += 1;
                }
                LedgerKind::OpenQuestion => {
                    had_thinking_entry = true;
                    self.open_questions.push(json!({
                        "qname": qn,
                        "question": entry.summary,
                    }));
                    *self.by_kind.get_mut("open_question").unwrap() += 1;
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
                    self.failed_attempts.push(json!({
                        "qname": qn,
                        "summary": entry.summary,
                        "tried": tried,
                        "because": because,
                    }));
                    *self.by_kind.get_mut("failed_attempt").unwrap() += 1;
                }
                _ => {}
            }
        }
        if had_thinking_entry {
            self.matched_for_query += 1;
        }
    }

    fn finish(self, engine: &Engine, scanned_qnames: usize) -> PriorThinking {
        let surfaced = self.by_kind.values().sum::<usize>();

        // Lazy workspace count: only walk when nothing matched the query —
        // that's when the agent might want to know "is there ANY thinking
        // in this workspace, or should I capture some?"
        let entries_in_workspace = if self.matched_for_query == 0 {
            Some(count_workspace_thinking(engine))
        } else {
            None
        };

        let summary = ThinkingSummary {
            scanned_qnames,
            matched_for_query: self.matched_for_query,
            surfaced,
            by_kind: self.by_kind,
            by_kind_dropped: self.by_kind_dropped,
            entries_in_workspace,
        };

        let entries = if surfaced == 0 {
            Value::Null
        } else {
            let mut out = serde_json::Map::new();
            if !self.hypotheses.is_empty() {
                out.insert("hypotheses".into(), json!(self.hypotheses));
            }
            if !self.mental_models.is_empty() {
                out.insert("mental_models".into(), json!(self.mental_models));
            }
            if !self.open_questions.is_empty() {
                out.insert("open_questions".into(), json!(self.open_questions));
            }
            if !self.failed_attempts.is_empty() {
                out.insert("failed_attempts".into(), json!(self.failed_attempts));
            }
            Value::Object(out)
        };

        PriorThinking { entries, summary }
    }
}

pub fn gather_prior_thinking(
    engine: &Engine,
    qnames: &[String],
    min_confidence: f64,
) -> PriorThinking {
    let index = AsgIndexStore::from_engine(engine);
    let ledger = AsgLedgerStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    let mut acc = GatherAcc::new();
    for qn in qnames {
        let sym = match index.get_symbol_by_qname(&ref_name, qn) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let entries = ledger
            .list_entries(&ref_name, &sym.symbol_id)
            .unwrap_or_default();
        acc.accumulate(qn, &entries, min_confidence);
    }
    acc.finish(engine, qnames.len())
}

/// Workspace-wide variant: ONE ledger-tree walk + ONE id-map build instead
/// of a per-qname symbol lookup + per-symbol ledger read. Same output shape
/// and filtering rules as calling [`gather_prior_thinking`] with every
/// indexed qname, but O(entries) instead of O(symbols × tree-depth) — the
/// per-qname form measured ~9 minutes on a 9.6k-symbol repo with a cold
/// FTS cache (Plan T t-007 finding); this walk is milliseconds.
pub fn gather_prior_thinking_all(engine: &Engine, min_confidence: f64) -> PriorThinking {
    let index = AsgIndexStore::from_engine(engine);
    let id_map = index.build_id_map(engine);

    let mut acc = GatherAcc::new();
    for (symbol_id, entries) in all_ledger_buckets(engine) {
        // Unindexed symbols are skipped, matching the per-qname path
        // (which can only reach entries through an indexed qname).
        let Some(sym) = id_map.get(&symbol_id) else {
            continue;
        };
        let live = live_entries(entries);
        acc.accumulate(&sym.qname, &live, min_confidence);
    }
    acc.finish(engine, id_map.len())
}

/// One-pass read of the whole ledger tree, grouped per symbol bucket.
///
/// Bulk analog of calling `LedgerStore::list_entries_with_superseded`
/// for every symbol: ONE tree walk instead of one read per symbol
/// (~54ms each on a cold FTS cache — minutes at 10k symbols while
/// holding the engine mutex; Plan T scale finding). Entries are NOT
/// supersede-filtered — pass each bucket through [`live_entries`].
pub fn all_ledger_buckets(engine: &Engine) -> Vec<(String, Vec<LedgerEntry>)> {
    let prefix = format!("{}/ledger", crate::paths::ASD_ROOT);
    let tree = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut buckets = Vec::new();
    if let Value::Object(by_symbol) = tree {
        for (symbol_id, symbol_bucket) in by_symbol {
            if let Value::Object(entry_map) = symbol_bucket {
                let entries: Vec<LedgerEntry> = entry_map
                    .into_iter()
                    .filter_map(|(_, v)| serde_json::from_value(v).ok())
                    .collect();
                if !entries.is_empty() {
                    buckets.push((symbol_id, entries));
                }
            }
        }
    }
    buckets
}

/// Per-bucket supersede filtering — the same rule as
/// `LedgerStore::list_entries` (supersession never crosses symbols, so
/// applying it per bucket is equivalent).
pub fn live_entries(all: Vec<LedgerEntry>) -> Vec<LedgerEntry> {
    let superseded: std::collections::HashSet<String> = all
        .iter()
        .flat_map(|e| e.supersedes.iter().cloned())
        .collect();
    all.into_iter()
        .filter(|e| !superseded.contains(&e.entry_id))
        .collect()
}

/// Bulk analog of the "list every indexed symbol's ledger entries"
/// loop (per-qname `get_symbol_by_qname` + per-symbol `list_entries`,
/// which is minutes at 10k symbols on a cold FTS cache): ONE id-map
/// build + ONE ledger-tree walk.
///
/// Returns `(qname, live entries)` per indexed symbol, sorted by qname
/// with entries newest-first — the same visit order and per-symbol
/// entry order as the per-qname loop it replaces. Symbols without
/// ledger entries and ledger buckets for unindexed symbols are both
/// skipped, matching the per-qname path.
pub fn all_symbol_entries(engine: &Engine) -> Vec<(String, Vec<LedgerEntry>)> {
    let index = AsgIndexStore::from_engine(engine);
    let id_map = index.build_id_map(engine);

    let mut out: Vec<(String, Vec<LedgerEntry>)> = Vec::new();
    for (symbol_id, entries) in all_ledger_buckets(engine) {
        let Some(sym) = id_map.get(&symbol_id) else {
            continue;
        };
        let mut live = live_entries(entries);
        if live.is_empty() {
            continue;
        }
        // Newest first — same ordering contract as `list_entries`.
        live.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out.push((sym.qname.clone(), live));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Workspace-wide count of thinking entries across all symbols.
/// Used as a fallback signal when prior_thinking finds nothing on the
/// queried symbols.
///
/// Single ledger-tree walk. (Was: a per-qname symbol lookup plus a
/// per-symbol ledger read — quadratic on big repos, and this function
/// runs exactly when nothing matched, i.e. on the workspace-wide
/// slow path. Plan T t-007 finding.)
pub fn count_workspace_thinking(engine: &Engine) -> usize {
    all_ledger_buckets(engine)
        .into_iter()
        .flat_map(|(_, entries)| live_entries(entries))
        .filter(|e| {
            matches!(
                e.kind,
                LedgerKind::Hypothesis
                    | LedgerKind::MentalModel
                    | LedgerKind::OpenQuestion
                    | LedgerKind::FailedAttempt
            )
        })
        .count()
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
    fn bulk_gather_matches_per_qname_gather() {
        // gather_prior_thinking_all must be output-identical to the
        // per-qname form fed every indexed qname — it exists only as the
        // O(entries) fast path (Plan T t-007 quadratic-listing finding).
        let (engine, qn) = seed();
        let index = AsgIndexStore::from_engine(&engine);
        let sym2 = Symbol {
            symbol_id: "sym_y".into(),
            symbol_fp: "fp_y".into(),
            qname: "pkg.other".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/other.py".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 3, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym2, "test").unwrap();

        append(
            &engine,
            "sym_x",
            LedgerKind::Hypothesis,
            "likely rc",
            Some(0.8),
            None,
        );
        append(
            &engine,
            "sym_x",
            LedgerKind::Hypothesis,
            "below floor",
            Some(0.1),
            None,
        );
        append(
            &engine,
            "sym_y",
            LedgerKind::FailedAttempt,
            "tried caching",
            None,
            Some(r#"{"tried":"lru","because":"stale reads"}"#),
        );
        append(
            &engine,
            "sym_y",
            LedgerKind::OpenQuestion,
            "why flaky?",
            None,
            None,
        );
        // Non-thinking entries must not surface in either form.
        append(
            &engine,
            "sym_x",
            LedgerKind::Decision,
            "use sqlite",
            None,
            None,
        );

        let per_qname = gather_prior_thinking(
            &engine,
            &[qn, "pkg.other".to_string()],
            DEFAULT_CONFIDENCE_FLOOR,
        );
        let bulk = gather_prior_thinking_all(&engine, DEFAULT_CONFIDENCE_FLOOR);

        assert_eq!(bulk.summary.scanned_qnames, 2);
        assert_eq!(
            bulk.summary.matched_for_query,
            per_qname.summary.matched_for_query
        );
        assert_eq!(bulk.summary.surfaced, per_qname.summary.surfaced);
        assert_eq!(bulk.summary.by_kind, per_qname.summary.by_kind);
        assert_eq!(
            bulk.summary.by_kind_dropped,
            per_qname.summary.by_kind_dropped
        );

        // Entry arrays must match as sets (bucket iteration order may
        // differ from the caller's qname order).
        for key in [
            "hypotheses",
            "mental_models",
            "open_questions",
            "failed_attempts",
        ] {
            let mut a: Vec<String> = per_qname.entries[key]
                .as_array()
                .map(|v| v.iter().map(|e| e.to_string()).collect())
                .unwrap_or_default();
            let mut b: Vec<String> = bulk.entries[key]
                .as_array()
                .map(|v| v.iter().map(|e| e.to_string()).collect())
                .unwrap_or_default();
            a.sort();
            b.sort();
            assert_eq!(a, b, "mismatch in {key}");
        }
    }

    #[test]
    fn all_symbol_entries_matches_per_qname_listing() {
        // all_symbol_entries must be output-identical to the per-qname
        // loop it replaces (get_symbol_by_qname + list_entries per
        // symbol) — same qname order, same newest-first entries, same
        // supersede filtering, symbols without entries skipped.
        let (engine, qn) = seed();
        let index = AsgIndexStore::from_engine(&engine);
        let ledger = AsgLedgerStore::from_engine(&engine);
        let sym2 = Symbol {
            symbol_id: "sym_y".into(),
            symbol_fp: "fp_y".into(),
            qname: "pkg.other".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/other.py".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 3, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym2, "test").unwrap();
        // Third symbol with no entries — must not appear in either form.
        let sym3 = Symbol {
            symbol_id: "sym_z".into(),
            symbol_fp: "fp_z".into(),
            qname: "pkg.zebra".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/zebra.py".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 3, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym3, "test").unwrap();

        append(
            &engine,
            "sym_x",
            LedgerKind::Hypothesis,
            "h1",
            Some(0.8),
            None,
        );
        append(
            &engine,
            "sym_x",
            LedgerKind::Decision,
            "use sqlite",
            None,
            None,
        );
        append(
            &engine,
            "sym_y",
            LedgerKind::OpenQuestion,
            "why flaky?",
            None,
            None,
        );

        // Per-qname reference implementation.
        let mut per_qname: Vec<(String, Vec<LedgerEntry>)> = Vec::new();
        for q in [qn.as_str(), "pkg.other", "pkg.zebra"] {
            let sym = index
                .get_symbol_by_qname(&engine.ref_name, q)
                .unwrap()
                .unwrap();
            let entries = ledger
                .list_entries(&engine.ref_name, &sym.symbol_id)
                .unwrap_or_default();
            if !entries.is_empty() {
                per_qname.push((sym.qname, entries));
            }
        }
        per_qname.sort_by(|a, b| a.0.cmp(&b.0));

        let bulk = all_symbol_entries(&engine);
        assert_eq!(bulk.len(), per_qname.len());
        for ((bq, be), (pq, pe)) in bulk.iter().zip(per_qname.iter()) {
            assert_eq!(bq, pq);
            let b_ids: Vec<&str> = be.iter().map(|e| e.entry_id.as_str()).collect();
            let p_ids: Vec<&str> = pe.iter().map(|e| e.entry_id.as_str()).collect();
            assert_eq!(b_ids, p_ids, "entry order mismatch for {bq}");
        }
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
