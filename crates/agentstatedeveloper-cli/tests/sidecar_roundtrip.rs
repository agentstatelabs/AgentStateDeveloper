//! End-to-end round-trip test for the `.asd/v1/` on-disk sidecar.
//!
//! Runs the `asd` CLI through an init / index / ledger append / sync /
//! delete-db / init / hydrate cycle against a copy of the sample Python
//! repo and asserts the ledger entry survives the round-trip — the core
//! "git clone on a fresh machine" promise from DESIGN.md.

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-sidecar-e2e-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn copy_py_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst");
    for entry in std::fs::read_dir(src).expect("read sample dir") {
        let entry = entry.expect("dir entry");
        let p = entry.path();
        let ft = entry.file_type().expect("file type");
        let target = dst.join(p.file_name().unwrap());
        // Skip any stray .asd-state.db or .asd/ from prior runs.
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == ".asd" || name == ".asd-state.db" {
            continue;
        }
        if ft.is_dir() {
            copy_py_tree(&p, &target);
        } else if ft.is_file() {
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(ext, "py" | "md" | "txt" | "json") {
                std::fs::copy(&p, &target).expect("copy py file");
            }
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

fn dump_tree(dir: &Path, depth: usize) {
    if !dir.is_dir() {
        return;
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        let prefix = "  ".repeat(depth);
        if p.is_dir() {
            eprintln!("{prefix}{name}/");
            dump_tree(&p, depth + 1);
        } else {
            let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            eprintln!("{prefix}{name} ({sz} bytes)");
        }
    }
}

fn run_asd(bin: &str, db: &Path, dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir);
    cmd.args(["--db", db.to_str().unwrap()]);
    cmd.args(args);
    let out = cmd.output().expect("run asd");
    if !out.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&out.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    }
    out
}

#[test]
fn sidecar_roundtrip_preserves_ledger_and_effects() {
    let workdir = unique_temp_dir("roundtrip");
    copy_py_tree(&sample_repo(), &workdir);

    let db = workdir.join(".asd-state.db");
    let bin = env!("CARGO_BIN_EXE_asd");

    // 1. init + index.
    assert!(run_asd(bin, &db, &workdir, &["init"]).status.success());
    assert!(
        run_asd(bin, &db, &workdir, &["index", workdir.to_str().unwrap()])
            .status
            .success()
    );

    // 2. Append a hazard entry.
    let append = run_asd(
        bin,
        &db,
        &workdir,
        &[
            "ledger",
            "append",
            "payments.charge_card",
            "--kind",
            "hazard",
            "--summary",
            "needs review",
            "--author-id",
            "alice",
            "--author-kind",
            "human",
        ],
    );
    assert!(append.status.success(), "ledger append failed");

    // 3. Capture pre-sync read.
    let pre = run_asd(bin, &db, &workdir, &["read", "payments.charge_card"]);
    assert!(pre.status.success(), "read before sync failed");
    let pre_stdout = String::from_utf8_lossy(&pre.stdout).to_string();
    assert!(
        pre_stdout.contains("needs review"),
        "expected pre-sync read to contain hazard: {pre_stdout}"
    );

    // 4. Sync to sidecar.
    let sync = run_asd(bin, &db, &workdir, &["sync"]);
    assert!(sync.status.success(), "sync failed");
    let sidecar_root = workdir.join(".asd/v1");
    assert!(sidecar_root.join("meta/schema-version").is_file());
    assert!(sidecar_root.join("effects").is_dir());
    assert!(sidecar_root.join("ledger").is_dir());
    assert!(sidecar_root.join("symbols").is_dir());
    // At least one effect file should have been written.
    let effect_files: Vec<_> = std::fs::read_dir(sidecar_root.join("effects"))
        .unwrap()
        .flatten()
        .collect();
    assert!(!effect_files.is_empty(), "no effect files written");
    eprintln!(
        "--- sync stdout ---\n{}",
        String::from_utf8_lossy(&sync.stdout)
    );
    eprintln!("--- .asd/v1 file listing ---");
    dump_tree(&sidecar_root, 0);

    // 5. Simulate fresh clone: delete the SQLite db, keep `.asd/` on disk.
    std::fs::remove_file(&db).expect("delete db");
    assert!(!db.exists());

    // 6. Re-init + hydrate.
    assert!(run_asd(bin, &db, &workdir, &["init"]).status.success());
    let hydrate = run_asd(bin, &db, &workdir, &["hydrate"]);
    assert!(
        hydrate.status.success(),
        "hydrate failed: {}",
        String::from_utf8_lossy(&hydrate.stderr)
    );

    // 7. Confirm the ledger hazard survived.
    let post = run_asd(bin, &db, &workdir, &["read", "payments.charge_card"]);
    assert!(
        post.status.success(),
        "read after hydrate failed: {}",
        String::from_utf8_lossy(&post.stderr)
    );
    let post_stdout = String::from_utf8_lossy(&post.stdout).to_string();
    eprintln!("--- post-hydrate `asd read payments.charge_card` ---\n{post_stdout}");
    assert!(
        post_stdout.contains("needs review"),
        "hazard summary should survive round-trip. Got: {post_stdout}"
    );
    assert!(
        post_stdout.contains("\"kind\": \"hazard\""),
        "hazard kind should survive round-trip. Got: {post_stdout}"
    );
    assert!(
        post_stdout.contains("payments.charge_card"),
        "symbol qname should survive round-trip. Got: {post_stdout}"
    );
    // Effects survive too — the charge_card function has known declared
    // effects from the Python adapter.
    assert!(
        post_stdout.contains("io.db.write") || post_stdout.contains("log"),
        "declared effects should survive round-trip. Got: {post_stdout}"
    );
}

#[test]
fn hydrate_errors_without_sidecar() {
    let workdir = unique_temp_dir("no-sidecar");
    let db = workdir.join(".asd-state.db");
    let bin = env!("CARGO_BIN_EXE_asd");

    assert!(run_asd(bin, &db, &workdir, &["init"]).status.success());
    let out = run_asd(bin, &db, &workdir, &["hydrate"]);
    assert!(!out.status.success(), "hydrate should fail without sidecar");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("asd sync"),
        "expected friendly hint pointing at `asd sync`. Got: {combined}"
    );
}
