//! Integration test for cross-module call-edge resolution.
//!
//! Copies the sample Python repo to a temp dir, runs `asd init` and
//! `asd index .` via the compiled binary, then opens the resulting SQLite
//! through the public `Engine` / `AsgIndexStore` API and asserts that the
//! `_driver.main`-style driver symbols have at least one callee that
//! resolves across module boundaries (e.g., `logger.write_log` or
//! `payments.charge_card`).

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{AsgIndexStore, Engine, IndexStore};

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-cross-module-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn copy_py_files(src: &Path, dst: &Path) {
    for entry in std::fs::read_dir(src).expect("read sample dir") {
        let entry = entry.expect("dir entry");
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("py") {
            let target = dst.join(p.file_name().unwrap());
            std::fs::copy(&p, &target).expect("copy python file");
        }
    }
}

fn sample_repo() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("examples").join("sample-py-repo"))
        .expect("locate sample-py-repo")
}

/// Drop an extra driver into the temp workdir that wraps the cross-module
/// calls inside a function (the existing `_driver.py` has them at module
/// scope, which isn't captured as a symbol body).
fn write_cross_module_driver(dir: &Path) {
    let body = r#"import payments
import logger
from greetings import hello


def main():
    logger.write_log("/tmp/asd-trace-demo.log", "hi")
    payments.charge_card("alice", 10.0)
    hello("world")
"#;
    std::fs::write(dir.join("driver_wrapper.py"), body).expect("write driver_wrapper.py");
}

#[test]
fn index_resolves_cross_module_call_edges() {
    let workdir = unique_temp_dir("idx");
    copy_py_files(&sample_repo(), &workdir);
    write_cross_module_driver(&workdir);

    let db = workdir.join(".asd-state.db");
    let bin = env!("CARGO_BIN_EXE_asd");

    let init = Command::new(bin)
        .args(["--db", db.to_str().unwrap(), "init"])
        .output()
        .expect("run asd init");
    assert!(
        init.status.success(),
        "asd init failed: stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );

    let index = Command::new(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "index",
            workdir.to_str().unwrap(),
        ])
        .output()
        .expect("run asd index");
    assert!(
        index.status.success(),
        "asd index failed: stdout={} stderr={}",
        String::from_utf8_lossy(&index.stdout),
        String::from_utf8_lossy(&index.stderr),
    );
    let stdout = String::from_utf8_lossy(&index.stdout).to_string();
    assert!(
        stdout.contains("\"cross_module_edges\""),
        "summary missing cross_module_edges field: {stdout}"
    );

    let engine = Engine::open_sqlite(&db).expect("open engine");
    let store = AsgIndexStore::new(&engine.repo);

    // Look at `driver_wrapper.main` (our synthesized driver function) —
    // every call in its body is cross-module.
    let main_sym = store
        .get_symbol_by_qname(&engine.ref_name, "driver_wrapper.main")
        .expect("qname lookup")
        .expect("driver_wrapper.main must exist");
    let callee_ids = store
        .get_callees(&engine.ref_name, &main_sym.symbol_id)
        .expect("get_callees");
    assert!(
        !callee_ids.is_empty(),
        "expected driver_wrapper.main to have at least one callee, got none",
    );

    // Look up the symbol_ids of the expected cross-module targets and
    // assert at least one appears in the callees list.
    let expected_any = [
        "logger.write_log",
        "payments.charge_card",
        "greetings.hello",
    ];
    let mut hit: Option<String> = None;
    for q in expected_any {
        if let Some(sym) = store
            .get_symbol_by_qname(&engine.ref_name, q)
            .expect("qname lookup")
        {
            if callee_ids.contains(&sym.symbol_id) {
                hit = Some(q.to_string());
                break;
            }
        }
    }
    assert!(
        hit.is_some(),
        "driver_wrapper.main callee ids {callee_ids:?} did not include any of {expected_any:?}",
    );
    eprintln!(
        "cross-module edge observed: driver_wrapper.main -> {}",
        hit.unwrap()
    );
}
