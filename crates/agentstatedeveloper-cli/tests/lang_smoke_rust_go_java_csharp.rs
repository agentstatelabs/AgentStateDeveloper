//! CLI smoke tests for Rust, Go, Java, and C# language adapters.
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
// Rust
// ---------------------------------------------------------------------------

#[test]
fn rust_indexes_struct_and_impl_method() {
    let dir = unique_temp_dir("rust");
    std::fs::write(
        dir.join("payments.rs"),
        r#"
pub struct PaymentService { api_key: String }

impl PaymentService {
    pub fn charge(&self, amount: f64) -> bool {
        println!("charging {}", amount);
        amount > 0.0
    }
    pub fn refund(&self, amount: f64) -> bool { amount > 0.0 }
}
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
fn rust_infers_log_effect() {
    let dir = unique_temp_dir("rust-fx");
    std::fs::write(
        dir.join("logger.rs"),
        r#"
pub fn log_event(msg: &str) { println!("{}", msg); }
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "logger.log_event");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

#[test]
fn go_indexes_package_functions() {
    let dir = unique_temp_dir("go");
    std::fs::write(
        dir.join("payments.go"),
        r#"package payments

import "fmt"

func ChargeCard(userID string, amount float64) error {
    fmt.Printf("charging %s\n", userID)
    return nil
}

func Refund(userID string, amount float64) error { return nil }
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let idx = index_json(&dir);
    assert!(
        idx["symbols"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 symbols; got {idx}"
    );

    let sym = read_symbol(&dir, "payments.ChargeCard");
    assert_eq!(
        sym["symbol"]["qname"].as_str().unwrap(),
        "payments.ChargeCard"
    );
    assert_eq!(sym["symbol"]["kind"].as_str().unwrap(), "function");
}

#[test]
fn go_infers_log_effect() {
    let dir = unique_temp_dir("go-fx");
    std::fs::write(
        dir.join("svc.go"),
        r#"package svc

import "fmt"

func Run() { fmt.Println("running") }
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "svc.Run");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

#[test]
fn java_indexes_class_and_methods() {
    let dir = unique_temp_dir("java");
    std::fs::write(
        dir.join("PaymentService.java"),
        r#"public class PaymentService {
    public boolean charge(String userId, double amount) {
        System.out.println("charging " + userId);
        return amount > 0;
    }
    public boolean refund(String userId, double amount) { return amount > 0; }
}
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let idx = index_json(&dir);
    assert!(
        idx["symbols"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 symbols; got {idx}"
    );

    let sym = read_symbol(&dir, "PaymentService.charge");
    assert_eq!(sym["symbol"]["kind"].as_str().unwrap(), "method");
}

#[test]
fn java_infers_log_effect() {
    let dir = unique_temp_dir("java-fx");
    std::fs::write(
        dir.join("Logger.java"),
        r#"public class Logger {
    public void log(String msg) { System.out.println(msg); }
}
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "Logger.log");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

#[test]
fn csharp_indexes_class_and_methods() {
    let dir = unique_temp_dir("csharp");
    std::fs::write(
        dir.join("PaymentService.cs"),
        r#"using System;

public class PaymentService {
    public bool Charge(string userId, decimal amount) {
        Console.WriteLine($"charging {userId}");
        return amount > 0;
    }
    public bool Refund(string userId, decimal amount) { return amount > 0; }
}
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let idx = index_json(&dir);
    assert!(
        idx["symbols"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 symbols; got {idx}"
    );

    let sym = read_symbol(&dir, "PaymentService.Charge");
    assert_eq!(sym["symbol"]["kind"].as_str().unwrap(), "method");
}

#[test]
fn csharp_infers_log_effect() {
    let dir = unique_temp_dir("csharp-fx");
    std::fs::write(
        dir.join("Logger.cs"),
        r#"using System;

public class Logger {
    public void Log(string msg) { Console.WriteLine(msg); }
}
"#,
    )
    .unwrap();

    init_and_index(&dir);
    let sym = read_symbol(&dir, "Logger.Log");
    let cats = declared_effect_categories(&sym);
    assert!(
        cats.iter().any(|c| c == "log"),
        "expected log effect; got {cats:?}"
    );
}
