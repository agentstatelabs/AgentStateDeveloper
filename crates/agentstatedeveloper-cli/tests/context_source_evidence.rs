//! Native CLI contract: indexed graph plus current source, bounded and explicit.
use std::{fs, path::Path, process::Command};

fn call(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_asd"))
        .current_dir(root)
        .args(["--db", root.join(".asd.db").to_str().unwrap()])
        .args(args)
        .output()
        .unwrap()
}
fn json(root: &Path, args: &[&str]) -> serde_json::Value {
    let out = call(root, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
#[test]
fn target_and_callers_include_real_source_and_explicit_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname='evidence-core'\nversion='0.1.0'\n",
    );
    write(
        root,
        "crates/core/src/lib.rs",
        "pub mod doctor; pub use doctor::run_doctor;",
    );
    write(
        root,
        "crates/core/src/doctor.rs",
        "pub fn run_doctor(path: &str) -> bool { !path.is_empty() }\n",
    );
    write(
        root,
        "crates/http/Cargo.toml",
        "[package]\nname='evidence-http'\nversion='0.1.0'\n",
    );
    write(
        root,
        "crates/http/src/lib.rs",
        "pub fn route(request: Option<&str>) -> bool {\n    match request { Some(p) => evidence_core::run_doctor(p), None => false }\n}\n",
    );
    write(
        root,
        "crates/mcp/Cargo.toml",
        "[package]\nname='evidence-mcp'\nversion='0.1.0'\n",
    );
    write(
        root,
        "crates/mcp/src/lib.rs",
        "pub fn tool(path: &str) -> bool {\n    evidence_core::run_doctor(path)\n}\n",
    );
    let indexed = call(root, &["index", root.to_str().unwrap()]);
    assert!(indexed.status.success());
    let ordinary = json(root, &["context-for", "doctor.run_doctor", "--quiet"]);
    assert!(ordinary["symbols"][0].get("source").is_none());
    let target = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-body",
            "--quiet",
        ],
    );
    assert!(
        target["symbols"][0]["source"]["text"]
            .as_str()
            .unwrap()
            .contains("!path.is_empty()")
    );
    let rich = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-caller-bodies",
            "--budget-tokens",
            "2000",
            "--quiet",
        ],
    );
    assert_eq!(rich["_budget"]["truncated"], false);
    let callers = rich["symbols"][0]["callers"].as_array().unwrap();
    assert_eq!(callers.len(), 2);
    assert!(
        callers
            .iter()
            .all(|r| r["source"]["text"].as_str().unwrap().contains("run_doctor"))
    );
    assert!(callers.iter().any(|r| {
        r["source"]["text"]
            .as_str()
            .unwrap()
            .contains("None => false")
    }));
    assert!(
        callers
            .iter()
            .all(|r| r["source"]["index_match"] == "matched")
    );
    // A changed caller does not invalidate the target's own source, but its
    // stale caller body must be suppressed independently.
    write(
        root,
        "crates/http/src/lib.rs",
        "pub fn route(request: Option<&str>) -> bool {\n    match request { Some(p) => evidence_core::run_doctor(p), None => true }\n}\n",
    );
    let mixed = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-caller-bodies",
            "--quiet",
        ],
    );
    assert_eq!(mixed["symbols"][0]["source"]["index_match"], "matched");
    let changed_caller = mixed["symbols"][0]["callers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["file"] == "crates/http/src/lib.rs")
        .unwrap();
    assert_eq!(changed_caller["source"]["index_match"], "mismatch");
    assert!(changed_caller["source"].get("text").is_none());
    assert_eq!(mixed["source_contract"]["graph_freshness"], "not_verified");
    let limited = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-caller-bodies",
            "--caller-limit",
            "0",
            "--quiet",
        ],
    );
    assert!(
        limited["symbols"][0]["callers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["source"]["status"] == "omitted")
    );
    for tokens in ["128", "256", "512"] {
        let output = call(
            root,
            &[
                "context-for",
                "doctor.run_doctor",
                "--include-caller-bodies",
                "--budget-tokens",
                tokens,
                "--quiet",
            ],
        );
        assert!(output.status.success());
        assert!(output.stdout.len() <= tokens.parse::<usize>().unwrap() * 4);
        let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(v["_budget"]["truncated"], true);
    }
    assert!(
        !call(
            root,
            &["context-for", "doctor.run_doctor", "--budget-tokens", "0"]
        )
        .status
        .success()
    );
    assert!(
        !call(
            root,
            &["context-for", "doctor.run_doctor", "--body-lines", "0"]
        )
        .status
        .success()
    );
    assert!(
        !call(
            root,
            &[
                "context-for",
                "doctor.run_doctor",
                "--budget-tokens",
                "18446744073709551615"
            ]
        )
        .status
        .success()
    );
    let before = target["symbols"][0]["source"]["file_blake3"].clone();
    write(
        root,
        "crates/core/src/doctor.rs",
        "pub fn run_doctor(path: &str) -> bool { path == \"changed\" }\n",
    );
    let changed = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-body",
            "--quiet",
        ],
    );
    assert_ne!(changed["symbols"][0]["source"]["file_blake3"], before);
    assert_eq!(changed["symbols"][0]["source"]["index_match"], "mismatch");
    assert!(changed["symbols"][0]["source"].get("text").is_none());
    // Inserting lines must not return the old range even when the function's
    // semantic body is unchanged. Reindexing restores a matched range.
    write(
        root,
        "crates/core/src/doctor.rs",
        "\n\n\n\npub fn run_doctor(path: &str) -> bool { path == \"changed\" }\n",
    );
    let moved = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-body",
            "--quiet",
        ],
    );
    assert_eq!(moved["symbols"][0]["source"]["index_match"], "mismatch");
    assert!(moved["symbols"][0]["source"].get("text").is_none());
    assert!(
        call(root, &["index", root.to_str().unwrap()])
            .status
            .success()
    );
    let refreshed = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-body",
            "--quiet",
        ],
    );
    assert_eq!(refreshed["symbols"][0]["source"]["index_match"], "matched");
    assert_eq!(refreshed["symbols"][0]["source"]["start_line"], 5);
    assert!(
        refreshed["symbols"][0]["source"]["text"]
            .as_str()
            .unwrap()
            .contains("changed")
    );
    fs::remove_file(root.join("crates/core/src/doctor.rs")).unwrap();
    let missing = json(
        root,
        &[
            "context-for",
            "doctor.run_doctor",
            "--include-body",
            "--quiet",
        ],
    );
    assert_eq!(missing["symbols"][0]["source"]["status"], "unavailable");
    assert!(
        call(root, &["index", root.to_str().unwrap()])
            .status
            .success()
    );
    let engine = agentstatedeveloper_core::Engine::open_sqlite(&root.join(".asd.db")).unwrap();
    let provenance = engine
        .repo
        .get_tree(
            &engine.ref_name,
            &agentstatedeveloper_core::paths::source_fingerprints_root(),
        )
        .unwrap();
    let removed_id = target["symbols"][0]["symbol"]["symbol_id"]
        .as_str()
        .unwrap();
    assert!(
        provenance.get(removed_id).is_none(),
        "deleted-symbol provenance must be pruned"
    );
}
