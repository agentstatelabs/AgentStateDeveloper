//! Audit-log integration test for the MCP server.
//!
//! Spawns `asd-mcp` with `ASD_DB` + `ASD_AUDIT_LOG` set, primes the db with a
//! single sample symbol, and calls `ledger_append`. After the tool call
//! completes we read back the JSONL audit log and assert a `ledger.append`
//! event was emitted with the expected actor id + outcome.
//!
//! Pattern mirrors `mcp_smoke.rs` — inline prime-db, then spawn the binary
//! via `CARGO_BIN_EXE_asd-mcp`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use agentstatedeveloper_core::read_jsonl;

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

    let parsed = adapter
        .parse_symbols(&file_str, &source)
        .expect("parse_symbols");
    let index_store = AsgIndexStore { repo: &engine.repo };
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
        };
        index_store
            .put_symbol(&engine.ref_name, &symbol, "audit-test")
            .expect("put_symbol");
    }
}

#[test]
#[ignore = "exercises commercial tamper-evident audit sink — runs against asd-pro in the enterprise workspace"]
fn mcp_ledger_append_emits_audit_event() {
    let unique = format!(
        "asd-mcp-audit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = std::env::temp_dir();
    let db_path = tmp.join(format!("{}.db", unique));
    let audit_path = tmp.join(format!("{}.audit.jsonl", unique));
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);

    prime_db(&db_path);

    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_asd-mcp"));
    let mut child = Command::new(&bin)
        .env("ASD_DB", &db_path)
        .env("ASD_AUDIT_LOG", &audit_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn asd-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"audit-test","version":"0.1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"ledger_append","arguments":{{"qname":"greetings.hello","kind":"decision","summary":"test audit emission","author_id":"audit-actor"}}}}}}"#
    )
    .unwrap();
    drop(stdin);

    std::thread::sleep(Duration::from_millis(1500));
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    // Find the response with id=2 and pull its tool-call result for debugging.
    let response_line = stdout
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .unwrap_or("");
    eprintln!("AUDIT_TOOL_RESPONSE: {}", response_line);

    // Now read back the audit log.
    assert!(
        audit_path.exists(),
        "audit log file was not created at {}",
        audit_path.display()
    );
    let events = read_jsonl(&audit_path).expect("read audit jsonl");
    eprintln!("AUDIT_EVENTS ({}):", events.len());
    for e in &events {
        eprintln!("  {}", serde_json::to_string(e).unwrap());
    }

    // Expect exactly one ledger.append event carrying the author_id we sent.
    let append_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "ledger.append")
        .collect();
    assert_eq!(
        append_events.len(),
        1,
        "expected exactly one ledger.append event, got {} (all events: {:?})",
        append_events.len(),
        events
            .iter()
            .map(|e| (&e.event_type, &e.outcome))
            .collect::<Vec<_>>(),
    );
    let evt = append_events[0];
    assert_eq!(evt.actor_id, "audit-actor");
    assert_eq!(evt.actor_kind, "agent");
    // Permissive gate matches no policy → status is "no-policy-match".
    // Accept either that or "allowed" in case policy wiring changes default.
    assert!(
        evt.outcome == "no-policy-match" || evt.outcome == "allowed",
        "unexpected outcome: {}",
        evt.outcome
    );
    assert!(
        evt.subject_id.is_some(),
        "expected subject_id (entry_id) to be set"
    );
    assert!(
        evt.secondary_id.is_some(),
        "expected secondary_id (symbol_id) to be set"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);
}
