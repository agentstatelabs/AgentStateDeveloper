//! Smoke test — run `asd init`, `asd index src`, and `asd read driver.main`
//! against the sample TS repo, and print the outputs so we can eyeball
//! transitive effects across modules.
//!
//! Kept separate from the general `typescript.rs` test so its `eprintln!`
//! output doesn't clutter the normal test run.

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-ts-smoke-{tag}-{id}"));
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
#[ignore = "smoke output; run with --ignored and --nocapture to see summaries"]
fn ts_smoke_shows_transitive_effects() {
    let workdir = unique_temp_dir("smoke");
    copy_ts_tree(&sample_repo(), &workdir);

    let db = workdir.join(".asd-state.db");
    let bin = env!("CARGO_BIN_EXE_asd");

    let init = Command::new(bin)
        .args(["--db", db.to_str().unwrap(), "init"])
        .output()
        .expect("run asd init");
    assert!(init.status.success());
    eprintln!(
        "--- asd init ---\n{}",
        String::from_utf8_lossy(&init.stdout)
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
    assert!(index.status.success());
    eprintln!(
        "--- asd index src ---\n{}",
        String::from_utf8_lossy(&index.stdout)
    );

    let read = Command::new(bin)
        .args(["--db", db.to_str().unwrap(), "read", "driver.main"])
        .output()
        .expect("run asd read");
    assert!(read.status.success());
    eprintln!(
        "--- asd read driver.main ---\n{}",
        String::from_utf8_lossy(&read.stdout)
    );
}
