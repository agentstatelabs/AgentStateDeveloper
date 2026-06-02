//! Plan G t-008: end-to-end acceptance tests for the thinking layer.
//!
//! These tests drive the same core entry points the CLI / MCP `asd
//! think *` handlers use, then assert that:
//!   1. Thinking entries land in the ledger with the expected kinds.
//!   2. `gather_prior_thinking` projects them back to the auto-surface
//!      shape (`hypotheses` / `mental_models` / `failed_attempts` /
//!      `open_questions`) consumed by prepare_change / context_for.
//!   3. Confidence floor excludes weak hypotheses, and re-running with
//!      the same det_id is idempotent (no duplicate rows).
//!
//! Storage is real SQLite (not in-memory) so the round-trip is real.

use std::path::PathBuf;

use agentstatedeveloper_core::{
    thinking::{gather_prior_thinking, DEFAULT_CONFIDENCE_FLOOR},
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind,
};

fn fresh_engine() -> (tempfile::TempDir, PathBuf, Engine) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db_path).expect("open sqlite engine");
    (tmp, db_path, engine)
}

fn put_sym(engine: &Engine, qname: &str) -> String {
    let index = AsgIndexStore::from_engine(engine);
    let sym = Symbol {
        symbol_id: format!("sym_{}", qname.replace('.', "_")),
        symbol_fp: "fp".into(),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: "src/x.py".into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: None,
        doc: None,
    };
    index.put_symbol(&engine.ref_name, &sym, "t").expect("put");
    sym.symbol_id
}

/// Mirror of the CLI/MCP det_id so re-runs are byte-identical.
fn det_id(intent: &str, qname: &str, content: &str) -> String {
    let key = format!("think:{intent}:{qname}:{content}");
    let h = blake3::hash(key.as_bytes()).to_hex();
    let short: String = h.chars().take(24).collect();
    format!("led_think_{short}")
}

fn append_think(
    engine: &Engine,
    sym_id: &str,
    kind: LedgerKind,
    entry_id: &str,
    summary: &str,
    conf: Option<f64>,
    body: Option<&str>,
    tags: Vec<String>,
) {
    let ledger = AsgLedgerStore::from_engine(engine);
    let mut entry = LedgerEntry::new(
        sym_id,
        kind,
        summary,
        Author { kind: AuthorKind::Agent, id: "asd-think".into() },
    );
    entry.entry_id = entry_id.to_string();
    entry.confidence = conf;
    entry.body = body.map(str::to_string);
    entry.tags = tags;
    ledger
        .append_entry(&engine.ref_name, &entry, "asd-think")
        .expect("append");
}

#[test]
fn end_to_end_thinking_surfaces_in_prior_thinking_projection() {
    let (_tmp, _db, engine) = fresh_engine();
    let qn = "pkg.target".to_string();
    let sid = put_sym(&engine, &qn);

    // Speculate: above and below the floor.
    append_think(
        &engine,
        &sid,
        LedgerKind::Hypothesis,
        &det_id("hypothesis", &qn, "strong claim"),
        "strong claim",
        Some(0.7),
        None,
        vec!["source:asd-think".into()],
    );
    append_think(
        &engine,
        &sid,
        LedgerKind::Hypothesis,
        &det_id("hypothesis", &qn, "weak guess"),
        "weak guess",
        Some(0.1),
        None,
        vec!["source:asd-think".into()],
    );
    // Mental model — body carries symbols[] + name.
    append_think(
        &engine,
        &sid,
        LedgerKind::MentalModel,
        &det_id("model", "audio-pipeline", "in→mix→out"),
        "audio-pipeline: in→mix→out",
        None,
        Some(r#"{"symbols":["pkg.target","pkg.other"],"name":"audio-pipeline"}"#),
        vec!["source:asd-think".into()],
    );
    // Failed attempt — body carries tried/because.
    append_think(
        &engine,
        &sid,
        LedgerKind::FailedAttempt,
        &det_id("failed", &qn, "caching"),
        "failed: caching — because broke under reload",
        None,
        Some(r#"{"tried":"caching","because":"broke under reload"}"#),
        vec!["source:asd-think".into()],
    );
    // Open question.
    append_think(
        &engine,
        &sid,
        LedgerKind::OpenQuestion,
        &det_id("question", &qn, "what is 4096?"),
        "what is 4096?",
        None,
        None,
        vec!["source:asd-think".into()],
    );

    let v = gather_prior_thinking(&engine, &[qn.clone()], DEFAULT_CONFIDENCE_FLOOR);
    let o = v.as_object().expect("non-null projection");

    let hyps = o["hypotheses"].as_array().expect("hypotheses");
    assert_eq!(hyps.len(), 1, "below-floor hypothesis must be filtered out");
    assert_eq!(hyps[0]["confidence"].as_f64(), Some(0.7));

    let mm = o["mental_models"].as_array().expect("mental_models");
    assert_eq!(mm.len(), 1);
    assert_eq!(mm[0]["name"].as_str(), Some("audio-pipeline"));
    assert_eq!(mm[0]["symbols"].as_array().unwrap().len(), 2);

    let fa = o["failed_attempts"].as_array().expect("failed_attempts");
    assert_eq!(fa[0]["tried"].as_str(), Some("caching"));
    assert_eq!(fa[0]["because"].as_str(), Some("broke under reload"));

    let oq = o["open_questions"].as_array().expect("open_questions");
    assert_eq!(oq[0]["question"].as_str(), Some("what is 4096?"));
}

#[test]
fn deterministic_entry_id_is_idempotent_on_replay() {
    // Two writes with the same (intent, qname, summary) collapse to one
    // row — re-running the initial-read prompt must not duplicate.
    let (_tmp, _db, engine) = fresh_engine();
    let qn = "pkg.idem".to_string();
    let sid = put_sym(&engine, &qn);

    let id = det_id("hypothesis", &qn, "same claim");
    append_think(
        &engine,
        &sid,
        LedgerKind::Hypothesis,
        &id,
        "same claim",
        Some(0.5),
        None,
        vec!["source:asd-think".into()],
    );
    append_think(
        &engine,
        &sid,
        LedgerKind::Hypothesis,
        &id,
        "same claim",
        Some(0.5),
        None,
        vec!["source:asd-think".into()],
    );

    let ledger = AsgLedgerStore::from_engine(&engine);
    let entries = ledger
        .list_entries(&engine.ref_name, &sid)
        .expect("list");
    let hyps: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == LedgerKind::Hypothesis)
        .collect();
    assert_eq!(
        hyps.len(),
        1,
        "deterministic id must collapse re-runs to a single row"
    );
}

#[test]
fn gather_prior_thinking_returns_null_when_only_non_thinking_kinds_exist() {
    // Decision / Constraint / Mapping live in different conclusion
    // buckets and must NOT leak into prior_thinking.
    let (_tmp, _db, engine) = fresh_engine();
    let qn = "pkg.other".to_string();
    let sid = put_sym(&engine, &qn);

    append_think(
        &engine,
        &sid,
        LedgerKind::Decision,
        "led_dec_1",
        "decided X",
        None,
        None,
        vec![],
    );
    append_think(
        &engine,
        &sid,
        LedgerKind::Mapping,
        "led_map_1",
        "covers Z",
        None,
        None,
        vec![],
    );

    let v = gather_prior_thinking(&engine, &[qn], DEFAULT_CONFIDENCE_FLOOR);
    assert_eq!(v, serde_json::Value::Null);
}
