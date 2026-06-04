//! ExampleFlow refinement (1.0.76): `asd prepare-change` must
//! surface captured thinking in a `prior_thinking` block + a
//! `thinking_summary` metadata block that always emits.
//!
//! Pre-fix behavior: the CLI handler called feedback/ledger code
//! but never invoked `gather_prior_thinking`. The MCP handler did.
//! Agents using the CLI surface (the ExampleFlow field-tester)
//! got invariants but no hypotheses/mental models/open questions/
//! failed attempts — and no signal that thinking entries existed
//! but had been filtered.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry,
    LedgerKind, LedgerStore, Position, SearchFtsDb, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn mk_sym(sym_id: &str, qname: &str, file: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: format!("fp-{sym_id}"),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!("def {}()", qname.rsplit('.').next().unwrap_or(qname))),
        doc: Some(format!("Function {qname}")),
    }
}

fn append_thinking(
    engine: &Engine,
    sym_id: &str,
    kind: LedgerKind,
    summary: &str,
    confidence: Option<f64>,
    body: Option<&str>,
) {
    let ledger = AsgLedgerStore::from_engine(engine);
    let mut e = LedgerEntry::new(
        sym_id,
        kind,
        summary,
        Author { kind: AuthorKind::Agent, id: "alice".into() },
    );
    e.confidence = confidence;
    e.body = body.map(str::to_string);
    ledger.append_entry(&engine.ref_name, &e, "alice").unwrap();
}

fn seed_drift_pad_world(db: &Path) {
    let engine = Engine::open_sqlite(db).expect("open");
    let idx = AsgIndexStore::from_engine(&engine);

    // Realistic Drift Pad-shaped symbol + the kind of thinking
    // the ExampleFlow agent captured.
    let sym = mk_sym(
        "sym_sync",
        "Sources.Sequencer.syncDriftScheduler",
        "Sources/Sequencer/SyncDriftScheduler.swift",
    );
    idx.put_symbol(&engine.ref_name, &sym, "t").unwrap();
    let fts = SearchFtsDb::open(db).unwrap();
    fts.rebuild(&[sym.clone()]).expect("fts rebuild");

    // One Hypothesis (high confidence — should surface)
    append_thinking(
        &engine,
        "sym_sync",
        LedgerKind::Hypothesis,
        "scheduler routing must preserve DriftSynthPool sink overrides",
        Some(0.8),
        None,
    );
    // One MentalModel
    append_thinking(
        &engine,
        "sym_sync",
        LedgerKind::MentalModel,
        "drift-pad-pipeline: Drift Pad → compile → LaneWorkspace stubs → Scheduler",
        None,
        Some(r#"{"name":"drift-pad-pipeline","symbols":["Sources.Sequencer.syncDriftScheduler"]}"#),
    );
    // One OpenQuestion
    append_thinking(
        &engine,
        "sym_sync",
        LedgerKind::OpenQuestion,
        "should Drift Pad fully replace legacy lane playback?",
        None,
        None,
    );
    // One FailedAttempt
    append_thinking(
        &engine,
        "sym_sync",
        LedgerKind::FailedAttempt,
        "tried: route DriftClips into Scheduler directly",
        None,
        Some(r#"{"tried":"direct routing","because":"Scheduler only accepts precompiled ScheduledEvent arrays"}"#),
    );
    // One LOW-confidence hypothesis (should NOT surface; should
    // appear in by_kind_dropped)
    append_thinking(
        &engine,
        "sym_sync",
        LedgerKind::Hypothesis,
        "speculative guess that maybe works",
        Some(0.1),
        None,
    );
}

fn run_prepare_change(db: &Path, args: &[&str]) -> serde_json::Value {
    let mut cmd = Command::new(asd_bin());
    cmd.arg("--db").arg(db).arg("prepare-change");
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "prepare-change exited non-zero\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "non-JSON stdout: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn cli_surfaces_all_four_thinking_kinds() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_drift_pad_world(&db);

    let v = run_prepare_change(&db, &["syncDriftScheduler"]);

    let pt = &v["prior_thinking"];
    assert!(pt.is_object(), "prior_thinking must be present and an object; got: {pt:#?}");
    assert!(pt.get("hypotheses").is_some(), "hypotheses arm: {pt:#?}");
    assert!(pt.get("mental_models").is_some(), "mental_models arm: {pt:#?}");
    assert!(pt.get("open_questions").is_some(), "open_questions arm: {pt:#?}");
    assert!(pt.get("failed_attempts").is_some(), "failed_attempts arm: {pt:#?}");
}

#[test]
fn cli_always_emits_thinking_summary_with_correct_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_drift_pad_world(&db);

    let v = run_prepare_change(&db, &["syncDriftScheduler"]);
    let s = &v["thinking_summary"];
    assert!(s.is_object(), "thinking_summary must always emit: {v:#?}");
    // 4 surfaced (high-conf hyp + model + question + failed)
    assert_eq!(s["surfaced"].as_u64(), Some(4), "summary: {s:#?}");
    // 1 hypothesis dropped (low confidence)
    assert_eq!(
        s["by_kind_dropped"]["hypothesis"].as_u64(),
        Some(1),
        "by_kind_dropped must record the low-conf hypothesis: {s:#?}"
    );
    // matched_for_query >= 1 (we hit the seeded symbol)
    assert!(
        s["matched_for_query"].as_u64().unwrap_or(0) >= 1,
        "must report at least one matched qname: {s:#?}"
    );
}

#[test]
fn cli_thinking_floor_flag_lowers_threshold() {
    // With --thinking-floor 0.05, the low-confidence hypothesis
    // (conf=0.1) should now surface alongside the high-conf one.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_drift_pad_world(&db);

    let v = run_prepare_change(
        &db,
        &["syncDriftScheduler", "--thinking-floor", "0.05"],
    );
    let hyps = v["prior_thinking"]["hypotheses"]
        .as_array()
        .expect("hypotheses array");
    assert_eq!(
        hyps.len(),
        2,
        "with floor 0.05 both hypotheses must surface: {hyps:#?}"
    );
    // by_kind_dropped should now be 0 for hypothesis
    assert_eq!(
        v["thinking_summary"]["by_kind_dropped"]["hypothesis"].as_u64(),
        Some(0),
        "no hypotheses dropped at floor 0.05"
    );
}

#[test]
fn cli_emits_workspace_count_only_when_query_matched_nothing() {
    // Seed thinking on sym_sync, then query a completely
    // unrelated description. matched_for_query stays 0 →
    // entries_in_workspace must populate.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_drift_pad_world(&db);

    let v = run_prepare_change(&db, &["completelyunrelatednonsensequery"]);
    let s = &v["thinking_summary"];
    // matched_for_query is 0 because likely_edit_files is empty
    // (or contains symbols with no thinking).
    // Either way: entries_in_workspace MUST appear because the
    // scan was empty-handed.
    if s["matched_for_query"].as_u64() == Some(0) {
        assert!(
            s["entries_in_workspace"].as_u64().unwrap_or(0) >= 1,
            "must populate workspace count when query matched nothing; got: {s:#?}"
        );
    }
}
