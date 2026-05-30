//! End-to-end smoke test for `asd index` call-graph extraction.
//!
//! Copies the sample Python repo to a temp dir, runs `asd init` and
//! `asd index .` via the compiled binary, then opens the resulting SQLite
//! through the public `Engine` / `AsgIndexStore` API and asserts that at
//! least one resolved call edge exists for a known qname.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{AsgIndexStore, Engine, IndexStore};

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-call-graph-{tag}-{id}"));
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
    // CARGO_MANIFEST_DIR points at crates/agentstatedeveloper-cli; the
    // repo root is two levels up.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("examples").join("sample-py-repo"))
        .expect("locate sample-py-repo")
}

#[test]
fn index_extracts_at_least_one_call_edge() {
    let workdir = unique_temp_dir("idx");
    copy_py_files(&sample_repo(), &workdir);

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
        stdout.contains("\"edges\""),
        "summary missing edges field: {stdout}"
    );

    // Open the same DB via the public engine surface and confirm we can
    // read at least one call edge for a known target. We try a few likely
    // candidates and accept the first one with a non-empty result.
    let engine = Engine::open_sqlite(&db).expect("open engine");
    let store = AsgIndexStore::new(&engine.repo);

    let candidates = [
        // pipeline.py is engineered to have several intra-module call edges
        // (free functions calling each other, methods using `self.helper()`).
        "pipeline.format_label",
        "pipeline.Pipeline.label_for",
        "pipeline.Pipeline.__init__",
        "pipeline.normalize",
        // Older sample modules — kept as fallbacks even though most of their
        // calls are cross-module / dynamic and therefore not resolvable.
        "payments.Payment.__init__",
        "payments.Payment.refund",
        "payments.charge_card",
        "payments.get_balance",
        "logger.write_log",
    ];

    let mut found_any = false;
    for qname in candidates {
        let Some(sym) = store
            .get_symbol_by_qname(&engine.ref_name, qname)
            .expect("qname lookup")
        else {
            continue;
        };
        let callees = store
            .get_callees(&engine.ref_name, &sym.symbol_id)
            .expect("get_callees");
        let callers = store
            .get_callers(&engine.ref_name, &sym.symbol_id)
            .expect("get_callers");
        if !callees.is_empty() || !callers.is_empty() {
            found_any = true;
            eprintln!("edge observed for {qname}: callees={callees:?} callers={callers:?}");
            break;
        }
    }

    assert!(
        found_any,
        "expected at least one call edge for a known sample symbol, got none. asd index stdout: {stdout}"
    );
}
