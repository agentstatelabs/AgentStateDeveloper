//! Integration test for the TypeScript adapter end-to-end.
//!
//! Copies the sample TypeScript repo to a temp dir, runs `asd init` and
//! `asd index <dir>` via the compiled binary, then opens the resulting
//! SQLite through the public `Engine` / `AsgIndexStore` API and asserts:
//! - at least one TypeScript symbol got indexed
//! - `driver.main` has at least one cross-module callee (`logger.writeLog`,
//!   `payments.chargeCard`, or `greetings.hello`).

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{AsgIndexStore, Engine, IndexStore};

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-ts-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn copy_ts_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst");
    for entry in std::fs::read_dir(src).expect("read sample dir") {
        let entry = entry.expect("dir entry");
        let p = entry.path();
        let ft = entry.file_type().expect("file type");
        let target = dst.join(p.file_name().unwrap());
        if ft.is_dir() {
            copy_ts_tree(&p, &target);
        } else if ft.is_file() {
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(ext, "ts" | "tsx" | "mts" | "cts" | "json" | "md") {
                std::fs::copy(&p, &target).expect("copy ts file");
            }
        }
    }
}

fn sample_repo() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("examples").join("sample-ts-repo"))
        .expect("locate sample-ts-repo")
}

#[test]
fn index_typescript_sample_produces_symbols_and_cross_module_edges() {
    let workdir = unique_temp_dir("idx");
    copy_ts_tree(&sample_repo(), &workdir);

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

    let src_dir = workdir.join("src");
    let index = Command::new(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "index",
            src_dir.to_str().unwrap(),
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
        stdout.contains("\"symbols\""),
        "summary missing symbols field: {stdout}"
    );

    let engine = Engine::open_sqlite(&db).expect("open engine");
    let store = AsgIndexStore::new(&engine.repo);

    // Sanity: at least one TS symbol showed up.
    let hello = store
        .get_symbol_by_qname(&engine.ref_name, "greetings.hello")
        .expect("qname lookup");
    assert!(
        hello.is_some(),
        "expected greetings.hello to be indexed; got None",
    );

    // driver.main should have callees that include at least one of our
    // three cross-module targets.
    let main_sym = store
        .get_symbol_by_qname(&engine.ref_name, "driver.main")
        .expect("qname lookup")
        .expect("driver.main must exist");
    let callee_ids = store
        .get_callees(&engine.ref_name, &main_sym.symbol_id)
        .expect("get_callees");
    assert!(
        !callee_ids.is_empty(),
        "expected driver.main to have at least one callee, got none",
    );

    let expected_any = ["logger.writeLog", "payments.chargeCard", "greetings.hello"];
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
        "driver.main callee ids {callee_ids:?} did not include any of {expected_any:?}",
    );
    eprintln!(
        "cross-module ts edge observed: driver.main -> {}",
        hit.unwrap()
    );
}
