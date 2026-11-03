//! Actual resolver route, source/manifest drift, and compiler-confirmed rebind.
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
fn write(root: &Path, file: &str, text: &str) {
    let p = root.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}
fn run(root: &Path, args: &[&str]) -> (Value, usize) {
    let r = Command::new(env!("CARGO_BIN_EXE_asd"))
        .current_dir(root)
        .args(["--db", root.join(".asd.db").to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    (serde_json::from_slice(&r.stdout).unwrap(), r.stdout.len())
}
fn index(root: &Path) {
    run(root, &["index", root.to_str().unwrap()]);
}
fn context(root: &Path, target: &str, extra: &[&str]) -> (Value, usize) {
    let mut args = vec!["context-for", target, "--include-binding-paths", "--quiet"];
    args.extend(extra);
    run(root, &args)
}
fn site(v: &Value) -> &Value {
    &v["symbols"][0]["callers"][0]["call_sites"]["sites"][0]
}
fn fixture(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers=['crates/api','crates/decoy','crates/facade','crates/caller']\nresolver='2'\n",
    );
    write(
        root,
        "crates/api/Cargo.toml",
        "[package]\nname='api'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        root,
        "crates/api/src/lib.rs",
        "pub fn record_event() -> bool { true }\npub use record_event as publish;\n",
    );
    write(
        root,
        "crates/decoy/Cargo.toml",
        "[package]\nname='decoy'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(
        root,
        "crates/decoy/src/lib.rs",
        "pub fn publish() -> bool { false }\n",
    );
    write(
        root,
        "crates/facade/Cargo.toml",
        "[package]\nname='front'\nversion='0.1.0'\nedition='2021'\n[lib]\nname='front_api'\n[dependencies]\napi_dep={package='api',path='../api'}\ndecoy_dep={package='decoy',path='../decoy'}\n",
    );
    write(
        root,
        "crates/facade/src/lib.rs",
        "pub use api_dep::publish as emit;\n",
    );
    write(
        root,
        "crates/caller/Cargo.toml",
        "[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nevent_front={package='front',path='../facade'}\n",
    );
    write(
        root,
        "crates/caller/src/lib.rs",
        "use event_front::emit as send;\npub fn run() -> bool { send() }\n",
    );
    index(root);
}
fn compiled(root: &Path, temp: &Path) -> bool {
    let runtime = temp.join("runtime");
    let dep = root.join("crates/caller");
    write(
        &runtime,
        "Cargo.toml",
        &format!(
            "[workspace]\n[package]\nname='binding-gold'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nconsumer={{path={}}}\n",
            json!(dep.to_str().unwrap())
        ),
    );
    write(
        &runtime,
        "src/main.rs",
        "fn main(){println!(\"{}\",consumer::run());}\n",
    );
    let r = Command::new("cargo")
        .args([
            "run",
            "--offline",
            "--quiet",
            "--manifest-path",
            runtime.join("Cargo.toml").to_str().unwrap(),
        ])
        .env("CARGO_TARGET_DIR", temp.join("target"))
        .env("RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    String::from_utf8(r.stdout).unwrap().trim() == "true"
}
#[test]
fn actual_import_cargo_export_route_has_deduplicated_provenance_and_bounded_delivery() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");
    fixture(&root);
    let (v, _) = context(&root, "api.lib.record_event", &[]);
    let path = &site(&v)["binding_path"];
    assert_eq!(path["status"], "verified_indexed_path");
    assert_eq!(path["target"], "api.lib.record_event");
    let steps = path["steps"].as_array().unwrap();
    assert_eq!(steps[0]["kind"], "import");
    assert_eq!(steps[0]["from"], "send");
    assert_eq!(steps[0]["to"], "event_front.emit");
    assert!(
        steps
            .iter()
            .any(|s| s["kind"] == "cargo_dependency" && s["to"] == "front_api.emit")
    );
    assert!(steps.iter().any(|s| s["kind"] == "reference_alias"
        && s["from"] == "front_api.emit"
        && s["to"] == "api.publish"));
    assert_eq!(steps.last().unwrap()["kind"], "definition");
    let evidence = path["evidence"].as_array().unwrap();
    for step in steps {
        for id in step["provenance"].as_array().unwrap() {
            assert!(id.as_u64().unwrap() < evidence.len() as u64);
        }
    }
    for e in evidence {
        assert_eq!(e["source"]["index_match"], "matched");
        assert!(root.join(e["span"]["file"].as_str().unwrap()).is_file());
    }
    assert!(
        evidence
            .iter()
            .any(|e| e["span"]["file"] == "crates/facade/Cargo.toml")
    );
    assert!(
        evidence
            .iter()
            .any(|e| e["span"]["file"] == "crates/api/src/lib.rs")
    );
    let (filtered, len) = context(
        &root,
        "api.lib.record_event",
        &[
            "--caller-qname",
            "caller.lib.run",
            "--budget-tokens",
            "2000",
        ],
    );
    assert!(len <= 8000);
    assert_eq!(
        site(&filtered)["binding_path"]["status"],
        "verified_indexed_path"
    );
    let (missing, _) = context(
        &root,
        "api.lib.record_event",
        &["--caller-qname", "does.not.exist"],
    );
    assert_eq!(missing["symbols"][0]["caller_filter"]["indexed_total"], 1);
    assert_eq!(missing["symbols"][0]["caller_filter"]["returned"], 0);
    let (omitted, _) = context(
        &root,
        "api.lib.record_event",
        &["--binding-step-limit", "1"],
    );
    assert_eq!(site(&omitted)["binding_path"]["status"], "omitted");
    assert!(site(&omitted)["binding_path"].get("steps").is_none());
    for budget in [128, 256, 512, 1024] {
        let (_, bytes) = context(
            &root,
            "api.lib.record_event",
            &["--budget-tokens", &budget.to_string()],
        );
        assert!(bytes <= budget * 4);
    }
    // Independently compiled binding, then source-only facade rebind to decoy.
    assert!(compiled(&root, tmp.path()));
    write(
        &root,
        "crates/facade/src/lib.rs",
        "pub use decoy_dep::publish as emit;\n",
    );
    assert!(!compiled(&root, tmp.path()));
    let (stale, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(site(&stale)["status"], "unavailable");
    assert_eq!(site(&stale)["binding_path"]["status"], "mismatch");
    assert!(site(&stale).get("expression").is_none());
    assert!(site(&stale)["binding_path"].get("evidence").is_none());
    index(&root);
    assert_eq!(
        context(&root, "api.lib.record_event", &[]).0["symbols"][0]["callers"],
        json!([])
    );
    let (rebound, _) = context(&root, "decoy.lib.publish", &[]);
    assert_eq!(
        site(&rebound)["binding_path"]["status"],
        "verified_indexed_path"
    );
    assert!(
        site(&rebound)["binding_path"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("decoy_dep::publish")))
    );
}
#[test]
fn manifest_or_target_drift_invalidates_path_without_changing_caller_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");
    fixture(&root);
    let caller = fs::read(root.join("crates/caller/src/lib.rs")).unwrap();
    for file in [
        "crates/caller/Cargo.toml",
        "crates/facade/Cargo.toml",
        "crates/api/Cargo.toml",
        "crates/api/src/lib.rs",
    ] {
        let path = root.join(file);
        let old = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{old}\n")).unwrap();
        let (stale, _) = context(&root, "api.lib.record_event", &[]);
        assert_eq!(site(&stale)["binding_path"]["status"], "mismatch", "{file}");
        assert!(site(&stale).get("expression").is_none());
        assert!(site(&stale)["binding_path"].get("evidence").is_none());
        assert_eq!(
            fs::read(root.join("crates/caller/src/lib.rs")).unwrap(),
            caller
        );
        fs::write(path, old).unwrap();
    }
    assert_eq!(
        site(&context(&root, "api.lib.record_event", &[]).0)["binding_path"]["status"],
        "verified_indexed_path"
    );
    // Missing manifest: fail closed, then restoring bytes recovers the snapshot.
    let path = root.join("crates/api/Cargo.toml");
    let old = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let (missing, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(site(&missing)["binding_path"]["status"], "unavailable");
    fs::write(path, old).unwrap();
}

#[test]
fn identical_dependency_keys_keep_distinct_evidence_scopes_and_legacy_is_explicit() {
    use agentstategraph::CommitOptions;
    use agentstategraph_core::IntentCategory;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("fixture");
    fixture(&root);
    write(
        &root,
        "crates/other/Cargo.toml",
        "[package]\nname='other'\nversion='0.1.0'\n[dependencies]\nevent_front={package='decoy',path='../decoy'}\n",
    );
    write(
        &root,
        "crates/other/src/lib.rs",
        "use event_front::publish as send; pub fn run() -> bool { send() }\n",
    );
    index(&root);
    let (real, _) = context(&root, "api.lib.record_event", &[]);
    let (decoy, _) = context(&root, "decoy.lib.publish", &[]);
    assert!(
        site(&real)["binding_path"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "cargo_dependency" && s["to"] == "front_api.emit")
    );
    assert!(
        site(&decoy)["binding_path"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "cargo_dependency" && s["to"] == "decoy.publish")
    );
    let proofs = site(&decoy)["binding_path"]["evidence"].as_array().unwrap();
    assert!(
        proofs
            .iter()
            .any(|p| p["span"]["file"] == "crates/other/Cargo.toml")
    );
    assert!(
        !proofs
            .iter()
            .any(|p| p["span"]["file"] == "crates/caller/Cargo.toml")
    );
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    let path = agentstatedeveloper_core::paths::call_sites_root();
    let mut records = engine.repo.get_tree(&engine.ref_name, &path).unwrap();
    let target = real["symbols"][0]["symbol"]["symbol_id"].as_str().unwrap();
    let sites = records[target]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_array_mut()
        .unwrap();
    sites[0]["binding_path"]["complete"] = json!(false);
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &path,
            &records,
            CommitOptions::new("test", IntentCategory::Refine, "incomplete path"),
        )
        .unwrap();
    drop(engine);
    let (incomplete, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(site(&incomplete)["binding_path"]["status"], "incomplete");
    assert!(site(&incomplete)["binding_path"].get("steps").is_none());
    records[target]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .as_array_mut()
        .unwrap()[0]
        .as_object_mut()
        .unwrap()
        .remove("binding_path");
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &path,
            &records,
            CommitOptions::new("test", IntentCategory::Refine, "legacy site"),
        )
        .unwrap();
    drop(engine);
    assert_eq!(
        site(&context(&root, "api.lib.record_event", &[]).0)["binding_path"]["status"],
        "not_recorded"
    );
}

