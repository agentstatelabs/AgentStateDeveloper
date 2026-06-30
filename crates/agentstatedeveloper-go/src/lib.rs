//! Go language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-go`. Parses top-level functions, receiver methods,
//! struct types, and interface types, and runs a substring-based effect
//! inference pass.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::cross_service::{
    DetectedEndpoint, Direction, Transport, http_contract,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Node, Parser};

/// Go language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct GoAdapter;

impl GoAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for GoAdapter {
    fn language(&self) -> &str {
        "go"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| AsdError::Parse(format!("failed to set go language: {e}")))?;

        let src_bytes = source.as_bytes();
        let tree = parser
            .parse(src_bytes, None)
            .ok_or_else(|| AsdError::Parse(format!("failed to parse {file}")))?;

        let module_prefix = module_qname_prefix(file);
        let root = tree.root_node();
        let mut out = Vec::new();
        walk(root, src_bytes, &module_prefix, &mut out);
        Ok(out)
    }

    fn infer_effects(&self, _source: &str, symbol: &ParsedSymbol) -> Vec<Effect> {
        infer_effects_from_body(&symbol.body)
    }

    fn infer_service_endpoints(
        &self,
        file: &str,
        source: &str,
        symbols: &[ParsedSymbol],
    ) -> Vec<DetectedEndpoint> {
        infer_service_endpoints_in_go(file, source, symbols)
    }

    fn extract_call_edges(
        &self,
        file: &str,
        source: &str,
        symbols: &[ParsedSymbol],
        workspace: &WorkspaceSymbols,
    ) -> Vec<CallEdge> {
        extract_call_edges_impl(file, source, symbols, workspace)
    }
}

/// Derive the dotted module prefix for a file path.
///
/// `payments/charge.go` -> `payments.charge`
/// `./foo/bar.go`       -> `foo.bar`
/// `main.go`            -> `main`
fn module_qname_prefix(file: &str) -> String {
    let mut s = file;
    if let Some(stripped) = s.strip_prefix("./") {
        s = stripped;
    }
    let s = s.strip_suffix(".go").unwrap_or(s);
    s.replace('\\', "/").replace('/', ".")
}

/// Walk the source tree. We enumerate:
/// - `function_declaration`  → Function
/// - `method_declaration`    → Method (receiver type becomes part of qname)
/// - `type_declaration`      → Class for struct_type / interface_type definitions
fn walk(node: Node<'_>, src: &[u8], module_prefix: &str, out: &mut Vec<ParsedSymbol>) {
    match node.kind() {
        "function_declaration" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = if module_prefix.is_empty() {
                name.clone()
            } else {
                format!("{module_prefix}.{name}")
            };
            let signature = extract_fn_signature(node, src, &name, None);
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Function,
                signature,
            ));
        }
        "method_declaration" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let receiver_type =
                extract_receiver_type(node, src).unwrap_or_else(|| "<unknown>".to_string());
            let qname = if module_prefix.is_empty() {
                format!("{receiver_type}.{name}")
            } else {
                format!("{module_prefix}.{receiver_type}.{name}")
            };
            let signature = extract_fn_signature(node, src, &name, Some(&receiver_type));
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Method,
                signature,
            ));
        }
        "type_declaration" => {
            // Contains one or more `type_spec` children.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    let name =
                        node_field_text(child, "name", src).unwrap_or_else(|| "<anon>".to_string());
                    let type_node = child.child_by_field_name("type");
                    let type_kind = type_node.map(|n| n.kind()).unwrap_or("");
                    if matches!(type_kind, "struct_type" | "interface_type") {
                        let qname = if module_prefix.is_empty() {
                            name.clone()
                        } else {
                            format!("{module_prefix}.{name}")
                        };
                        let kw = if type_kind == "struct_type" {
                            "struct"
                        } else {
                            "interface"
                        };
                        let signature = Some(format!("type {name} {kw}"));
                        out.push(make_parsed_symbol(
                            child,
                            src,
                            qname,
                            SymbolKind::Class,
                            signature,
                        ));
                    }
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk(child, src, module_prefix, out);
            }
        }
    }
}

