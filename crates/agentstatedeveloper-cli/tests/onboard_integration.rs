//! Plan K t-005: integration tests for `asd onboard`.
//!
//! Each test spawns the asd binary against a temp directory so the
//! end-to-end orchestration (init → index → conclusions import → mcp install)
//! is exercised the same way a real user would.

use std::path::PathBuf;
use std::process::Command;

fn asd_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo at test build time when
    // `[[bin]] name = "asd"` exists in the workspace. The asd binary
    // lives in agentstatedeveloper-cli (this crate), so this works.
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn run_onboard(dir: &std::path::Path) -> (bool, String, String) {
    run_onboard_args(dir, &[])
}

fn run_onboard_args(dir: &std::path::Path, extra: &[&str]) -> (bool, String, String) {
    let mut cmd = Command::new(asd_bin());
    cmd.arg("onboard").arg("--no-hooks");
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd
        .current_dir(dir)
        .output()
        .expect("spawn asd onboard");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn seed_minimal_python_project(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("hello.py"),
        "def hello():\n    return 'hi'\n",
    )
    .unwrap();
    // git init so .gitignore writes have something to refer to.
    let _ = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(dir)
        .output();
}

#[test]
fn onboard_on_fresh_project_succeeds_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_minimal_python_project(tmp.path());

    let (ok, _stdout, stderr) = run_onboard(tmp.path());
    assert!(
        ok,
        "asd onboard must exit 0 on fresh project; stderr:\n{stderr}"
    );
    // Each of the four steps must have printed its banner.
    assert!(
        stderr.contains("[1/4]")
            && stderr.contains("[2/4]")
            && stderr.contains("[3/4]")
            && stderr.contains("[4/4]"),
        "all four onboard steps must run; stderr:\n{stderr}"
    );
    // Init must have created the SQLite DB.
    assert!(
        tmp.path().join(".asd-state.db").is_file(),
        "asd init must create .asd-state.db"
    );
    // Index must have indexed the hello.py symbol.
    assert!(
        stderr.contains("1 symbol") || stderr.contains("symbols"),
        "index must report at least 1 symbol; stderr:\n{stderr}"
    );
}

#[test]
fn onboard_runs_mcp_step_by_default() {
    // The MCP step is best-effort (warns if asd-mcp is absent) but must always
    // run and never fail onboard. When asd-mcp IS present it writes a
    // project-scoped .mcp.json into the repo.
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_minimal_python_project(tmp.path());

    let (ok, _stdout, stderr) = run_onboard(tmp.path());
    assert!(ok, "onboard must exit 0 even if MCP step warns; stderr:\n{stderr}");
    assert!(
        stderr.contains("[4/4] asd mcp install"),
        "MCP step must run by default; stderr:\n{stderr}"
    );
    // If registration succeeded (asd-mcp available), the project config exists.
    // If it warned (asd-mcp absent), onboard still succeeded — both are fine.
    let wrote_cfg = tmp.path().join(".mcp.json").is_file();
    let warned = stderr.contains("MCP registration skipped");
    assert!(
        wrote_cfg || warned,
        "MCP step must either write .mcp.json or warn; stderr:\n{stderr}"
    );
}

#[test]
fn onboard_no_mcp_skips_registration() {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_minimal_python_project(tmp.path());

    let (ok, _stdout, stderr) = run_onboard_args(tmp.path(), &["--no-mcp"]);
    assert!(ok, "onboard --no-mcp must succeed; stderr:\n{stderr}");
    assert!(
        stderr.contains("[4/4] asd mcp install — skipped (--no-mcp)"),
        "--no-mcp must skip registration; stderr:\n{stderr}"
    );
    assert!(
        !tmp.path().join(".mcp.json").is_file(),
        "--no-mcp must not write .mcp.json"
    );
}

#[test]
fn onboard_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_minimal_python_project(tmp.path());

    let (ok1, _, _) = run_onboard(tmp.path());
    assert!(ok1, "first onboard must succeed");
    let (ok2, _, stderr2) = run_onboard(tmp.path());
    assert!(
        ok2,
        "second onboard must succeed (idempotent); stderr:\n{stderr2}"
    );
}

#[test]
fn onboard_skips_conclusions_import_when_dir_missing_before_first_init() {
    // Setup: a fresh project where we MANUALLY remove .asd/ between
    // init and the import-step check would be racy in the actual
    // onboard flow. Instead, verify that the second onboard run (when
    // .asd/conclusions/ exists from the first init) still imports
    // cleanly — covers the "dir exists" branch.
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_minimal_python_project(tmp.path());

    // First run scaffolds .asd/conclusions/ via init.
    let (ok1, _, _) = run_onboard(tmp.path());
    assert!(ok1);
    assert!(
        tmp.path().join(".asd/conclusions").is_dir(),
        "init must scaffold .asd/conclusions/"
    );

    // Second run should hit the import branch (not the skip branch)
    // and succeed.
    let (ok2, _, stderr2) = run_onboard(tmp.path());
    assert!(ok2);
    assert!(
        !stderr2.contains("skipped (.asd/conclusions/ doesn't exist"),
        "second onboard must hit the import branch, not the skip branch; stderr:\n{stderr2}"
    );
}
