//! Policy-gating integration test for the MCP server.
//!
//! Spawns `asd-mcp` with `ASD_DB` + `ASD_POLICY` set, primes the db with a
//! single sample symbol, and calls `ledger_append` with a kind that the
//! policy file denies. Asserts the server either returns a JSON error
//! carrying the policy path or a structured `{ "status": "denied" }`
//! response — whichever shape the tool emits.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Prime a SQLite db with a single python file's symbols. Scoped down from
/// `mcp_smoke::prime_db` — we only need something `ledger_append` can target.
fn prime_db(db_path: &std::path::Path) {
    use std::sync::Arc;

    use agentstatedeveloper_core::{
        AsgIndexStore, Engine, IndexStore, LanguageAdapter, Position, Symbol, canonical_symbol_id,
        symbol_fingerprint,
    };
    use agentstatedeveloper_python::PythonAdapter;

    let engine = Engine::open_sqlite(db_path).expect("open engine");
    let adapter = Arc::new(PythonAdapter::new());

    let sample_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample-py-repo")
        .canonicalize()
        .expect("sample-py-repo must exist");

    let target = sample_root.join("greetings.py");
    let source = std::fs::read_to_string(&target).expect("read greetings.py");
    let rel = target.strip_prefix(&sample_root).unwrap_or(&target);
    let file_str = rel.to_string_lossy().replace('\\', "/");

    let parsed = adapter.parse_symbols(&file_str, &source).expect("parse_symbols");
    let index_store = AsgIndexStore::new(&engine.repo);
    for p in &parsed {
        let symbol_id = canonical_symbol_id(&p.qname, p.kind, &file_str);
        let symbol_fp = symbol_fingerprint(&p.body);
        let symbol = Symbol {
            symbol_id,
            symbol_fp,
            qname: p.qname.clone(),
            language: "python".to_string(),
            kind: p.kind,
            file: file_str.clone(),
            start: Position {
                line: p.start_line,
                col: p.start_col,
            },
            end: Position {
                line: p.end_line,
                col: p.end_col,
            },
            signature: p.signature.clone(),
            doc: None,
        };
        index_store
            .put_symbol(&engine.ref_name, &symbol, "policy-test")
            .expect("put_symbol");
    }
}

fn write_policy_file(path: &std::path::Path) {
    // Deny all `asd.ledger.append.tradeoff` actions regardless of agent.
    let body = r#"{
        "policies": [
            {
                "path": "/policies/test/no-tradeoffs",
                "version": 3,
                "match_action": "asd.ledger.append.tradeoff",
                "deny": true,
                "reason": "tradeoff entries disabled for test"
            }
        ],
        "strict": false
    }"#;
    std::fs::write(path, body).expect("write policy file");
}

#[test]
fn mcp_ledger_append_denied_by_policy() {
    let unique = format!(
        "asd-mcp-policy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = std::env::temp_dir();
    let db_path = tmp.join(format!("{}.db", unique));
    let policy_path = tmp.join(format!("{}.policy.json", unique));
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&policy_path);

    prime_db(&db_path);
    write_policy_file(&policy_path);

    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_asd-mcp"));
    let mut child = Command::new(&bin)
        .env("ASD_DB", &db_path)
        .env("ASD_POLICY", &policy_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn asd-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"policy-test","version":"0.1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"ledger_append","arguments":{{"qname":"greetings.hello","kind":"tradeoff","summary":"should be denied","author_id":"test-agent"}}}}}}"#
    )
    .unwrap();
    drop(stdin);

    std::thread::sleep(Duration::from_millis(1500));
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    eprintln!("POLICY_STDOUT:\n{}", stdout);

    // Find the response with id=2.
    let response_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .unwrap_or("");
    eprintln!("POLICY_RESPONSE: {}", response_line);

    // The tool result comes back as a text-content string embedded in the
    // JSON-RPC response. It should mention the policy path and either an
    // error/denied shape.
    // normalize() strips the leading slash, so match without it.
    let matched_policy_present = response_line.contains("policies/test/no-tradeoffs");
    let denied_shape = response_line.contains("policy denied")
        || response_line.contains("\"status\":\"denied\"")
        || response_line.contains("\\\"status\\\":\\\"denied\\\"");

    assert!(
        matched_policy_present && denied_shape,
        "expected policy-denied response, got: {}",
        response_line
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&policy_path);
}
