//! CLI integration tests for `asd verify-effects`.
//!
//! Covers:
//!   - Statically-inferred effects surface as `unverified`
//!   - Symbols with no effects return an empty declared list
//!   - Non-existent symbol returns a non-zero exit code

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-vefx-{tag}-{id}"));
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
    std::fs::write(
        dir.join("svc.py"),
        r#"import os

def write_file(path, content):
    with open(path, 'w') as f:
        f.write(content)

def pure_add(a, b):
    return a + b
"#,
    )
    .unwrap();
    let o = asd(dir, &["init", "--no-hooks"]);
    assert!(
        o.status.success(),
        "init: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let o = asd(dir, &["index", "."]);
    assert!(
        o.status.success(),
        "index: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

// ---------------------------------------------------------------------------

#[test]
fn verify_effects_inferred_effect_is_mismatch() {
    let dir = unique_temp_dir("infer");
    setup(&dir);

    let o = asd(&dir, &["verify-effects", "svc.write_file"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse output");

    assert_eq!(v["qname"].as_str().unwrap(), "svc.write_file");
    // The Python adapter infers svc.write_file's effects and compares them to
    // the declared set; a discrepancy is reported as "mismatch". ("unverified"
    // is reserved for when there's no adapter / unreadable source.)
    assert_eq!(v["status"].as_str().unwrap(), "mismatch");

    let declared = v["declared"].as_array().unwrap();
    assert!(
        !declared.is_empty(),
        "expected at least one declared effect"
    );
}

#[test]
fn verify_effects_pure_symbol_has_empty_declared() {
    let dir = unique_temp_dir("pure");
    setup(&dir);

    let o = asd(&dir, &["verify-effects", "svc.pure_add"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse output");

    assert_eq!(v["qname"].as_str().unwrap(), "svc.pure_add");
    let declared = v["declared"].as_array().unwrap();
    assert!(
        declared.is_empty(),
        "expected no effects for pure function; got {declared:?}"
    );
}

#[test]
fn verify_effects_unknown_symbol_fails() {
    let dir = unique_temp_dir("missing");
    setup(&dir);

    let o = asd(&dir, &["verify-effects", "svc.does_not_exist"]);
    assert!(
        !o.status.success(),
        "expected non-zero exit for unknown symbol; stdout: {}",
        String::from_utf8_lossy(&o.stdout)
    );
}

#[test]
fn verify_effects_result_includes_symbol_id() {
    let dir = unique_temp_dir("sym-id");
    setup(&dir);

    let o = asd(&dir, &["verify-effects", "svc.write_file"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("parse output");

    let sym_id = v["symbol_id"].as_str().unwrap_or("");
    assert!(
        sym_id.starts_with("sym_"),
        "expected symbol_id starting with sym_; got: {sym_id}"
    );
}