/// Extract the base receiver type name from a `method_declaration`.
///
/// Handles `(c *Client)` → `"Client"` and `(c Client)` → `"Client"`.
fn extract_receiver_type(node: Node<'_>, src: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    // receiver is a `parameter_list`; walk its children for a `parameter_declaration`.
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            // The type field may be pointer_type or type_identifier.
            if let Some(type_node) = child.child_by_field_name("type") {
                return base_type_name(type_node, src);
            }
        }
    }
    None
}

/// Collapse a Go type node to its base identifier.
fn base_type_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => node_text(node, src),
        "pointer_type" => {
            // `*Client` → walk child for the base type.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_identifier" {
                    return node_text(child, src);
                }
            }
            None
        }
        "qualified_type" => {
            // `pkg.Type` → we want just `Type` for the receiver.
            node.child_by_field_name("name")
                .and_then(|n| node_text(n, src))
        }
        _ => node_text(node, src),
    }
}

fn extract_fn_signature(
    node: Node<'_>,
    src: &[u8],
    name: &str,
    receiver: Option<&str>,
) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_text = node_text(params, src)?;
    let result = node
        .child_by_field_name("result")
        .and_then(|n| node_text(n, src))
        .map(|s| format!(" {}", s.trim()))
        .unwrap_or_default();
    if let Some(recv) = receiver {
        Some(format!("func ({recv}) {name}{params_text}{result}"))
    } else {
        Some(format!("func {name}{params_text}{result}"))
    }
}

fn node_field_text(node: Node<'_>, field: &str, src: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|n| node_text(n, src))
}

fn node_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    src.get(node.byte_range())
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

fn make_parsed_symbol(
    node: Node<'_>,
    src: &[u8],
    qname: String,
    kind: SymbolKind,
    signature: Option<String>,
) -> ParsedSymbol {
    let start = node.start_position();
    let end = node.end_position();
    let body = node_text(node, src).unwrap_or_default();
    ParsedSymbol {
        qname,
        kind,
        start_line: (start.row as u32) + 1,
        start_col: (start.column as u32) + 1,
        end_line: (end.row as u32) + 1,
        end_col: (end.column as u32) + 1,
        body,
        signature,
        doc: None,
    }
}

// -----------------------------------------------------------------------------
// Cross-service endpoint detection (t-014)
//
// Inbound: chi/gin/echo route methods `r.Get("/p", …)` / `r.GET("/p", …)` with
// a literal path starting with '/'. Outbound: stdlib `http.Get/Post/Head/
// PostForm("url")` and `http.NewRequest[WithContext](… "METHOD", "url" …)`.
// Go has no string interpolation, so backtick raw strings are literals too.
// `http.HandleFunc` (no method) and client-object calls are skipped.
// -----------------------------------------------------------------------------

const VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

fn infer_service_endpoints_in_go(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
) -> Vec<DetectedEndpoint> {
    let mut out = Vec::new();
    for (idx, raw) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line = raw.trim();

        for (method, path) in go_routes(line) {
            if let Some(owner) = owner_for_line(symbols, line_no) {
                out.push(DetectedEndpoint {
                    transport: Transport::Http,
                    direction: Direction::Inbound,
                    contract: http_contract(&method, &path),
                    owner_qname: owner.qname.clone(),
                    file: file.to_string(),
                    line: line_no,
                    confidence: 0.9,
                    note: None,
                });
            }
        }
        for (method, url) in go_clients(line) {
            if let Some(owner) = owner_for_line(symbols, line_no) {
                out.push(DetectedEndpoint {
                    transport: Transport::Http,
                    direction: Direction::Outbound,
                    contract: http_contract(&method, &url),
                    owner_qname: owner.qname.clone(),
                    file: file.to_string(),
                    line: line_no,
                    confidence: 0.85,
                    note: None,
                });
            }
        }
    }
    out
}

