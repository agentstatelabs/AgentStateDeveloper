//! Plan G t-006: read-side helper for surfacing captured thinking
//! (Hypothesis / MentalModel / FailedAttempt / OpenQuestion) into
//! prepare-change and context-for responses.
//!
//! The shape `gather_prior_thinking` returns is the exact JSON section
//! that gets embedded as `prior_thinking` in both handlers' output.
//! Keeps the projection logic single-sourced so future tweaks (a new
//! kind, a different threshold) land once.

use serde_json::{json, Value};

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::LedgerKind;

/// Hypotheses with confidence below this are excluded from auto-surface
/// (still queryable via `asd think list`). Plan G t-001 picked 0.3 as
/// the default; callers may override.
pub const DEFAULT_CONFIDENCE_FLOOR: f64 = 0.3;

/// Walk the given qnames, collect Plan G thinking entries, project to
/// the compact `prior_thinking` JSON shape. Hypotheses below
/// `min_confidence` are dropped.
///
/// Returns `Value::Null` when nothing surfaces (lets callers omit the
/// field instead of emitting an empty section).
pub fn gather_prior_thinking(
    engine: &Engine,
    qnames: &[String],
    min_confidence: f64,
) -> Value {
    let index = AsgIndexStore::from_engine(engine);
    let ledger = AsgLedgerStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    let mut hypotheses: Vec<Value> = Vec::new();
    let mut mental_models: Vec<Value> = Vec::new();
    let mut open_questions: Vec<Value> = Vec::new();
    let mut failed_attempts: Vec<Value> = Vec::new();

    for qn in qnames {
        let sym = match index.get_symbol_by_qname(&ref_name, qn) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let entries = ledger
            .list_entries(&ref_name, &sym.symbol_id)
            .unwrap_or_default();
        for entry in entries {
            match entry.kind {
                LedgerKind::Hypothesis => {
                    let conf = entry.confidence.unwrap_or(0.0);
                    if conf < min_confidence {
                        continue;
                    }
                    hypotheses.push(json!({
                        "qname": qn,
                        "summary": entry.summary,
                        "confidence": conf,
                    }));
                }
                LedgerKind::MentalModel => {
                    // Parse symbols[] + name out of body JSON.
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
                }
                LedgerKind::OpenQuestion => {
                    open_questions.push(json!({
                        "qname": qn,
                        "question": entry.summary,
                    }));
                }
                LedgerKind::FailedAttempt => {
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
                }
                _ => {}
            }
        }
    }

    if hypotheses.is_empty()
        && mental_models.is_empty()
        && open_questions.is_empty()
        && failed_attempts.is_empty()
    {
        return Value::Null;
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        Author, AuthorKind, LedgerEntry, Position, Symbol, SymbolKind,
    };

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

    fn append(engine: &Engine, sym_id: &str, kind: LedgerKind, summary: &str, conf: Option<f64>, body: Option<&str>) {
        let ledger = AsgLedgerStore::from_engine(engine);
        let mut entry = LedgerEntry::new(
            sym_id,
            kind,
            summary,
            Author { kind: AuthorKind::Agent, id: "t".into() },
        );
        entry.confidence = conf;
        entry.body = body.map(str::to_string);
        ledger.append_entry(&engine.ref_name, &entry, "t").unwrap();
    }

    #[test]
    fn returns_null_when_no_thinking_entries() {
        let (engine, qn) = seed();
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn surfaces_high_confidence_hypothesis() {
        let (engine, qn) = seed();
        append(&engine, "sym_x", LedgerKind::Hypothesis, "X causes Y", Some(0.7), None);
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let o = v.as_object().unwrap();
        let hyps = o["hypotheses"].as_array().unwrap();
        assert_eq!(hyps.len(), 1);
        assert_eq!(hyps[0]["confidence"].as_f64(), Some(0.7));
        assert_eq!(hyps[0]["summary"].as_str(), Some("X causes Y"));
    }

    #[test]
    fn excludes_below_confidence_floor() {
        let (engine, qn) = seed();
        append(&engine, "sym_x", LedgerKind::Hypothesis, "weak guess", Some(0.1), None);
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        assert_eq!(v, Value::Null, "below-floor hypothesis must be excluded");
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
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let mm = v["mental_models"].as_array().unwrap();
        assert_eq!(mm.len(), 1);
        assert_eq!(mm[0]["name"].as_str(), Some("audio-pipeline"));
        let syms = mm[0]["symbols"].as_array().unwrap();
        assert_eq!(syms.len(), 2);
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
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let fa = v["failed_attempts"].as_array().unwrap();
        assert_eq!(fa[0]["tried"].as_str(), Some("caching"));
        assert_eq!(fa[0]["because"].as_str(), Some("broke under reload"));
    }

    #[test]
    fn surfaces_open_question() {
        let (engine, qn) = seed();
        append(&engine, "sym_x", LedgerKind::OpenQuestion, "what does 4096 mean?", None, None);
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        let oq = v["open_questions"].as_array().unwrap();
        assert_eq!(oq[0]["question"].as_str(), Some("what does 4096 mean?"));
    }

    #[test]
    fn excludes_non_thinking_kinds() {
        // Decision/Constraint/Mapping should NOT appear in prior_thinking.
        let (engine, qn) = seed();
        append(&engine, "sym_x", LedgerKind::Decision, "decided X", None, None);
        append(&engine, "sym_x", LedgerKind::Constraint, "must Y", None, None);
        append(&engine, "sym_x", LedgerKind::Mapping, "covers Z", None, None);
        let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
        assert_eq!(v, Value::Null, "non-thinking kinds must not surface in prior_thinking");
    }
}