#[test]
fn inherited_workspace_manifest_is_provenance_and_drift_suppresses_then_rebinds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");
    fixture(&root);
    let workspace = "[workspace]\nmembers=['crates/api','crates/decoy','crates/facade','crates/caller']\nresolver='2'\n[workspace.dependencies]\nevent_front={package='front',path='crates/facade'}\n";
    write(&root, "Cargo.toml", workspace);
    write(
        &root,
        "crates/caller/Cargo.toml",
        "[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nevent_front={workspace=true}\n",
    );
    index(&root);
    let (fresh, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(
        site(&fresh)["binding_path"]["status"],
        "verified_indexed_path"
    );
    assert!(
        site(&fresh)["binding_path"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["span"]["file"] == "Cargo.toml")
    );
    let rebound = workspace.replace(
        "package='front',path='crates/facade'",
        "package='decoy',path='crates/decoy'",
    );
    // Give the alternate facade the same exported entry point.
    write(
        &root,
        "crates/decoy/src/lib.rs",
        "pub fn publish() -> bool { false } pub use publish as emit;",
    );
    // Reindex this source change before testing manifest-only drift.
    index(&root);
    write(&root, "Cargo.toml", &rebound);
    let (stale, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(site(&stale)["binding_path"]["status"], "mismatch");
    assert!(site(&stale).get("expression").is_none());
    assert!(site(&stale)["binding_path"].get("evidence").is_none());
    index(&root);
    let (old, _) = context(&root, "api.lib.record_event", &[]);
    assert!(old["symbols"][0]["callers"].as_array().unwrap().is_empty());
    let (new, _) = context(&root, "decoy.lib.publish", &[]);
    assert_eq!(
        site(&new)["binding_path"]["status"],
        "verified_indexed_path"
    );
}

#[test]
fn file_module_reexport_provenance_drift_rebind_cycles_and_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");
    fixture(&root);
    write(
        &root,
        "crates/caller/src/lib.rs",
        "mod bridge; use crate::bridge::send; pub fn run()->bool {send()}",
    );
    write(
        &root,
        "crates/caller/src/bridge.rs",
        "pub use event_front::emit as send;",
    );
    index(&root);
    let (fresh, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(
        site(&fresh)["binding_path"]["status"],
        "verified_indexed_path"
    );
    assert!(
        site(&fresh)["binding_path"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["span"]["file"] == "crates/caller/src/bridge.rs")
    );
    // Only the intermediate alias module changes: old proof must disappear.
    write(
        &root,
        "crates/caller/src/bridge.rs",
        "pub use crate::bridge::send as send;",
    );
    let (stale, _) = context(&root, "api.lib.record_event", &[]);
    assert_eq!(site(&stale)["binding_path"]["status"], "mismatch");
    assert!(site(&stale).get("expression").is_none());
    index(&root);
    let (cycle, _) = context(&root, "api.lib.record_event", &[]);
    assert!(
        cycle["symbols"][0]["callers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Rebind through a different dependency and verify old edges are removed.
    let manifest = std::fs::read_to_string(root.join("crates/caller/Cargo.toml")).unwrap();
    write(
        &root,
        "crates/caller/Cargo.toml",
        &(manifest + "other={package='decoy',path='../decoy'}\n"),
    );
    write(
        &root,
        "crates/caller/src/bridge.rs",
        "pub use other::publish as send;",
    );
    index(&root);
    let (new, _) = context(&root, "decoy.lib.publish", &[]);
    assert_eq!(
        site(&new)["binding_path"]["status"],
        "verified_indexed_path"
    );
    // Conflicting explicit aliases are unknown, not whichever was visited last.
    write(
        &root,
        "crates/caller/src/bridge.rs",
        "pub use other::publish as send; pub use event_front::emit as send;",
    );
    index(&root);
    for target in ["api.lib.record_event", "decoy.lib.publish"] {
        let (value, _) = context(&root, target, &[]);
        assert!(
            value["symbols"][0]["callers"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    write(
        &root,
        "crates/caller/src/bridge.rs",
        "pub use other::publish as send;",
    );
    index(&root);
    std::fs::remove_file(root.join("crates/caller/src/bridge.rs")).unwrap();
    index(&root);
    let (deleted, _) = context(&root, "decoy.lib.publish", &[]);
    assert!(
        deleted["symbols"][0]["callers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