fn owner_for_line(symbols: &[ParsedSymbol], line: u32) -> Option<&ParsedSymbol> {
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .max_by_key(|s| s.start_line)
}

/// Router routes `<obj>.<Verb>("/path", …)` for both chi (`Get`) and gin/echo
/// (`GET`) capitalizations; path must be a literal starting with '/'.
fn go_routes(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for verb in VERBS {
        for variant in [title_case(verb), verb.to_uppercase()] {
            let needle = format!(".{variant}(");
            let mut from = 0;
            while let Some(rel) = line[from..].find(&needle) {
                let start = from + rel;
                from = start + needle.len();
                if let Some(path) = first_go_string(&line[start + needle.len()..]) {
                    if path.starts_with('/') {
                        out.push((verb.to_uppercase(), path));
                    }
                }
            }
        }
    }
    out
}

/// stdlib HTTP clients: `http.Get/Post/Head/PostForm("url")` and
/// `http.NewRequest[WithContext](… "METHOD", "url" …)`.
fn go_clients(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (func, method) in [
        ("Get", "GET"),
        ("Post", "POST"),
        ("Head", "HEAD"),
        ("PostForm", "POST"),
    ] {
        let needle = format!("http.{func}(");
        if let Some(pos) = line.find(&needle) {
            if call_boundary_ok(line, pos) {
                if let Some(url) = first_go_string(&line[pos + needle.len()..]) {
                    out.push((method.to_string(), url));
                }
            }
        }
    }
    for ctor in ["http.NewRequestWithContext(", "http.NewRequest("] {
        if let Some(pos) = line.find(ctor) {
            if call_boundary_ok(line, pos) {
                // First two string literals = (METHOD, url); a leading ctx arg
                // (NewRequestWithContext) isn't a string, so it's skipped.
                let lits = go_string_literals(&line[pos + ctor.len()..], 2);
                if let [m, u] = lits.as_slice() {
                    out.push((m.to_uppercase(), u.clone()));
                }
            }
        }
    }
    out
}

fn call_boundary_ok(line: &str, pos: usize) -> bool {
    pos == 0
        || !line[..pos]
            .chars()
            .next_back()
            .map(|c| c.is_alphanumeric() || c == '_' || c == '.')
            .unwrap_or(false)
}

