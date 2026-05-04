//! Tests for M19 git-native sidecar features:
//!   - `asd sync --prune` removes orphaned .asd/v1/ files
//!   - `asd init` installs hook scripts with correct content
//!   - `asd init --no-hooks` skips hook installation
//!   - `asd hooks` reports installed/missing status

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_temp_dir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-hooks-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn asd_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // debug/
    p.push("asd");
    p
}

fn run_asd(dir: &Path, args: &[&str]) -> std::process::Output {
    let db = dir.join(".asd-state.db");
    Command::new(asd_bin())
        .args(["--db", db.to_str().unwrap()])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run asd")
}

fn copy_py_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dst");
    for entry in std::fs::read_dir(src).expect("read sample dir") {
        let entry = entry.expect("dir entry");
        let p = entry.path();
        let ft = entry.file_type().expect("file type");
        let target = dst.join(p.file_name().unwrap());
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == ".asd" || name == ".asd-state.db" {
            continue;
        }
        if ft.is_dir() {
            copy_py_tree(&p, &target);
        } else if ft.is_file() {
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(ext, "py" | "md" | "txt" | "json") {
                std::fs::copy(&p, &target).expect("copy file");
            }
        }
    }
}

fn sample_py_repo() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/sample-py-repo")
}

// ---------------------------------------------------------------------------

#[test]
fn sync_prune_removes_orphan_files() {
    let dir = unique_temp_dir("prune");
    copy_py_tree(&sample_py_repo(), &dir);

    // init + index + sync
    assert!(run_asd(&dir, &["init", "--no-hooks"]).status.success());
    assert!(run_asd(&dir, &["index", "."]).status.success());
    assert!(run_asd(&dir, &["sync"]).status.success());

    // Plant an orphan effects file for a fake symbol_id.
    let orphan = dir.join(".asd/v1/effects/sym_orphan_fake_9999.json");
    std::fs::write(
        &orphan,
        r#"{"symbol_id":"sym_orphan_fake_9999","effects":[],"verification":{"by":"static-checker","status":"unverified"}}"#,
    )
    .expect("write orphan");
    assert!(orphan.exists(), "orphan should exist before prune");

    // sync --prune should remove it.
    let out = run_asd(&dir, &["sync", "--prune"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse sync output");
    assert!(
        json["pruned"].as_u64().unwrap_or(0) >= 1,
        "expected pruned >= 1, got: {json}"
    );
    assert!(!orphan.exists(), "orphan should be gone after prune");
}

#[test]
fn init_installs_hook_scripts() {
    let dir = unique_temp_dir("init-hooks");
    copy_py_tree(&sample_py_repo(), &dir);

    // init without --no-hooks (default)
    let out = run_asd(&dir, &["init"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let stdout = String::from_utf8_lossy(&out.stdout);

    // Hook scripts should exist on disk.
    for hook in &["pre-commit", "post-merge", "post-checkout"] {
        let path = dir.join(".asd/hooks").join(hook);
        assert!(path.exists(), "hook script missing: {hook}");

        // Must be executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "{hook} is not executable");
        }
    }

    // Output should describe each hook.
    assert!(stdout.contains("pre-commit"), "init output missing pre-commit");
    assert!(stdout.contains("post-merge"), "init output missing post-merge");
    assert!(stdout.contains("post-checkout"), "init output missing post-checkout");
    assert!(stdout.contains("asd sync --prune"), "init output missing sync command");
    assert!(stdout.contains("asd hydrate"), "init output missing hydrate command");
    assert!(stdout.contains("--no-hooks"), "init output missing opt-out hint");
}

#[test]
fn init_no_hooks_skips_installation() {
    let dir = unique_temp_dir("no-hooks");
    copy_py_tree(&sample_py_repo(), &dir);

    let out = run_asd(&dir, &["init", "--no-hooks"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Hook scripts should NOT exist.
    for hook in &["pre-commit", "post-merge", "post-checkout"] {
        let path = dir.join(".asd/hooks").join(hook);
        assert!(!path.exists(), "hook should not exist with --no-hooks: {hook}");
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("skipped"), "expected 'skipped' in --no-hooks output");
}

#[test]
fn hooks_subcommand_reports_status() {
    let dir = unique_temp_dir("hooks-cmd");
    copy_py_tree(&sample_py_repo(), &dir);

    // Before init — hooks missing.
    assert!(run_asd(&dir, &["init", "--no-hooks"]).status.success());
    let out = run_asd(&dir, &["hooks"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pre-commit"), "hooks output missing pre-commit");
    assert!(stdout.contains("post-merge"), "hooks output missing post-merge");

    // After init with hooks — all installed.
    assert!(run_asd(&dir, &["init"]).status.success());
    let out2 = run_asd(&dir, &["hooks"]);
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains('✓'), "expected ✓ after hooks installed");
}

#[test]
fn init_updates_gitignore() {
    let dir = unique_temp_dir("gitignore");
    copy_py_tree(&sample_py_repo(), &dir);

    // Write a .gitignore that currently ignores .asd/ entirely.
    std::fs::write(dir.join(".gitignore"), ".asd/\n").expect("write .gitignore");

    assert!(run_asd(&dir, &["init", "--no-hooks"]).status.success());

    let content = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
    assert!(
        content.contains(".asd-state.db"),
        ".gitignore should contain .asd-state.db"
    );
    // The blanket .asd/ ignore should have been removed.
    assert!(
        !content.lines().any(|l| l.trim() == ".asd/" || l.trim() == ".asd"),
        ".gitignore should not blanket-ignore .asd/; got:\n{content}"
    );
}
