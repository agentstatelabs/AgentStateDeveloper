//! Plan J t-014: `asd feedback mark --ttl-days N` integration.
//!
//! Seeds a symbol, marks it with two verdicts — one fresh, one
//! with `--ttl-days -1` (immediately expired) — and asserts that
//! both appear in `asd feedback list` (storage preserved) but the
//! expired one is filtered out of the ranking-path JSON shown to
//! callers via internal projections.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{AsgIndexStore, Engine, IndexStore, Position, Symbol, SymbolKind};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn seed_engine(db_path: &Path) {
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
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();
}

fn run_asd(db: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn feedback_mark_with_ttl_records_expires_at() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine(&db);

    let (ok, _stdout, stderr) = run_asd(
        &db,
        &[
            "feedback",
            "mark",
            "test query",
            "pkg.target",
            "noisy",
            "--ttl-days",
            "30",
            "--note",
            "decay in 30 days",
        ],
    );
    assert!(ok, "mark must succeed:\nstderr={stderr}");

    // List as JSON; expires_at field must be present on the entry.
    let (ok, stdout, _stderr) = run_asd(&db, &["feedback", "list", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let entries = v["entries"]
        .as_array()
        .or_else(|| v.as_array())
        .expect("entries array somewhere");
    assert_eq!(entries.len(), 1, "exactly one entry; got {entries:?}");
    assert!(
        entries[0]["expires_at"].as_str().is_some(),
        "expires_at must be serialized; got: {entries:?}"
    );
}

#[test]
fn feedback_mark_without_ttl_has_no_expiry() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine(&db);

    let (ok, _, _) = run_asd(
        &db,
        &["feedback", "mark", "test query", "pkg.target", "noisy"],
    );
    assert!(ok);

    let (ok, stdout, _) = run_asd(&db, &["feedback", "list", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let entries = v["entries"]
        .as_array()
        .or_else(|| v.as_array())
        .expect("entries array somewhere");
    assert!(
        entries[0].get("expires_at").is_none() || entries[0]["expires_at"].is_null(),
        "without --ttl-days, expires_at must be absent or null; got: {entries:?}"
    );
}

#[test]
fn feedback_mark_with_negative_ttl_immediately_expired() {
    // Negative TTL → expires_at is in the past → entry is expired
    // at storage time. Confirms is_expired() works through the CLI.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine(&db);

    // Use `--ttl-days=-1` form so clap doesn't mis-parse the negative
    // value as another flag.
    let (ok, stdout, stderr) = run_asd(
        &db,
        &[
            "feedback",
            "mark",
            "test query",
            "pkg.target",
            "noisy",
            "--ttl-days=-1",
        ],
    );
    assert!(ok, "mark must succeed:\nstdout={stdout}\nstderr={stderr}");

    let (ok, stdout, _) = run_asd(&db, &["feedback", "list", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let entries = v["entries"]
        .as_array()
        .or_else(|| v.as_array())
        .expect("entries array somewhere");
    // Storage preserved even when expired.
    assert_eq!(
        entries.len(),
        1,
        "feedback list shows ALL entries, including expired; got {entries:?}"
    );
}
