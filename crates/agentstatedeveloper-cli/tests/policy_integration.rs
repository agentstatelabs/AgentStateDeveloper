//! CLI integration tests for the `asd policy` subcommand.
//!
//! Covers:
//!   - `asd policy list` — lists loaded rules from a policy file
//!   - `asd policy evaluate` — returns allow / awaiting-approval / denied
//!   - `asd ledger append` with `--policy` — enforces deny rule at write time

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-policy-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn asd(dir: &Path, extra_flags: &[&str], args: &[&str]) -> std::process::Output {
    let db = dir.join(".asd-state.db");
    Command::new(env!("CARGO_BIN_EXE_asd"))
        .args(["--db", db.to_str().unwrap()])
        .args(extra_flags)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run asd")
}

fn policy_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .parent().unwrap()
        .join("examples/policies.json")
}

fn setup(dir: &Path) {
    std::fs::write(
        dir.join("svc.py"),
        "def charge(amount):\n    print(amount)\n    return amount > 0\n",
    )
    .unwrap();
    let o = asd(dir, &[], &["init", "--no-hooks"]);
    assert!(o.status.success(), "init: {}", String::from_utf8_lossy(&o.stderr));
    let o = asd(dir, &[], &["index", "."]);
    assert!(o.status.success(), "index: {}", String::from_utf8_lossy(&o.stderr));
}

// ---------------------------------------------------------------------------

#[test]
fn policy_list_returns_loaded_rules() {
    let dir = unique_temp_dir("list");
    setup(&dir);
    let policy = policy_file();
    let policy_flag = ["--policy", policy.to_str().unwrap()];

    let o = asd(&dir, &policy_flag, &["policy", "list"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse list output");
    let count = v["count"].as_u64().unwrap_or(0);
    assert!(count >= 1, "expected at least 1 policy rule; got {v}");
    let policies = v["policies"].as_array().unwrap();
    assert_eq!(policies.len() as u64, count);
    // Spot-check a known rule from examples/policies.json.
    assert!(
        policies.iter().any(|p| p["match_action"].as_str() == Some("asd.ledger.append.hazard")),
        "hazard-requires-human rule missing from list"
    );
}

#[test]
fn policy_evaluate_deny() {
    let dir = unique_temp_dir("deny");
    setup(&dir);
    let policy = policy_file();
    let policy_flag = ["--policy", policy.to_str().unwrap()];

    let o = asd(
        &dir,
        &policy_flag,
        &["policy", "evaluate", "asd.ledger.append.tradeoff", "--agent-id", "experimental-bot"],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse evaluate output");
    assert_eq!(v["status"].as_str().unwrap(), "denied", "expected denied; got {v}");
    assert!(v["matched_policy"].as_str().is_some());
}

#[test]
fn policy_evaluate_require_approval() {
    let dir = unique_temp_dir("approval");
    setup(&dir);
    let policy = policy_file();
    let policy_flag = ["--policy", policy.to_str().unwrap()];

    let o = asd(
        &dir,
        &policy_flag,
        &["policy", "evaluate", "asd.ledger.append.hazard", "--agent-id", "my-agent"],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse evaluate output");
    assert_eq!(v["status"].as_str().unwrap(), "awaiting-approval", "expected awaiting-approval; got {v}");
    let approvers = v["approvers"].as_array().unwrap();
    assert!(approvers.iter().any(|a| a.as_str() == Some("human")));
}

#[test]
fn policy_evaluate_allow() {
    let dir = unique_temp_dir("allow");
    setup(&dir);
    let policy = policy_file();
    let policy_flag = ["--policy", policy.to_str().unwrap()];

    let o = asd(
        &dir,
        &policy_flag,
        &["policy", "evaluate", "asd.ledger.append.rationale", "--agent-id", "my-agent"],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse evaluate output");
    assert_eq!(v["status"].as_str().unwrap(), "allowed", "expected allowed; got {v}");
    assert!(v["matched_policy"].is_null());
}

#[test]
fn policy_denies_ledger_append_at_write_time() {
    let dir = unique_temp_dir("write-gate");
    setup(&dir);
    let policy = policy_file();
    let policy_flag = ["--policy", policy.to_str().unwrap()];

    // experimental-bot cannot append tradeoff entries.
    let o = asd(
        &dir,
        &policy_flag,
        &[
            "ledger", "append", "svc.charge",
            "--kind", "tradeoff",
            "--summary", "should be blocked",
            "--author-id", "experimental-bot",
        ],
    );
    assert!(!o.status.success(), "expected ledger append to fail under deny policy");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("denied") || stderr.contains("policy"),
        "expected 'denied' in stderr; got: {stderr}"
    );
}

#[test]
fn policy_allows_ledger_append_without_matching_rule() {
    let dir = unique_temp_dir("write-allow");
    setup(&dir);
    let policy = policy_file();
    let policy_flag = ["--policy", policy.to_str().unwrap()];

    // rationale entries have no deny rule — should succeed.
    let o = asd(
        &dir,
        &policy_flag,
        &[
            "ledger", "append", "svc.charge",
            "--kind", "rationale",
            "--summary", "this is fine",
            "--author-id", "regular-agent",
        ],
    );
    assert!(o.status.success(), "expected append to succeed; stderr: {}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse append output");
    let status = v["status"].as_str().unwrap();
    assert!(
        status == "allowed" || status == "no-policy-match",
        "expected allowed/no-policy-match; got: {status}"
    );
    assert!(v["entry_id"].as_str().is_some(), "entry_id missing from response");
}
