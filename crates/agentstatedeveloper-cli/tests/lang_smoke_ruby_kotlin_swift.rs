//! CLI smoke tests for Ruby, Kotlin, and Swift language adapters.
//!
//! Each test writes a small inline source file, runs `asd index`, and
//! verifies that `asd read` returns a symbol — confirming the adapter is
//! wired up, the file-extension routing works, and the symbol lands in the db.

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-lang-{tag}-{id}"));
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

fn init_and_index(dir: &Path) {
    let o = asd(dir, &["init", "--no-hooks"]);
    assert!(
        o.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let o = asd(dir, &["index", "."]);
    assert!(
        o.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn index_json(dir: &Path) -> serde_json::Value {
    let o = asd(dir, &["index", "."]);
    assert!(
        o.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("parse index output")
}

fn read_symbol(dir: &Path, qname: &str) -> serde_json::Value {
    let o = asd(dir, &["read", qname]);
    assert!(
        o.status.success(),
        "asd read {qname} failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("parse read output")
}

fn declared_effect_categories(sym: &serde_json::Value) -> Vec<String> {
    let empty = vec![];
    sym["effects"]["declared"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|e| e["effect"].as_str().map(String::from))
        .collect()
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

#[test]
fn ruby_indexes_class_and_methods() {
    let dir = unique_temp_dir("ruby");
    std::fs::write(
        dir.join("payments.rb"),
        r#"
class PaymentService
  def charge(user_id, amount)
    puts "charging #{user_id}"
    amount > 0
  end
  def refund(user_id, amount)
    amount > 0
  end
end
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let idx = index_json(&dir);
    assert!(
        idx["symbols"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 symbols; got {idx}"
    );

    let sym = read_symbol(&dir, "payments.PaymentService.charge");
    assert_eq!(
        sym["symbol"]["qname"].as_str().unwrap(),
        "payments.PaymentService.charge"
    );
    assert_eq!(sym["symbol"]["kind"].as_str().unwrap(), "method");
}

#[test]
fn ruby_infers_log_effect() {
    let dir = unique_temp_dir("ruby-fx");
    std::fs::write(
        dir.join("logger.rb"),
        r#"
class Logger
  def log(msg)
    puts msg
  end
end
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "logger.Logger.log");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}

// ---------------------------------------------------------------------------
// Kotlin
// ---------------------------------------------------------------------------

#[test]
fn kotlin_indexes_top_level_functions() {
    let dir = unique_temp_dir("kotlin");
    std::fs::write(
        dir.join("payments.kt"),
        r#"
fun chargeCard(userId: String, amount: Double): Boolean {
    println("charging $userId")
    return amount > 0
}

fun refund(userId: String, amount: Double): Boolean = amount > 0
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let idx = index_json(&dir);
    assert!(
        idx["symbols"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 symbols; got {idx}"
    );

    let sym = read_symbol(&dir, "chargeCard");
    assert_eq!(sym["symbol"]["qname"].as_str().unwrap(), "chargeCard");
    assert_eq!(sym["symbol"]["kind"].as_str().unwrap(), "function");
}

#[test]
fn kotlin_infers_log_effect() {
    let dir = unique_temp_dir("kotlin-fx");
    std::fs::write(
        dir.join("svc.kt"),
        r#"
fun run() { println("running") }
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "run");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}

// ---------------------------------------------------------------------------
// Swift
// ---------------------------------------------------------------------------

#[test]
fn swift_indexes_top_level_functions() {
    let dir = unique_temp_dir("swift");
    std::fs::write(
        dir.join("payments.swift"),
        r#"
func chargeCard(userId: String, amount: Double) -> Bool {
    print("charging \(userId)")
    return amount > 0
}

func refund(userId: String, amount: Double) -> Bool { return amount > 0 }
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let idx = index_json(&dir);
    assert!(
        idx["symbols"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 symbols; got {idx}"
    );

    let sym = read_symbol(&dir, "payments.chargeCard");
    assert_eq!(
        sym["symbol"]["qname"].as_str().unwrap(),
        "payments.chargeCard"
    );
    assert_eq!(sym["symbol"]["kind"].as_str().unwrap(), "function");
}

#[test]
fn swift_infers_log_effect() {
    let dir = unique_temp_dir("swift-fx");
    std::fs::write(
        dir.join("svc.swift"),
        r#"
func run() { print("running") }
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "svc.run");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}
