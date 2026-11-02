//! Real index/query regression for the public re-export caller gap found in Crucible.
use std::{fs, path::Path, process::Command};
fn write(root: &Path, path: &str, body: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, body).unwrap();
}
fn index(root: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_asd"))
        .args([
            "--db",
            root.join(".asd.db").to_str().unwrap(),
            "index",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "index failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn callers(root: &Path, target: &str) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_asd"))
        .args([
            "--db",
            root.join(".asd.db").to_str().unwrap(),
            "callers",
            target,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut names: Vec<_> = value["callers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["qname"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}
#[test]
fn cargo_public_name_reexports_bind_callers_and_reindex_moves_and_removes_edges() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "crates/folder/Cargo.toml",
        "[package]\nname='different-package'\nversion='0.1.0'\n[lib]\nname='public_api'\npath='src/entry.rs'\n",
    );
    write(
        root,
        "crates/folder/src/entry.rs",
        "pub mod doctor; pub use doctor::{run_doctor, other};",
    );
    write(
        root,
        "crates/folder/src/doctor.rs",
        "pub fn run_doctor() {} pub fn other() {}",
    );
    write(
        root,
        "crates/http/src/lib.rs",
        "fn handler() { public_api::run_doctor(); }",
    );
    write(
        root,
        "crates/mcp/src/lib.rs",
        "struct Mcp; impl Mcp { fn doctor(&self) { public_api::run_doctor(); } }",
    );
    write(
        root,
        "crates/alias/src/lib.rs",
        "use public_api::run_doctor as inspect; use public_api as api; fn handler() { inspect(); } fn module_handler() { api::run_doctor(); }",
    );
    index(root);
    let expected = vec![
        "alias.lib.handler",
        "alias.lib.module_handler",
        "http.lib.handler",
        "mcp.lib.Mcp.doctor",
    ];
    assert_eq!(callers(root, "doctor.run_doctor"), expected);
    // Changing the re-export must move incoming edges, not leave stale ones behind.
    write(
        root,
        "crates/folder/src/entry.rs",
        "pub mod doctor; pub use doctor::other as run_doctor;",
    );
    index(root);
    assert!(callers(root, "doctor.run_doctor").is_empty());
    assert_eq!(callers(root, "doctor.other"), expected);
    // Repeated indexing is stable, then removing a call removes that edge.
    index(root);
    assert_eq!(callers(root, "doctor.other"), expected);
    write(root, "crates/http/src/lib.rs", "fn handler() {}");
    index(root);
    assert_eq!(
        callers(root, "doctor.other"),
        vec![
            "alias.lib.handler",
            "alias.lib.module_handler",
            "mcp.lib.Mcp.doctor"
        ]
    );
}
