//! Smoke test for the MCP stdio server.
//!
//! Spawns `asd-mcp` as a child process, sends the MCP handshake plus
//! `tools/list` + `tools/call name=health` over stdin, and asserts the first
//! three JSON-RPC responses appear on stdout.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Build a primed SQLite db by walking sample-py-repo with the same logic the
/// `asd` CLI uses. We inline it here so the test doesn't depend on a prebuilt
/// CLI binary.
fn prime_db(db_path: &std::path::Path) {
    use std::sync::Arc;

    use agentstatedeveloper_core::{
        AsgEffectStore, AsgIndexStore, EffectDecl, EffectStore, Engine, IndexStore,
        LanguageAdapter, Position, Symbol, Verification, VerificationSource, VerificationStatus,
        canonical_symbol_id, symbol_fingerprint,
    };
    use agentstatedeveloper_python::PythonAdapter;
    use chrono::Utc;

    let mut engine = Engine::open_sqlite(db_path).expect("open engine");
    let adapter = Arc::new(PythonAdapter::new());
    let adapter_dyn: Arc<dyn agentstatedeveloper_core::LanguageAdapter> = adapter.clone();
    engine.register_adapter(adapter_dyn);

    let sample_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample-py-repo")
        .canonicalize()
        .expect("sample-py-repo must exist");

    let mut files = Vec::new();
    collect_py(&sample_root, &mut files);

    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };

    for file in &files {
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = file.strip_prefix(&sample_root).unwrap_or(file);
        let file_str = rel.to_string_lossy().replace('\\', "/");

        let parsed = match adapter.parse_symbols(&file_str, &source) {
            Ok(p) => p,
            Err(_) => continue,
        };
        for p in &parsed {
            let symbol_id = canonical_symbol_id(&p.qname, p.kind, &file_str);
            let symbol_fp = symbol_fingerprint(&p.body);
            let symbol = Symbol {
                symbol_id: symbol_id.clone(),
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
                doc: p.doc.clone(),
            };
            index_store
                .put_symbol(&engine.ref_name, &symbol, "smoke")
                .expect("put_symbol");

            let declared = adapter.infer_effects(&source, p);
            let decl = EffectDecl {
                symbol_id: symbol_id.clone(),
                declared,
                transitive: Vec::new(),
                verification: Some(Verification {
                    by: VerificationSource::StaticChecker,
                    at: Utc::now(),
                    status: VerificationStatus::Unverified,
                    mismatches: Vec::new(),
                }),
                confidence: None,
                matched_policy: None,
            };
            effect_store
                .put_effects(&engine.ref_name, &symbol_id, &decl, "smoke")
                .expect("put_effects");
        }
    }
}

fn collect_py(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_py(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("py") {
            out.push(path);
        }
    }
}

#[test]
fn mcp_stdio_smoke() {
    let db_path = std::env::temp_dir().join(format!("asd-mcp-smoke-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    prime_db(&db_path);

    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_asd-mcp"));
    let mut child = Command::new(&bin)
        .env("ASD_DB", &db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn asd-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"smoke","version":"0.1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"health","arguments":{{}}}}}}"#
    )
    .unwrap();
    drop(stdin);

    std::thread::sleep(Duration::from_millis(1500));
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

    let mut lines = stdout.lines();
    let line1 = lines.next().unwrap_or("").to_string();
    let line2 = lines.next().unwrap_or("").to_string();
    let line3 = lines.next().unwrap_or("").to_string();

    eprintln!("SMOKE_LINE1: {}", line1);
    eprintln!("SMOKE_LINE2: {}", line2);
    eprintln!("SMOKE_LINE3: {}", line3);

    assert!(line1.contains("\"id\":1"), "line1: {}", line1);
    assert!(
        line2.contains("\"id\":2") && line2.contains("health"),
        "line2: {}",
        line2
    );
    assert!(
        line3.contains("\"id\":3") && line3.contains("symbol_count"),
        "line3: {}",
        line3
    );

    let _ = std::fs::remove_file(&db_path);
}