fn title_case(v: &str) -> String {
    let mut c = v.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// First string literal if the argument list starts with one (`"…"` or raw
/// `` `…` ``).
fn first_go_string(s: &str) -> Option<String> {
    let s = s.trim_start();
    let q = s.as_bytes().first().copied()?;
    if q != b'"' && q != b'`' {
        return None;
    }
    let inner = &s[1..];
    let end = inner.find(q as char)?;
    Some(inner[..end].to_string())
}

/// Up to `max` string literals scanned left-to-right from `s`, skipping
/// non-string args between them.
fn go_string_literals(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && out.len() < max {
        let c = bytes[i];
        if c == b'"' || c == b'`' {
            if let Some(rel) = s[i + 1..].find(c as char) {
                out.push(s[i + 1..i + 1 + rel].to_string());
                i = i + 1 + rel + 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    out
}

// -----------------------------------------------------------------------------
// Effect inference
// -----------------------------------------------------------------------------

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // File system — os, ioutil, bufio, filepath ops
    let fs_read_needles = [
        "os.Open(",
        "os.ReadFile(",
        "ioutil.ReadFile(",
        "ioutil.ReadDir(",
        "os.ReadDir(",
        "bufio.NewReader(",
        "filepath.Walk(",
        "filepath.WalkDir(",
    ];
    let fs_write_needles = [
        "os.Create(",
        "os.WriteFile(",
        "ioutil.WriteFile(",
        "os.OpenFile(",
        "os.Remove(",
        "os.Rename(",
        "os.Mkdir",
    ];
    if let Some(note) = first_match_note(body, &fs_read_needles) {
        effects.push(Effect {
            effect: EffectCategory::IoFsRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }
    if let Some(note) = first_match_note(body, &fs_write_needles) {
        effects.push(Effect {
            effect: EffectCategory::IoFsWrite,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Network — net/http, resty, grpc, websocket
    let net_needles = [
        "http.Get(",
        "http.Post(",
        "http.Do(",
        "http.NewRequest(",
        "resty.",
        "grpc.Dial(",
        "grpc.NewClient(",
        "net.Dial(",
        "websocket.Dial(",
    ];
    let mut net_hosts: Vec<String> = Vec::new();
    let mut net_note: Option<String> = None;
    for needle in net_needles {
        if body.contains(needle) && net_note.is_none() {
            net_note = first_matching_line(body, &[needle]);
            // Try to extract URL hosts from nearby string literals.
            for off in find_occurrences(body, needle) {
                let end = (off + 200).min(body.len());
                let snippet = &body[off..end];
                if let Some(host) = extract_url_host(snippet) {
                    if !net_hosts.contains(&host) {
                        net_hosts.push(host);
                    }
                }
            }
        }
    }
    if net_note.is_some() {
        let qualifiers = if net_hosts.is_empty() {
            serde_json::Value::Null
        } else {
            json!({ "hosts": net_hosts })
        };
        effects.push(Effect {
            effect: EffectCategory::IoNetOut,
            qualifiers,
            note: net_note,
            ..Default::default()
        });
    }

    // Database — database/sql, gorm, sqlx, pgx
    let db_needles = [
        "db.Query(",
        "db.Exec(",
        "db.QueryRow(",
        "gorm.",
        "sqlx.",
        "pgx.",
        "sql.Open(",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        let has_write = body.contains("db.Exec(")
            || body.contains(".Create(")
            || body.contains(".Save(")
            || body.contains(".Delete(")
            || body.contains(".Update(");
        let has_read = body.contains("db.Query(")
            || body.contains("db.QueryRow(")
            || body.contains(".Find(")
            || body.contains(".First(");
        if has_read || (!has_read && !has_write) {
            effects.push(Effect {
                effect: EffectCategory::IoDbRead,
                qualifiers: serde_json::Value::Null,
                note: Some(note.clone()),
                ..Default::default()
            });
        }
        if has_write || (!has_read && !has_write) {
            effects.push(Effect {
                effect: EffectCategory::IoDbWrite,
                qualifiers: serde_json::Value::Null,
                note: Some(note),
                ..Default::default()
            });
        }
    }

    // Process spawn — os/exec
    let proc_needles = ["exec.Command(", "exec.CommandContext(", "os.StartProcess("];
    if let Some(note) = first_match_note(body, &proc_needles) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Env read — os.Getenv, os.LookupEnv
    let env_needles = ["os.Getenv(", "os.LookupEnv(", "os.Environ("];
    if let Some(note) = first_match_note(body, &env_needles) {
        let mut vars: Vec<String> = Vec::new();
        for off in find_occurrences(body, "os.Getenv(") {
            let args = &body[off + "os.Getenv(".len()..];
            if let Some(v) = extract_first_string_literal(args) {
                if !vars.contains(&v) {
                    vars.push(v);
                }
            }
        }
        let qualifiers = if vars.is_empty() {
            serde_json::Value::Null
        } else {
            json!({ "vars": vars })
        };
        effects.push(Effect {
            effect: EffectCategory::EnvRead,
            qualifiers,
            note: Some(note),
            ..Default::default()
        });
    }

    // Logging — fmt.Print*, log.*, zap.*, slog.*
    let log_needles = [
        "fmt.Print(",
        "fmt.Println(",
        "fmt.Printf(",
        "fmt.Fprintf(",
        "log.Print(",
        "log.Println(",
        "log.Printf(",
        "log.Fatal(",
        "log.Panic(",
        "zap.",
        "slog.",
        "logrus.",
    ];
    if let Some(note) = first_match_note(body, &log_needles) {
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Sleep — time.Sleep
    if let Some(note) = first_match_note(body, &["time.Sleep("]) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Time read — time.Now
    let time_read_needles = ["time.Now()", "time.Since(", "time.Until(", "time.Date("];
    if let Some(note) = first_match_note(body, &time_read_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Random — math/rand, crypto/rand
    let random_needles = [
        "rand.Int(",
        "rand.Intn(",
        "rand.Float",
        "rand.Read(",
        "rand.Shuffle(",
        "rand.Int63(",
        "rand.Uint32(",
        "rand.Uint64(",
        "rand.Perm(",
        "crypto/rand",
        "rand.New(",
    ];
    if let Some(note) = first_match_note(body, &random_needles) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Throw — panic()
    if body.contains("panic(") {
        let note = first_matching_line(body, &["panic("]);
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note,
            ..Default::default()
        });
    }

    effects
}

fn find_occurrences(body: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(idx) = body[from..].find(needle) {
        out.push(from + idx);
        from += idx + needle.len();
    }
    out
}

fn extract_first_string_literal(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let q = bytes[0];
    if q != b'"' {
        return None;
    }
    let mut j = 1;
    let mut out = String::new();
    while j < bytes.len() {
        let c = bytes[j];
        if c == b'\\' && j + 1 < bytes.len() {
            out.push(bytes[j + 1] as char);
            j += 2;
            continue;
        }
        if c == q {
            return Some(out);
        }
        out.push(c as char);
        j += 1;
    }
    None
}

fn extract_url_host(s: &str) -> Option<String> {
    for scheme in ["https://", "http://"] {
        if let Some(idx) = s.find(scheme) {
            let tail = &s[idx + scheme.len()..];
            let end = tail
                .find(|c: char| c == '/' || c == '"' || c == ')' || c.is_whitespace())
                .unwrap_or(tail.len());
            let host = &tail[..end];
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
}

fn first_matching_line(body: &str, needles: &[&str]) -> Option<String> {
    for line in body.lines() {
        for n in needles {
            if line.contains(n) {
                return Some(line.trim().to_string());
            }
        }
    }
    None
}

fn first_match_note(body: &str, needles: &[&str]) -> Option<String> {
    for n in needles {
        if body.contains(n) {
            return first_matching_line(body, needles);
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Call-edge extraction
// -----------------------------------------------------------------------------

fn extract_call_edges_impl(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
    workspace: &WorkspaceSymbols,
) -> Vec<CallEdge> {
    let module_prefix = module_qname_prefix(file);
    let known: HashSet<&str> = symbols.iter().map(|s| s.qname.as_str()).collect();

    let mut by_simple: HashMap<String, String> = HashMap::new();
    for s in symbols {
        let simple = s.qname.rsplit('.').next().unwrap_or(&s.qname).to_string();
        let module_level = if module_prefix.is_empty() {
            s.qname == simple
        } else {
            s.qname == format!("{}.{}", module_prefix, simple)
        };
        if module_level {
            by_simple.insert(simple, s.qname.clone());
        } else {
            by_simple.entry(simple).or_insert_with(|| s.qname.clone());
        }
    }

    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let imports = parse_imports(source, &mut parser);
    let mut edges: HashSet<CallEdge> = HashSet::new();

    for sym in symbols {
        if !matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        let src_bytes = sym.body.as_bytes();
        let tree = match parser.parse(src_bytes, None) {
            Some(t) => t,
            None => continue,
        };
        let enclosing_type = enclosing_type_qname(&sym.qname, &module_prefix, &known);
        collect_calls(
            tree.root_node(),
            src_bytes,
            sym,
            &module_prefix,
            &by_simple,
            &known,
            &imports,
            workspace,
            enclosing_type.as_deref(),
            &mut edges,
        );
    }

    let mut out: Vec<CallEdge> = edges.into_iter().collect();
    out.sort_by(|a, b| {
        a.caller_qname
            .cmp(&b.caller_qname)
            .then_with(|| a.callee_qname.cmp(&b.callee_qname))
    });
    out
}

/// Import binding: local package alias → dotted qname prefix.
#[derive(Debug, Clone)]
struct ImportBinding {
    qname_prefix: String,
}

/// Collect import declarations from the source file.
///
/// Handles:
/// - `import "pkg/path"` → local name is last path segment
/// - `import alias "pkg/path"` → local name is alias
/// - Blank (`_`) and dot (`.`) imports are skipped.
fn parse_imports(source: &str, parser: &mut Parser) -> HashMap<String, ImportBinding> {
    let mut out = HashMap::new();
    let src_bytes = source.as_bytes();
    let tree = match parser.parse(src_bytes, None) {
        Some(t) => t,
        None => return out,
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "import_declaration" => {
                collect_import_declaration(child, src_bytes, &mut out);
            }
            _ => {}
        }
    }
    out
}

fn collect_import_declaration(
    node: Node<'_>,
    src: &[u8],
    out: &mut HashMap<String, ImportBinding>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_spec" => collect_import_spec(child, src, out),
            "import_spec_list" => {
                let mut c2 = child.walk();
                for spec in child.children(&mut c2) {
                    if spec.kind() == "import_spec" {
                        collect_import_spec(spec, src, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_import_spec(node: Node<'_>, src: &[u8], out: &mut HashMap<String, ImportBinding>) {
    // `path` field holds the quoted import path.
    let path_node = match node.child_by_field_name("path") {
        Some(n) => n,
        None => return,
    };
    let raw = match node_text(path_node, src) {
        Some(s) => s,
        None => return,
    };
    let path = raw.trim().trim_matches('"');
    if path.is_empty() {
        return;
    }

    // Derive the default local name from the last path segment.
    let default_local = path.rsplit('/').next().unwrap_or(path).to_string();

    // `name` field holds an optional alias identifier.
    let local = if let Some(name_node) = node.child_by_field_name("name") {
        let name = node_text(name_node, src).unwrap_or_default();
        let name = name.trim().to_string();
        if name == "_" || name == "." || name.is_empty() {
            return; // skip blank and dot imports
        }
        name
    } else {
        default_local.clone()
    };

    // Map path separators to dots for qname prefix.
    let qname_prefix = path.replace('/', ".");
    out.insert(local, ImportBinding { qname_prefix });
}

fn enclosing_type_qname(qname: &str, module_prefix: &str, known: &HashSet<&str>) -> Option<String> {
    let parts: Vec<&str> = qname.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    for end in (1..parts.len()).rev() {
        let candidate = parts[..end].join(".");
        if !module_prefix.is_empty() && !candidate.starts_with(module_prefix) {
            continue;
        }
        if known.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn collect_calls(
    node: Node<'_>,
    src: &[u8],
    sym: &ParsedSymbol,
    module_prefix: &str,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    imports: &HashMap<String, ImportBinding>,
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    out: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            if let Some(callee) = resolve_callee(
                func,
                src,
                module_prefix,
                by_simple,
                known,
                imports,
                workspace,
                enclosing_type,
            ) {
                out.insert(CallEdge {
                    caller_qname: sym.qname.clone(),
                    callee_qname: callee,
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(
            child,
            src,
            sym,
            module_prefix,
            by_simple,
            known,
            imports,
            workspace,
            enclosing_type,
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_callee(
    func: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    imports: &HashMap<String, ImportBinding>,
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
) -> Option<String> {
    match func.kind() {
        "identifier" => {
            let name = node_text(func, src)?;
            if let Some(q) = by_simple.get(&name) {
                return Some(q.clone());
            }
            if let Some(binding) = imports.get(&name) {
                if workspace.contains(&binding.qname_prefix) {
                    return Some(binding.qname_prefix.clone());
                }
            }
            None
        }
        "selector_expression" => {
            // `pkg.Function` or `receiver.Method`
            let operand = func.child_by_field_name("operand")?;
            let field = func.child_by_field_name("field")?;
            let field_name = node_text(field, src)?;

            if operand.kind() == "identifier" {
                let obj_name = node_text(operand, src)?;

                // self-like receiver method call: `r.Method`
                // Check if obj_name matches the receiver variable name.
                // For simplicity, treat any identifier that matches a known type as a type ref.
                let type_qname = if module_prefix.is_empty() {
                    obj_name.clone()
                } else {
                    format!("{module_prefix}.{obj_name}")
                };
                if known.contains(type_qname.as_str()) {
                    let candidate = format!("{type_qname}.{field_name}");
                    if known.contains(candidate.as_str()) {
                        return Some(candidate);
                    }
                }

                // Cross-module via import.
                if let Some(binding) = imports.get(&obj_name) {
                    let candidate = format!("{}.{field_name}", binding.qname_prefix);
                    if workspace.contains(&candidate) {
                        return Some(candidate);
                    }
                }

                // Intra-module: if obj is the enclosing receiver var, resolve to type.method.
                if let Some(class) = enclosing_type {
                    let candidate = format!("{class}.{field_name}");
                    if known.contains(candidate.as_str()) {
                        return Some(candidate);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_prefix_strips_go_and_leading_dot_slash() {
        assert_eq!(module_qname_prefix("payments/charge.go"), "payments.charge");
        assert_eq!(module_qname_prefix("./foo/bar.go"), "foo.bar");
        assert_eq!(module_qname_prefix("main.go"), "main");
    }

    #[test]
    fn parses_function_method_struct_interface() {
        let src = r#"
package payments

type Client struct{}
type Store interface{}

func NewClient() *Client { return &Client{} }

func (c *Client) Charge(amount int) error { return nil }
"#;
        let a = GoAdapter::new();
        let syms = a.parse_symbols("payments/client.go", src).unwrap();
        let qnames: Vec<_> = syms.iter().map(|s| s.qname.clone()).collect();
        assert!(
            qnames.contains(&"payments.client.Client".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"payments.client.Store".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"payments.client.NewClient".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"payments.client.Client.Charge".to_string()),
            "got {qnames:?}"
        );
        let charge = syms
            .iter()
            .find(|s| s.qname == "payments.client.Client.Charge")
            .unwrap();
        assert_eq!(charge.kind, SymbolKind::Method);
        let new_client = syms
            .iter()
            .find(|s| s.qname == "payments.client.NewClient")
            .unwrap();
        assert_eq!(new_client.kind, SymbolKind::Function);
    }

    #[test]
    fn infers_fs_read_and_net_out() {
        let body = r#"
func f() {
    f, _ := os.Open("/tmp/data")
    resp, _ := http.Get("https://api.example.com/v1")
}
"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::IoFsRead), "cats: {cats:?}");
        assert!(cats.contains(&EffectCategory::IoNetOut), "cats: {cats:?}");
    }

    #[test]
    fn infers_log_from_fmt_println() {
        let body = r#"func f() { fmt.Println("hello") }"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::Log), "cats: {cats:?}");
    }

    #[test]
    fn empty_when_no_patterns() {
        let body = "func add(a, b int) int { return a + b }";
        assert!(infer_effects_from_body(body).is_empty());
    }

    #[test]
    fn extracts_cross_module_call_edges() {
        let src = r#"
package main

import "payments/client"

func main() {
    c := client.NewClient()
}
"#;
        let a = GoAdapter::new();
        let syms = a.parse_symbols("main.go", src).unwrap();

        fn ws(qnames: &[&str]) -> WorkspaceSymbols {
            let mut ws = WorkspaceSymbols::default();
            for q in qnames {
                ws.qnames.insert((*q).to_string());
                ws.kinds.insert(
                    (*q).to_string(),
                    agentstatedeveloper_core::schema::SymbolKind::Function,
                );
            }
            ws
        }

        let workspace = ws(&["payments.client.NewClient", "main.main"]);
        let edges = a.extract_call_edges("main.go", src, &syms, &workspace);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&(
                "main.main".to_string(),
                "payments.client.NewClient".to_string()
            )),
            "expected main -> NewClient edge; got {pairs:?}",
        );
    }
}

#[cfg(test)]
mod service_endpoint_tests {
    use super::*;

    fn detect(src: &str) -> Vec<DetectedEndpoint> {
        let a = GoAdapter;
        let symbols = a.parse_symbols("svc.go", src).unwrap();
        a.infer_service_endpoints("svc.go", src, &symbols)
    }
    fn inbound(eps: &[DetectedEndpoint]) -> Vec<&DetectedEndpoint> {
        eps.iter()
            .filter(|e| e.direction == Direction::Inbound)
            .collect()
    }
    fn outbound(eps: &[DetectedEndpoint]) -> Vec<&DetectedEndpoint> {
        eps.iter()
            .filter(|e| e.direction == Direction::Outbound)
            .collect()
    }

    #[test]
    fn chi_and_gin_routes_inbound() {
        let chi = "func setup(r chi.Router) {\n\tr.Get(\"/users/{id}\", getUser)\n}\n";
        let inb = detect(chi);
        let i = inbound(&inb);
        assert_eq!(i.len(), 1, "{inb:?}");
        assert_eq!(i[0].contract, "http:GET /users/{}");

        let gin = "func reg(r *gin.Engine) {\n\tr.POST(\"/charge\", chargeHandler)\n}\n";
        let g = detect(gin);
        assert_eq!(inbound(&g)[0].contract, "http:POST /charge");
    }

    #[test]
    fn non_route_method_call_ignored() {
        // .Get on a map/cache without a path literal isn't a route.
        let src = "func f() {\n\tv := cache.Get(\"key\")\n\t_ = v\n}\n";
        assert!(detect(src).is_empty(), "{:?}", detect(src));
    }

    #[test]
    fn http_clients_outbound() {
        let src = "func call() {\n\thttp.Get(\"https://users.svc/users/{id}\")\n\thttp.Post(\"/charge\", \"application/json\", body)\n}\n";
        let eps = detect(src);
        let mut got: Vec<String> = outbound(&eps).iter().map(|e| e.contract.clone()).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                "http:GET /users/{}".to_string(),
                "http:POST /charge".to_string()
            ]
        );
    }

    #[test]
    fn new_request_method_and_url() {
        let src = "func call() {\n\treq, _ := http.NewRequest(\"DELETE\", \"https://api/things/{id}\", nil)\n\t_ = req\n}\n";
        let eps = detect(src);
        assert_eq!(outbound(&eps)[0].contract, "http:DELETE /things/{}");
    }

    #[test]
    fn new_request_with_context_skips_ctx_arg() {
        let src = "func call(ctx context.Context) {\n\treq, _ := http.NewRequestWithContext(ctx, \"PUT\", \"/items/{id}\", nil)\n\t_ = req\n}\n";
        let eps = detect(src);
        assert_eq!(outbound(&eps)[0].contract, "http:PUT /items/{}");
    }

    #[test]
    fn go_client_matches_python_server_contract() {
        // Cross-language: a Go client contract equals a Python/Express route's.
        let server = detect("func s(r chi.Router) { r.Get(\"/users/{id}\", h) }");
        let client = detect("func c() { http.Get(\"https://svc/users/:id\") }");
        assert_eq!(inbound(&server)[0].contract, outbound(&client)[0].contract);
        assert_eq!(inbound(&server)[0].contract, "http:GET /users/{}");
    }
}
