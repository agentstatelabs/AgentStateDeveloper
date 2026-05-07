//! Integration test for the CLI audit-log emit path.
//! Spawns `asd` via CARGO_BIN_EXE_asd, exercises ledger ops with
//! `--audit-log`, reads back the JSONL file, and verifies the expected
//! events are there.
//!
//! NOTE (2026-04-20): this test exercises commercial features
//! (ledger approve + hash-chained audit-log writes) which have been
//! extracted to `AgentStateDeveloper-Enterprise` / `asd-pro`. It is
//! retained here ignored; the equivalent live test lives in the
//! enterprise workspace.

use std::path::PathBuf;
use std::process::Command;

use agentstatedeveloper_core::{event_types, read_jsonl};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at .../crates/agentstatedeveloper-cli
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // repo root
    p
}

fn run(cmd: &mut Command) -> std::process::Output {
    let out = cmd.output().expect("spawn asd");
    if !out.status.success() {
        eprintln!(
            "cmd {:?} failed:\nstdout: {}\nstderr: {}",
            cmd,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out
}

#[test]
#[ignore = "exercises commercial features (ledger approve + hash-chained audit) — runs against asd-pro in the enterprise workspace"]
fn cli_emits_audit_events_for_ledger_ops() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = repo_root().join("examples/sample-py-repo");
    let dst = tmp.path().join("repo");
    copy_dir(&src, &dst);

    let db = dst.join(".asd-state.db");
    let audit = dst.join(".asd-audit.jsonl");
    let policy = repo_root().join("examples/policies.json");

    // init + index
    run(Command::new(asd_bin())
        .arg("--db").arg(&db)
        .arg("init"));
    run(Command::new(asd_bin())
        .arg("--db").arg(&db)
        .arg("index").arg(&dst));

    // append a hazard under policy (should emit awaiting-approval)
    let out = run(Command::new(asd_bin())
        .arg("--db").arg(&db)
        .arg("--policy").arg(&policy)
        .arg("--audit-log").arg(&audit)
        .arg("ledger").arg("append").arg("payments.charge_card")
        .arg("--kind").arg("hazard")
        .arg("--summary").arg("hazard test")
        .arg("--author-id").arg("alice")
        .arg("--author-kind").arg("human"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("parse json");
    let entry_id = parsed["entry_id"].as_str().expect("entry_id").to_string();

    // approve it
    run(Command::new(asd_bin())
        .arg("--db").arg(&db)
        .arg("--audit-log").arg(&audit)
        .arg("ledger").arg("approve").arg(&entry_id)
        .arg("--approver").arg("alice")
        .arg("--approver-kind").arg("human"));

    // policy-denied append (experimental-bot + tradeoff → deny)
    let denied = Command::new(asd_bin())
        .arg("--db").arg(&db)
        .arg("--policy").arg(&policy)
        .arg("--audit-log").arg(&audit)
        .arg("ledger").arg("append").arg("payments.charge_card")
        .arg("--kind").arg("tradeoff")
        .arg("--summary").arg("rejected op")
        .arg("--author-id").arg("experimental-bot")
        .arg("--author-kind").arg("agent")
        .output()
        .unwrap();
    assert!(!denied.status.success(), "expected denied op to fail");

    // Read back events.
    let events = read_jsonl(&audit).expect("read audit log");
    assert!(events.len() >= 3, "expected ≥3 events, got {}", events.len());

    // Verify shapes.
    let append_evt = events
        .iter()
        .find(|e| e.event_type == event_types::LEDGER_APPEND && e.outcome == "awaiting-approval")
        .expect("missing awaiting-approval append event");
    assert_eq!(append_evt.actor_id, "alice");
    assert_eq!(append_evt.subject_id.as_deref(), Some(entry_id.as_str()));
    assert!(append_evt.matched_policy.is_some());

    let approve_evt = events
        .iter()
        .find(|e| e.event_type == event_types::LEDGER_APPROVE)
        .expect("missing approve event");
    assert_eq!(approve_evt.outcome, "approved");
    assert_eq!(approve_evt.actor_id, "alice");

    let denied_evt = events
        .iter()
        .find(|e| e.event_type == event_types::LEDGER_APPEND && e.outcome == "denied")
        .expect("missing denied append event");
    assert_eq!(denied_evt.actor_id, "experimental-bot");
    assert!(denied_evt.reason.is_some());
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let e = entry.unwrap();
        let name = e.file_name();
        if matches!(name.to_string_lossy().as_ref(), ".asd" | ".asd-state.db") {
            continue;
        }
        let from = e.path();
        let to = dst.join(&name);
        if e.file_type().unwrap().is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}
