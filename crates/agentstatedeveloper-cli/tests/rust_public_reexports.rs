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

#[test]
fn dependency_keys_are_consumer_scoped_and_manifest_reindex_moves_edges() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "crates/api/Cargo.toml",
        "[package]\nname='real-api'\nversion='0.1.0'\n[lib]\nname='public_api'\npath='src/entry.rs'\n",
    );
    write(
        root,
        "crates/api/src/entry.rs",
        "pub mod events; pub use events::record_event as publish;",
    );
    write(root, "crates/api/src/events.rs", "pub fn record_event() {}");
    write(
        root,
        "crates/decoy/Cargo.toml",
        "[package]\nname='decoy-api'\nversion='0.1.0'\n[lib]\nname='decoy_api'\n",
    );
    write(root, "crates/decoy/src/lib.rs", "pub fn publish() {}");
    write(
        root,
        "crates/facade/Cargo.toml",
        "[package]\nname='front'\nversion='0.1.0'\n[lib]\nname='front_api'\n[dependencies]\napi_dep={package='real-api',path='../api'}\n",
    );
    write(
        root,
        "crates/facade/src/lib.rs",
        "pub use api_dep::publish as emit;",
    );
    let consumer = "[package]\nname='consumer'\nversion='0.1.0'\n[dependencies]\napi_dep={package='real-api',path='../api'}\nevent_front={package='front',path='../facade'}\n";
    write(root, "crates/consumer/Cargo.toml", consumer);
    write(
        root,
        "crates/consumer/src/lib.rs",
        "use api_dep::publish as audit; use api_dep as backend; fn bare(){audit();} fn qualified(){api_dep::publish();} fn namespace(){backend::publish();} fn facade(){event_front::emit();}",
    );
    write(
        root,
        "crates/other/Cargo.toml",
        "[package]\nname='other'\nversion='0.1.0'\n[dependencies]\napi_dep={package='decoy-api',path='../decoy'}\n",
    );
    write(
        root,
        "crates/other/src/lib.rs",
        "fn other(){api_dep::publish();}",
    );
    index(root);
    assert_eq!(
        callers(root, "events.record_event"),
        vec![
            "consumer.lib.bare",
            "consumer.lib.facade",
            "consumer.lib.namespace",
            "consumer.lib.qualified"
        ]
    );
    assert_eq!(callers(root, "decoy.lib.publish"), vec!["other.lib.other"]);
    // Only the consumer manifest changes; its source bytes remain untouched.
    write(
        root,
        "crates/consumer/Cargo.toml",
        &consumer.replace(
            "package='real-api',path='../api'",
            "package='decoy-api',path='../decoy'",
        ),
    );
    index(root);
    assert_eq!(
        callers(root, "events.record_event"),
        vec!["consumer.lib.facade"]
    );
    assert_eq!(
        callers(root, "decoy.lib.publish"),
        vec![
            "consumer.lib.bare",
            "consumer.lib.namespace",
            "consumer.lib.qualified",
            "other.lib.other"
        ]
    );
    index(root);
    assert_eq!(
        callers(root, "events.record_event"),
        vec!["consumer.lib.facade"]
    );
    write(root, "crates/consumer/Cargo.toml", consumer);
    index(root);
    assert_eq!(callers(root, "events.record_event").len(), 4);
    assert_eq!(callers(root, "decoy.lib.publish"), vec!["other.lib.other"]);
}

#[test]
fn unsupported_dependency_scopes_do_not_fall_back_to_global_library_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "crates/api/Cargo.toml",
        "[package]\nname='real-api'\nversion='0.1.0'\n[lib]\nname='public_api'\n",
    );
    write(root, "crates/api/src/lib.rs", "pub fn check() {}");
    for (name, table) in [
        (
            "mismatch",
            "[dependencies]\npublic_api={package='wrong',path='../api'}\n",
        ),
        ("inherited", "[dependencies]\npublic_api={workspace=true}\n"),
        ("registry", "[dependencies]\npublic_api='1'\n"),
        (
            "dev",
            "[dev-dependencies]\npublic_api={package='real-api',path='../api'}\n",
        ),
        (
            "build",
            "[build-dependencies]\npublic_api={package='real-api',path='../api'}\n",
        ),
        (
            "target",
            "[target.'cfg(unix)'.dependencies]\npublic_api={package='real-api',path='../api'}\n",
        ),
        (
            "optional",
            "[dependencies]\npublic_api={package='real-api',path='../api',optional=true}\n",
        ),
    ] {
        write(
            root,
            &format!("crates/{name}/Cargo.toml"),
            &format!("[package]\nname='{name}'\nversion='0.1.0'\n{table}"),
        );
        write(
            root,
            &format!("crates/{name}/src/lib.rs"),
            "use public_api::check as inspect; fn qualified(){public_api::check();} fn renamed(){inspect();}",
        );
    }
    index(root);
    assert!(callers(root, "api.lib.check").is_empty());
}

#[test]
fn dependency_paths_outside_index_root_and_normalized_key_collisions_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "crates/api/Cargo.toml",
        "[package]\nname='real-api'\nversion='0.1.0'\n[lib]\nname='public_api'\n",
    );
    write(root, "crates/api/src/lib.rs", "pub fn check() {}");
    write(
        outside.path(),
        "Cargo.toml",
        "[package]\nname='real-api'\nversion='0.1.0'\n[lib]\nname='public_api'\n",
    );
    write(outside.path(), "src/lib.rs", "pub fn check() {}");
    write(
        root,
        "crates/external/Cargo.toml",
        &format!(
            "[package]\nname='external'\nversion='0.1.0'\n[dependencies]\npublic_api={{package='real-api',path='{}'}}\n",
            outside.path().display()
        ),
    );
    write(
        root,
        "crates/external/src/lib.rs",
        "fn external(){public_api::check();}",
    );
    write(
        root,
        "crates/collision/Cargo.toml",
        "[package]\nname='collision'\nversion='0.1.0'\n[dependencies]\npublic-api={package='real-api',path='../api'}\npublic_api={package='wrong',path='../api'}\n",
    );
    write(
        root,
        "crates/collision/src/lib.rs",
        "fn collision(){public_api::check();}",
    );
    index(root);
    assert!(callers(root, "api.lib.check").is_empty());
}
