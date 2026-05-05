//! CLI integration tests for ledger subcommands.
//!
//! Covers:
//!   - `asd ledger append` — entry lands in `asd read` output
//!   - `asd ledger supersede` — new entry marks old one as superseded
//!   - `asd ledger rebind` — re-parents ledger entries to a new symbol
//!   - `asd ledger approve/reject/withdraw` — commercial-feature gate fires

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-ledger-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn asd(dir: &Path, args: &[&str]) -> std::process::Output {
    let db = dir.join(".asd-state.db");
    Command::new(env!("CARGO_BIN_EXE_asd"))
        .args(["--db", db.to_str().unwrap()])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run asd")
}

fn setup(dir: &Path) {
    // Write a small Python file so we have indexable symbols.
    std::fs::write(
        dir.join("svc.py"),
        "def charge(amount):\n    print(amount)\n    return amount > 0\n\ndef refund(amount):\n    return amount > 0\n",
    )
    .unwrap();
    let o = asd(dir, &["init", "--no-hooks"]);
    assert!(o.status.success(), "init: {}", String::from_utf8_lossy(&o.stderr));
    let o = asd(dir, &["index", "."]);
    assert!(o.status.success(), "index: {}", String::from_utf8_lossy(&o.stderr));
}

fn ledger_append(dir: &Path, qname: &str, kind: &str, summary: &str) -> String {
    let o = asd(
        dir,
        &[
            "ledger", "append", qname,
            "--kind", kind,
            "--summary", summary,
            "--author-kind", "human",
            "--author-id", "tester@example.com",
        ],
    );
    assert!(o.status.success(), "ledger append failed: {}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse append output");
    assert_eq!(v["status"].as_str().unwrap(), "allowed");
    v["entry_id"].as_str().unwrap().to_owned()
}

fn read_ledger(dir: &Path, qname: &str) -> Vec<serde_json::Value> {
    let o = asd(dir, &["read", qname]);
    assert!(o.status.success(), "asd read {qname}: {}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse read output");
    v["ledger"].as_array().cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------

#[test]
fn ledger_append_appears_in_read() {
    let dir = unique_temp_dir("append");
    setup(&dir);

    let entry_id = ledger_append(&dir, "svc.charge", "hazard", "fails silently above 10000");

    let entries = read_ledger(&dir, "svc.charge");
    assert!(
        entries.iter().any(|e| e["entry_id"].as_str() == Some(&entry_id)),
        "entry {entry_id} not found in read output; got {entries:?}"
    );
    let entry = entries.iter().find(|e| e["entry_id"].as_str() == Some(&entry_id)).unwrap();
    assert_eq!(entry["kind"].as_str().unwrap(), "hazard");
    assert_eq!(entry["summary"].as_str().unwrap(), "fails silently above 10000");
    assert_eq!(entry["author"]["id"].as_str().unwrap(), "tester@example.com");
}

#[test]
fn ledger_supersede_marks_old_entry() {
    let dir = unique_temp_dir("supersede");
    setup(&dir);

    let old_id = ledger_append(&dir, "svc.charge", "assumption", "amount is always positive");

    let o = asd(
        &dir,
        &[
            "ledger", "supersede", "svc.charge",
            "--supersede", &old_id,
            "--kind", "assumption",
            "--summary", "amount validated upstream — safe",
        ],
    );
    assert!(o.status.success(), "ledger supersede failed: {}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse supersede output");
    assert_eq!(v["status"].as_str().unwrap(), "superseded");
    assert!(
        v["supersedes"].as_array().map_or(false, |a| a.iter().any(|s| s.as_str() == Some(&old_id))),
        "supersedes list missing old_id; got {v}"
    );

    let new_id = v["entry_id"].as_str().unwrap();
    let entries = read_ledger(&dir, "svc.charge");
    // New superseding entry must appear.
    assert!(
        entries.iter().any(|e| e["entry_id"].as_str() == Some(new_id)),
        "new entry {new_id} missing from read output"
    );
}

#[test]
fn ledger_rebind_reparents_entries() {
    let dir = unique_temp_dir("rebind");
    setup(&dir);

    // Append to `charge`, then rename it to `refund` via rebind.
    let entry_id = ledger_append(&dir, "svc.charge", "decision", "chosen algorithm");

    let o = asd(
        &dir,
        &["ledger", "rebind", "--from", "svc.charge", "--to", "svc.refund"],
    );
    assert!(o.status.success(), "ledger rebind failed: {}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse rebind output");
    assert_eq!(v["status"].as_str().unwrap(), "rebound");
    assert_eq!(v["to_qname"].as_str().unwrap(), "svc.refund");
    // entries_moved may be 0 if the entry already was superseded, but status must be rebound.
    assert!(v["entries_moved"].as_u64().is_some());

    // The old entry should now appear on the new symbol.
    let entries = read_ledger(&dir, "svc.refund");
    assert!(
        entries.iter().any(|e| e["entry_id"].as_str() == Some(&entry_id)),
        "entry {entry_id} not found on svc.refund after rebind; entries: {entries:?}"
    );
}

#[test]
fn ledger_approve_requires_commercial() {
    let dir = unique_temp_dir("approve-gate");
    setup(&dir);
    let entry_id = ledger_append(&dir, "svc.charge", "rationale", "test entry");

    let o = asd(
        &dir,
        &["ledger", "approve", &entry_id, "--approver", "alice@example.com"],
    );
    assert!(!o.status.success(), "expected approve to fail");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("commercial feature"),
        "expected 'commercial feature' in stderr; got: {stderr}"
    );
}

#[test]
fn ledger_reject_requires_commercial() {
    let dir = unique_temp_dir("reject-gate");
    setup(&dir);
    let entry_id = ledger_append(&dir, "svc.charge", "rationale", "test entry");

    let o = asd(
        &dir,
        &[
            "ledger", "reject", &entry_id,
            "--reviewer", "alice@example.com",
            "--reason", "not valid",
        ],
    );
    assert!(!o.status.success(), "expected reject to fail");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("commercial feature"),
        "expected 'commercial feature' in stderr; got: {stderr}"
    );
}

#[test]
fn ledger_withdraw_requires_commercial() {
    let dir = unique_temp_dir("withdraw-gate");
    setup(&dir);
    let entry_id = ledger_append(&dir, "svc.charge", "rationale", "test entry");

    let o = asd(
        &dir,
        &[
            "ledger", "withdraw", &entry_id,
            "--author-id", "tester@example.com",
        ],
    );
    assert!(!o.status.success(), "expected withdraw to fail");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("commercial feature"),
        "expected 'commercial feature' in stderr; got: {stderr}"
    );
}
