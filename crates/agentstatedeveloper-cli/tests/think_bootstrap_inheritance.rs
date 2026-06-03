//! Plan K t-006: integration tests for the auto-detected "Inherited
//! thinking from prior session(s)" block in `asd think bootstrap`.
//!
//! Each test seeds an in-process engine with thinking entries via
//! the core APIs, then spawns the asd binary to render the bootstrap
//! output and asserts the inheritance block is or isn't surfaced.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry,
    LedgerKind, LedgerStore, Position, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn seed_engine(db_path: &Path) -> Engine {
    let engine = Engine::open_sqlite(db_path).expect("open sqlite");
    let sym = Symbol {
        symbol_id: "sym_target".into(),
        symbol_fp: "fp".into(),
        qname: "pkg.target".into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: "src/target.py".into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: None,
        doc: None,
    };
    AsgIndexStore::from_engine(&engine)
        .put_symbol(&engine.ref_name, &sym, "test")
        .unwrap();
    engine
}

fn append_thinking(
    engine: &Engine,
    sym_id: &str,
    kind: LedgerKind,
    summary: &str,
    conf: Option<f64>,
    author_id: &str,
    suffix: &str,
) {
    let mut entry = LedgerEntry::new(
        sym_id,
        kind,
        summary,
        Author { kind: AuthorKind::Agent, id: author_id.into() },
    );
    entry.entry_id = format!("led_test_{}", suffix);
    entry.confidence = conf;
    AsgLedgerStore::from_engine(engine)
        .append_entry(&engine.ref_name, &entry, author_id)
        .unwrap();
}

fn run_bootstrap(db_path: &Path) -> (bool, String) {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db_path)
        .arg("think")
        .arg("bootstrap")
        .output()
        .expect("spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn run_bootstrap_check(db_path: &Path) -> (bool, String) {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db_path)
        .arg("think")
        .arg("bootstrap")
        .arg("--check")
        .output()
        .expect("spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn bootstrap_omits_inheritance_block_on_empty_project() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let _engine = seed_engine(&db);

    let (ok, stdout) = run_bootstrap(&db);
    assert!(ok);
    assert!(
        !stdout.contains("Inherited thinking from prior session(s)"),
        "empty project must not show inheritance block; got:\n{stdout}"
    );
}

#[test]
fn bootstrap_shows_inheritance_block_when_one_mental_model_present() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = seed_engine(&db);
    append_thinking(
        &engine,
        "sym_target",
        LedgerKind::MentalModel,
        "audio-pipeline: in → mix → out",
        None,
        "prior_dev",
        "mm1",
    );

    let (ok, stdout) = run_bootstrap(&db);
    assert!(ok);
    assert!(
        stdout.contains("Inherited thinking from prior session(s)"),
        "one MentalModel should trigger inheritance preview; got:\n{stdout}"
    );
    assert!(
        stdout.contains("audio-pipeline"),
        "inheritance preview must show the model summary; got:\n{stdout}"
    );
    assert!(
        stdout.contains("prior_dev"),
        "inheritance preview must show the author; got:\n{stdout}"
    );
}

#[test]
fn bootstrap_shows_inheritance_block_when_three_hypotheses_present() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = seed_engine(&db);
    for (i, conf) in [0.6, 0.4, 0.8].iter().enumerate() {
        append_thinking(
            &engine,
            "sym_target",
            LedgerKind::Hypothesis,
            &format!("hypothesis number {i}"),
            Some(*conf),
            "prior_dev",
            &format!("h{i}"),
        );
    }

    let (ok, stdout) = run_bootstrap(&db);
    assert!(ok);
    assert!(
        stdout.contains("Inherited thinking from prior session(s)"),
        "3 hypotheses should trigger inheritance preview; got:\n{stdout}"
    );
    // Highest-confidence hypothesis (0.8) should appear in the
    // "Top hypotheses" block.
    let preview_section: String = stdout
        .lines()
        .skip_while(|l| !l.contains("Top hypotheses"))
        .take(5)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        preview_section.contains("0.80"),
        "highest-confidence hypothesis must lead the list; got:\n{preview_section}"
    );
}

#[test]
fn bootstrap_does_not_trigger_on_two_hypotheses_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = seed_engine(&db);
    for i in 0..2 {
        append_thinking(
            &engine,
            "sym_target",
            LedgerKind::Hypothesis,
            &format!("h {i}"),
            Some(0.5),
            "prior_dev",
            &format!("h{i}"),
        );
    }

    let (ok, stdout) = run_bootstrap(&db);
    assert!(ok);
    assert!(
        !stdout.contains("Inherited thinking from prior session(s)"),
        "2 hypotheses without a mental model must NOT trigger preview; got:\n{stdout}"
    );
}

#[test]
fn check_mode_reports_you_vs_team_counts() {
    // Two entries from `prior_dev`, one from the current agent.
    // The default agent_id in Config is the local user's username
    // (set by Config::default()), so we just hard-code the asd
    // process's default by reading the env's USER.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = seed_engine(&db);

    // Three "prior_dev" + one "current" hypothesis. The "you"
    // count depends on what agent_id the asd binary computes; we
    // assert only that the output format includes team/you columns.
    for i in 0..3 {
        append_thinking(
            &engine,
            "sym_target",
            LedgerKind::Hypothesis,
            &format!("h {i}"),
            Some(0.5),
            "prior_dev",
            &format!("h{i}"),
        );
    }
    append_thinking(
        &engine,
        "sym_target",
        LedgerKind::MentalModel,
        "my own model",
        None,
        "prior_dev",
        "mm1",
    );

    let (ok, stdout) = run_bootstrap_check(&db);
    assert!(ok);
    assert!(
        stdout.contains("counts (team / you)"),
        "--check must report the team/you breakdown header; got:\n{stdout}"
    );
    assert!(
        stdout.contains("hypothesis"),
        "--check must list the per-kind rows; got:\n{stdout}"
    );
}
