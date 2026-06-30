//! Plan K t-008: integration tests for `asd conclusions export
//! --check-budget` and `--soft`. Seeds an engine with enough decision
//! entries to blow a small budget, exports, and asserts the exit
//! code + JSON shape.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn seed_engine_with_decisions(db_path: &Path, n: usize) {
    let engine = Engine::open_sqlite(db_path).expect("open sqlite");
    let sym = Symbol {
        symbol_id: "sym_budget_test".into(),
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
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();
    let ledger = AsgLedgerStore::from_engine(&engine);
    for i in 0..n {
        let mut entry = LedgerEntry::new(
            "sym_budget_test",
            LedgerKind::Decision,
            &format!("decision #{i} with some prose to take bytes"),
            Author {
                kind: AuthorKind::Agent,
                id: "t".into(),
            },
        );
        entry.entry_id = format!("led_budget_{i:04}");
        ledger.append_entry(&engine.ref_name, &entry, "t").unwrap();
    }
}

fn write_budget_config(project_root: &Path, total: u64, per_shard: u64) {
    let asd = project_root.join(".asd");
    std::fs::create_dir_all(&asd).unwrap();
    std::fs::write(
        asd.join("config.toml"),
        format!("[sidecar]\nbudget_total_bytes = {total}\nbudget_per_shard_bytes = {per_shard}\n"),
    )
    .unwrap();
}

fn run_export(project_root: &Path, args: &[&str]) -> (i32, String, String) {
    let db = project_root.join(".asd-state.db");
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(&db)
        .arg("conclusions")
        .arg("export")
        .args(args)
        .current_dir(project_root)
        .output()
        .expect("spawn");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn check_budget_passes_when_under_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_with_decisions(&db, 3);
    write_budget_config(tmp.path(), 10_000, 10_000);

    let (code, stdout, stderr) = run_export(tmp.path(), &["--check-budget"]);
    assert_eq!(
        code, 0,
        "exit 0 when under budget; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("\"ok\": true"),
        "budget block must report ok=true; got: {stdout}"
    );
}

#[test]
fn check_budget_hard_fails_when_total_exceeded() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_with_decisions(&db, 20);
    // Total budget impossibly small — guaranteed bust.
    write_budget_config(tmp.path(), 50, 100_000);

    let (code, stdout, stderr) = run_export(tmp.path(), &["--check-budget"]);
    assert_ne!(
        code, 0,
        "hard fail must exit non-zero; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("sidecar budget exceeded") || stderr.contains("total"),
        "error message must mention budget; stderr: {stderr}"
    );
}

#[test]
fn check_budget_soft_warns_but_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_with_decisions(&db, 20);
    write_budget_config(tmp.path(), 50, 100_000);

    let (code, stdout, stderr) = run_export(tmp.path(), &["--check-budget", "--soft"]);
    assert_eq!(
        code, 0,
        "--soft must exit 0 even on violation; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("warning") && stderr.contains("budget exceeded"),
        "--soft must emit a stderr warning; got: {stderr}"
    );
    // JSON payload should still include the budget block with ok=false.
    assert!(
        stdout.contains("\"ok\": false"),
        "budget block must show ok=false; got: {stdout}"
    );
}

#[test]
fn check_budget_omits_budget_block_when_flag_unused() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_with_decisions(&db, 3);
    // No config file → defaults; but we're not passing --check-budget
    // so the budget block must not appear at all.

    let (code, stdout, _stderr) = run_export(tmp.path(), &[]);
    assert_eq!(code, 0);
    assert!(
        !stdout.contains("\"budget\""),
        "JSON must not include `budget` key without --check-budget; got: {stdout}"
    );
}
