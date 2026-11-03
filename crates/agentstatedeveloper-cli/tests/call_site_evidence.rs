//! Real parser/index/CLI contracts for opt-in call-site evidence.
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
fn write(root: &Path, file: &str, text: &str) {
    let p = root.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}
fn call(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_asd"))
        .current_dir(root)
        .args(["--db", root.join(".asd.db").to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}
fn query(root: &Path, args: &[&str]) -> Value {
    let r = call(root, args);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    serde_json::from_slice(&r.stdout).unwrap()
}
fn index(root: &Path) {
    query(root, &["index", root.to_str().unwrap()]);
}
fn context(root: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["context-for", "api.lib.record_event", "--quiet"];
    args.extend(extra);
    query(root, &args)
}
const CALLER: &str = "use api_dep::publish as audit;\npub fn handler() {\n    let _ = \"π\";\n    audit(); audit();\n    api_dep::publish();\n}\n";
fn fixture(root: &Path) {
    write(
        root,
        "crates/api/Cargo.toml",
        "[package]\nname='api'\nversion='0.1.0'\n",
    );
    write(
        root,
        "crates/api/src/lib.rs",
        "pub fn record_event() {} pub use record_event as publish;\n",
    );
    write(
        root,
        "crates/caller/Cargo.toml",
        "[package]\nname='caller'\nversion='0.1.0'\n[dependencies]\napi_dep={package='api',path='../api'}\n",
    );
    write(root, "crates/caller/src/lib.rs", CALLER);
    write(
        root,
        "crates/other/Cargo.toml",
        "[package]\nname='other'\nversion='0.1.0'\n[dependencies]\napi_dep={package='api',path='../api'}\n",
    );
    write(
        root,
        "crates/other/src/lib.rs",
        "pub fn handle() { api_dep::publish(); }\n",
    );
    write(
        root,
        "crates/decoy/src/lib.rs",
        "fn record_event() {} fn preview(){record_event();}\n",
    );
    index(root);
}
fn row<'a>(v: &'a Value, file: &str) -> &'a Value {
    v["symbols"][0]["callers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["file"] == file)
        .unwrap()
}
#[test]
fn sites_preserve_duplicate_locations_import_candidates_limits_and_source_freshness() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let plain = context(root, &[]);
    assert!(
        plain["symbols"][0]["callers"][0]
            .get("call_sites")
            .is_none()
    );
    let v = context(root, &["--include-call-sites"]);
    assert_eq!(v["symbols"][0]["callers"].as_array().unwrap().len(), 2);
    let caller = row(&v, "crates/caller/src/lib.rs");
    let sites = caller["call_sites"]["sites"].as_array().unwrap();
    assert_eq!(caller["call_sites"]["total_sites"], 3);
    assert_eq!(caller["call_sites"]["binding_chain"], "not_recorded");
    assert_eq!(sites[0]["expression"], "audit");
    assert_eq!(sites[1]["expression"], "audit");
    assert_eq!(sites[0]["syntactic_import_candidate"], "api_dep.publish");
    assert_eq!(sites[2]["expression"], "api_dep::publish");
    assert_eq!(sites[0]["location"]["start_line"], 4);
    assert_eq!(sites[0]["location"]["start_col"], 5);
    assert_eq!(sites[1]["location"]["start_col"], 14);
    for s in sites {
        assert_eq!(s["source"]["index_match"], "matched");
        let loc = &s["location"];
        let text = CALLER
            .lines()
            .nth(loc["start_line"].as_u64().unwrap() as usize - 1)
            .unwrap();
        let a = loc["start_col"].as_u64().unwrap() as usize - 1;
        let b = loc["end_col"].as_u64().unwrap() as usize - 1;
        assert_eq!(&text[a..b], s["expression"].as_str().unwrap());
    }
    let limited = context(root, &["--include-call-sites", "--call-site-limit", "1"]);
    assert_eq!(
        row(&limited, "crates/caller/src/lib.rs")["call_sites"]["omitted_sites"],
        2
    );
    let limited = context(root, &["--include-call-sites", "--caller-limit", "0"]);
    assert!(
        limited["symbols"][0]["callers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["call_sites"]["status"] == "omitted")
    );
    for tokens in [128, 256, 512, 1024, 4096] {
        let r = call(
            root,
            &[
                "context-for",
                "api.lib.record_event",
                "--quiet",
                "--include-call-sites",
                "--budget-tokens",
                &tokens.to_string(),
            ],
        );
        assert!(r.status.success());
        assert!(r.stdout.len() <= tokens * 4);
        let _: Value = serde_json::from_slice(&r.stdout).unwrap();
    }
    write(root, "crates/caller/src/lib.rs", &format!("\n\n{CALLER}"));
    let stale = context(root, &["--include-call-sites"]);
    for s in row(&stale, "crates/caller/src/lib.rs")["call_sites"]["sites"]
        .as_array()
        .unwrap()
    {
        assert_eq!(s["source"]["index_match"], "mismatch");
        assert!(s.get("expression").is_none());
        assert!(s.get("syntactic_import_candidate").is_none());
        assert!(s["source"].get("text").is_none());
    }
    assert_eq!(
        row(&stale, "crates/other/src/lib.rs")["call_sites"]["sites"][0]["source"]["index_match"],
        "matched"
    );
    index(root);
    let fresh = context(root, &["--include-call-sites"]);
    assert_eq!(
        row(&fresh, "crates/caller/src/lib.rs")["call_sites"]["sites"][0]["location"]["start_line"],
        6
    );
    // Removing every edge must clear graph AND evidence, not keep an old subtree.
    write(root, "crates/caller/src/lib.rs", "pub fn handler() {}\n");
    write(root, "crates/other/src/lib.rs", "pub fn handle() {}\n");
    write(
        root,
        "crates/decoy/src/lib.rs",
        "fn record_event() {} fn preview() {}\n",
    );
    index(root);
    assert_eq!(
        context(root, &["--include-call-sites"])["symbols"][0]["callers"],
        json!([])
    );
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    let tree = engine
        .repo
        .get_tree(
            &engine.ref_name,
            &agentstatedeveloper_core::paths::call_sites_root(),
        )
        .unwrap();
    assert!(
        tree.get(v["symbols"][0]["symbol"]["symbol_id"].as_str().unwrap())
            .is_none()
    );
}
#[test]
fn legacy_or_mismatched_site_records_do_not_present_an_unverified_explanation() {
    use agentstategraph::CommitOptions;
    use agentstategraph_core::IntentCategory;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let v = context(root, &[]);
    let target = v["symbols"][0]["symbol"]["symbol_id"].as_str().unwrap();
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    let path = agentstatedeveloper_core::paths::call_sites_root();
    let mut records = engine.repo.get_tree(&engine.ref_name, &path).unwrap();
    let caller = records[target]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    for site in caller.as_array_mut().unwrap() {
        site["caller_symbol_fp"] = json!("wrong");
    }
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &path,
            &records,
            CommitOptions::new("test", IntentCategory::Refine, "mismatch"),
        )
        .unwrap();
    drop(engine);
    let mismatched = context(root, &["--include-call-sites"]);
    assert!(
        mismatched["symbols"][0]["callers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["call_sites"]["sites"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["status"] == "unavailable" && s.get("expression").is_none()))
    );
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    for bucket in records.as_object_mut().unwrap().values_mut() {
        for rows in bucket.as_object_mut().unwrap().values_mut() {
            for site in rows.as_array_mut().unwrap() {
                site["indexed_file_blake3"] = Value::Null;
            }
        }
    }
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &path,
            &records,
            CommitOptions::new("test", IntentCategory::Refine, "missing provenance"),
        )
        .unwrap();
    drop(engine);
    let unknown = context(root, &["--include-call-sites"]);
    for row in unknown["symbols"][0]["callers"].as_array().unwrap() {
        for site in row["call_sites"]["sites"].as_array().unwrap() {
            assert_eq!(site["status"], "unavailable");
            assert!(site.get("expression").is_none());
            assert!(site["source"].get("text").is_none());
        }
    }
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &path,
            &json!({}),
            CommitOptions::new("test", IntentCategory::Refine, "legacy"),
        )
        .unwrap();
    drop(engine);
    let legacy = context(root, &["--include-call-sites"]);
    assert!(
        legacy["symbols"][0]["callers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["call_sites"]["status"] == "not_recorded")
    );
}
