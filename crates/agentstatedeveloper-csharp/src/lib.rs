//! C# language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-c-sharp`. Parses namespaces, classes, interfaces,
//! structs, enums, records, methods, and constructors, then runs
//! substring-based effect inference.

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

/// C# language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct CSharpAdapter;

impl CSharpAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for CSharpAdapter {
    fn language(&self) -> &str {
        "csharp"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["cs"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .map_err(|e| AsdError::Parse(format!("failed to set c# language: {e}")))?;

        let src_bytes = source.as_bytes();
        let tree = parser
            .parse(src_bytes, None)
            .ok_or_else(|| AsdError::Parse(format!("failed to parse {file}")))?;

        let root = tree.root_node();
        let mut out = Vec::new();
        walk(root, src_bytes, "", &mut out);
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
        infer_service_endpoints_in_csharp(file, source, symbols)
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

// -----------------------------------------------------------------------------
// qname helpers
// -----------------------------------------------------------------------------

fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn child_by_field<'a>(node: Node<'a>, field: &str) -> Option<Node<'a>> {
    node.child_by_field_name(field)
}

fn join_qname(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", prefix, name)
    }
}

fn make_symbol(node: Node<'_>, src: &[u8], qname: String, kind: SymbolKind) -> ParsedSymbol {
    make_symbol_sig(node, src, qname, kind, None)
}

fn make_symbol_sig(
    node: Node<'_>,
    src: &[u8],
    qname: String,
    kind: SymbolKind,
    signature: Option<String>,
) -> ParsedSymbol {
    ParsedSymbol {
        qname,
        kind,
        start_line: node.start_position().row as u32 + 1,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32 + 1,
        end_col: node.end_position().column as u32,
        body: node_text(node, src).to_string(),
        signature,
        doc: None,
    }
}

/// Extract method/constructor signature: text up to (not including) body `{`.
fn extract_sig_before_brace(node: Node<'_>, src: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).ok()?;
    let bytes = text.as_bytes();
    let mut depth: i32 = 0;
    let mut i = 0;
    let mut sig_end = text.len();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b'{' if depth == 0 => {
                sig_end = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let sig = text[..sig_end].trim().to_string();
    if sig.is_empty() { None } else { Some(sig) }
}

// -----------------------------------------------------------------------------
// Symbol walking
// -----------------------------------------------------------------------------

fn walk(node: Node<'_>, src: &[u8], scope: &str, out: &mut Vec<ParsedSymbol>) {
    match node.kind() {
        "compilation_unit" => {
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, out);
            }
        }
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let ns_scope = join_qname(scope, name);
            // Walk the body
            if let Some(body) = child_by_field(node, "body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &ns_scope, out);
                }
            } else {
                // file-scoped namespace: remaining siblings in compilation_unit
                for i in 0..node.child_count() {
                    walk(node.child(i).unwrap(), src, &ns_scope, out);
                }
            }
        }
        "class_declaration"
        | "interface_declaration"
        | "struct_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "record_struct_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            out.push(make_symbol(node, src, qname.clone(), SymbolKind::Class));
            // Walk body for members
            if let Some(body) = child_by_field(node, "body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "method_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let sig = extract_sig_before_brace(node, src);
            out.push(make_symbol_sig(node, src, qname, SymbolKind::Method, sig));
        }
        "constructor_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let sig = extract_sig_before_brace(node, src);
            out.push(make_symbol_sig(node, src, qname, SymbolKind::Function, sig));
        }
        _ => {
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, out);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Effect inference
// -----------------------------------------------------------------------------

fn find_occurrences(body: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(idx) = body[from..].find(needle) {
        out.push(from + idx);
        from += idx + needle.len();
    }
    out
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

fn extract_first_string_literal(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes[0] != b'"' {
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
        if c == b'"' {
            return Some(out);
        }
        out.push(c as char);
        j += 1;
    }
    None
}

// -----------------------------------------------------------------------------
// Cross-service endpoint detection (t-014) — ASP.NET Core
//
// Inbound: attribute controllers ([HttpGet("x")] … with the class-level
// [Route("api/[controller]")] prefix; [controller] resolves to the controller
// class name) and minimal APIs (app.MapGet("/x", …)). Outbound: HttpClient
// GetAsync/PostAsync/…/GetFromJsonAsync<T>. Verbatim @"…" strings are literals;
// interpolated $"…" are skipped.
// -----------------------------------------------------------------------------

const HTTP_ATTRS: &[(&str, &str)] = &[
    ("HttpGet", "GET"),
    ("HttpPost", "POST"),
    ("HttpPut", "PUT"),
    ("HttpDelete", "DELETE"),
    ("HttpPatch", "PATCH"),
];

fn infer_service_endpoints_in_csharp(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
) -> Vec<DetectedEndpoint> {
    let prefix = cs_class_prefix(source);
    let mut out = Vec::new();
    for (idx, raw) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line = raw.trim();

        for (method, sub) in cs_method_attrs(line) {
            if let Some(owner) = owner_for_annotation(symbols, line_no) {
                out.push(DetectedEndpoint {
                    transport: Transport::Http,
                    direction: Direction::Inbound,
                    contract: http_contract(&method, &join_path(&prefix, &sub)),
                    owner_qname: owner.qname.clone(),
                    file: file.to_string(),
                    line: line_no,
                    confidence: 0.9,
                    note: None,
                });
            }
        }
        for (method, path) in cs_minimal_api(line) {
            if let Some(owner) = owner_for_body_line(symbols, line_no) {
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
        for (method, url) in cs_clients(line) {
            if let Some(owner) = owner_for_body_line(symbols, line_no) {
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

fn owner_for_body_line(symbols: &[ParsedSymbol], line: u32) -> Option<&ParsedSymbol> {
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .max_by_key(|s| s.start_line)
}

fn owner_for_annotation(symbols: &[ParsedSymbol], line: u32) -> Option<&ParsedSymbol> {
    if let Some(s) = owner_for_body_line(symbols, line) {
        return Some(s);
    }
    symbols
        .iter()
        .filter(|s| s.start_line > line && s.start_line <= line + 6)
        .min_by_key(|s| s.start_line)
}

/// Class-level `[Route("…")]` prefix; `[controller]` resolves to the controller
/// class name (minus the `Controller` suffix).
fn cs_class_prefix(source: &str) -> String {
    let controller = cs_controller_name(source);
    for line in source.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("[Route(") {
            if let Some(p) = first_cs_string(rest) {
                return p.replace("[controller]", &controller);
            }
        }
        if t.contains("class ") {
            break;
        }
    }
    String::new()
}

fn cs_controller_name(source: &str) -> String {
    for line in source.lines() {
        if let Some(pos) = line.find("class ") {
            let name: String = line[pos + 6..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return name.strip_suffix("Controller").unwrap_or(&name).to_string();
        }
    }
    String::new()
}

/// `(METHOD, sub_path)` for a method-level HTTP attribute line.
fn cs_method_attrs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (attr, method) in HTTP_ATTRS {
        if line.starts_with(&format!("[{attr}")) {
            let sub = line
                .find('(')
                .and_then(|o| first_cs_string(&line[o + 1..]))
                .unwrap_or_default();
            out.push((method.to_string(), sub));
        }
    }
    out
}

/// Minimal-API routes `app.MapGet("/x", …)`.
fn cs_minimal_api(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (m, method) in [
        ("MapGet", "GET"),
        ("MapPost", "POST"),
        ("MapPut", "PUT"),
        ("MapDelete", "DELETE"),
        ("MapPatch", "PATCH"),
    ] {
        if let Some(args) = cs_call_args(line, m) {
            if let Some(path) = first_cs_string(args) {
                if path.starts_with('/') {
                    out.push((method.to_string(), path));
                }
            }
        }
    }
    out
}

/// HttpClient calls. URLs must look like a URL so unrelated `*Async` methods
/// with a non-URL string argument don't match.
fn cs_clients(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let methods = [
        ("GetAsync", "GET"),
        ("GetStringAsync", "GET"),
        ("GetFromJsonAsync", "GET"),
        ("PostAsync", "POST"),
        ("PostAsJsonAsync", "POST"),
        ("PutAsync", "PUT"),
        ("PutAsJsonAsync", "PUT"),
        ("DeleteAsync", "DELETE"),
        ("PatchAsync", "PATCH"),
    ];
    for (m, method) in methods {
        if let Some(args) = cs_call_args(line, m) {
            if let Some(url) = first_cs_string(args) {
                if looks_like_url(&url) {
                    out.push((method.to_string(), url));
                }
            }
        }
    }
    out
}

/// Returns the argument substring after `.method(` or `.method<…>(`.
fn cs_call_args<'a>(line: &'a str, method: &str) -> Option<&'a str> {
    let pos = line.find(&format!(".{method}"))?;
    let mut rest = &line[pos + method.len() + 1..];
    if rest.starts_with('<') {
        let close = rest.find('>')?;
        rest = &rest[close + 1..];
    }
    rest.strip_prefix('(')
}

fn join_path(prefix: &str, sub: &str) -> String {
    let p = prefix.trim_end_matches('/');
    let s = sub.trim_start_matches('/');
    match (p.is_empty(), s.is_empty()) {
        (true, true) => "/".to_string(),
        (false, true) => p.to_string(),
        (true, false) => format!("/{s}"),
        (false, false) => format!("{p}/{s}"),
    }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with('/') || s.contains("://")
}

/// First string literal: `"…"` or verbatim `@"…"`. Interpolated `$"…"` → None.
fn first_cs_string(s: &str) -> Option<String> {
    let s = s.trim_start();
    if s.starts_with('$') {
        return None;
    }
    let offset = if s.starts_with("@\"") { 1 } else { 0 };
    if s.as_bytes().get(offset).copied()? != b'"' {
        return None;
    }
    let inner = &s[offset + 1..];
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // FS Read
    let fs_read_needles = [
        "File.ReadAll",
        "File.ReadLines(",
        "File.OpenRead(",
        "File.Open(",
        "new FileStream(",
        "new StreamReader(",
        "Directory.GetFiles(",
        "Directory.EnumerateFiles(",
    ];
    if let Some(note) = first_match_note(body, &fs_read_needles) {
        effects.push(Effect {
            effect: EffectCategory::IoFsRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // FS Write
    let fs_write_needles = [
        "File.WriteAll",
        "File.AppendAll",
        "File.Create(",
        "File.OpenWrite(",
        "new StreamWriter(",
        "Directory.CreateDirectory(",
        "Directory.Delete(",
    ];
    if let Some(note) = first_match_note(body, &fs_write_needles) {
        effects.push(Effect {
            effect: EffectCategory::IoFsWrite,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Network
    let net_needles = [
        "new HttpClient(",
        "HttpClient(",
        "_httpClient.",
        "httpClient.",
        "client.GetAsync(",
        "client.PostAsync(",
        "client.SendAsync(",
        "RestClient(",
        "new RestClient(",
        "WebClient(",
        "new WebClient(",
        "WebRequest.Create(",
        "SocketsHttpHandler",
    ];
    let mut net_hosts: Vec<String> = Vec::new();
    let mut net_note: Option<String> = None;
    for needle in net_needles {
        if body.contains(needle) && net_note.is_none() {
            net_note = first_matching_line(body, &[needle]);
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

    // Database
    let db_needles = [
        "new SqlConnection(",
        "SqlConnection(",
        ".ExecuteReader(",
        ".ExecuteScalar(",
        ".ExecuteNonQuery(",
        "dbContext.",
        "context.SaveChanges",
        "context.Add(",
        "context.Remove(",
        "context.Find(",
        "SqlCommand(",
        "new SqlCommand(",
        ".Query<",
        ".Execute(",
        "connection.QueryAsync",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        let has_write = body.contains(".ExecuteNonQuery(")
            || body.contains("context.SaveChanges")
            || body.contains("context.Add(")
            || body.contains("context.Remove(")
            || body.contains(".Execute(");
        let has_read = body.contains(".ExecuteReader(")
            || body.contains(".ExecuteScalar(")
            || body.contains("context.Find(")
            || body.contains(".Query<")
            || body.contains("connection.QueryAsync");
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

    // Process spawn
    let proc_needles = ["Process.Start(", "new Process(", "ProcessStartInfo("];
    if let Some(note) = first_match_note(body, &proc_needles) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Env read
    let env_needles = [
        "Environment.GetEnvironmentVariable(",
        "Environment.GetEnvironmentVariables(",
    ];
    if let Some(note) = first_match_note(body, &env_needles) {
        let mut vars: Vec<String> = Vec::new();
        for off in find_occurrences(body, "Environment.GetEnvironmentVariable(") {
            let args = &body[off + "Environment.GetEnvironmentVariable(".len()..];
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

    // Logging
    let log_needles = [
        "Console.Write",
        "_logger.",
        "logger.",
        "Log.Information(",
        "Log.Warning(",
        "Log.Error(",
        "Log.Debug(",
        "_log.",
        "ILogger",
        "NLog.",
        "log4net.",
    ];
    if let Some(note) = first_match_note(body, &log_needles) {
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Time sleep
    let sleep_needles = ["Thread.Sleep(", "Task.Delay(", "await Task.Delay("];
    if let Some(note) = first_match_note(body, &sleep_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Time read
    let time_needles = [
        "DateTime.Now",
        "DateTime.UtcNow",
        "DateTime.Today",
        "DateTimeOffset.Now",
        "DateTimeOffset.UtcNow",
        "Stopwatch.GetTimestamp()",
        "Stopwatch.StartNew()",
        "Environment.TickCount",
        "TimeProvider.System",
    ];
    if let Some(note) = first_match_note(body, &time_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Random
    let rand_needles = [
        "new Random(",
        "Random.Shared",
        "RandomNumberGenerator",
        "Guid.NewGuid()",
        "new RNGCryptoServiceProvider()",
    ];
    if let Some(note) = first_match_note(body, &rand_needles) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Throw
    let throw_needles = ["throw new ", "throw;", "throw ex;", "throw e;"];
    if let Some(note) = first_match_note(body, &throw_needles) {
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    effects
}

// -----------------------------------------------------------------------------
// Call-edge extraction
// -----------------------------------------------------------------------------

/// Import binding: `using` alias or fully-qualified type.
#[derive(Debug, Clone)]
struct UsingBinding {
    /// Fully-qualified namespace or type, e.g. `MyApp.Payments`.
    fqn: String,
    /// Simple alias when the `using` is `using Alias = Fully.Qualified.Type`.
    alias: Option<String>,
}

fn parse_usings(source: &str) -> Vec<UsingBinding> {
    let mut out = Vec::new();
    for line in source.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("using ") {
            let rest = rest.trim_end_matches(';').trim();
            // Skip `using static` for simplicity
            if rest.starts_with("static ") {
                continue;
            }
            if let Some((alias, fqn)) = rest.split_once('=') {
                out.push(UsingBinding {
                    fqn: fqn.trim().to_string(),
                    alias: Some(alias.trim().to_string()),
                });
            } else if !rest.is_empty() {
                out.push(UsingBinding {
                    fqn: rest.to_string(),
                    alias: None,
                });
            }
        }
    }
    out
}

fn enclosing_type_qname(qname: &str, known: &HashSet<&str>) -> Option<String> {
    let idx = qname.rfind('.')?;
    let parent = &qname[..idx];
    if known.contains(parent) {
        Some(parent.to_string())
    } else {
        None
    }
}

fn collect_invocations(
    node: Node<'_>,
    src: &[u8],
    sym: &ParsedSymbol,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    usings: &[UsingBinding],
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    edges: &mut HashSet<CallEdge>,
) {
    if node.kind() == "invocation_expression" {
        // The function is the first child (a member_access_expression or identifier)
        if let Some(func_node) = node.child(0) {
            let (object_text, method_text) = if func_node.kind() == "member_access_expression" {
                // obj.Method
                let obj = func_node.child(0).map(|n| node_text(n, src)).unwrap_or("");
                let method = child_by_field(func_node, "name")
                    .map(|n| node_text(n, src))
                    .unwrap_or("");
                (obj, method)
            } else {
                // bare method call
                ("", node_text(func_node, src))
            };

            if !method_text.is_empty() {
                let callee = if object_text.is_empty() {
                    // Bare call — try enclosing type then module-level
                    if let Some(et) = enclosing_type {
                        let q = format!("{}.{}", et, method_text);
                        if known.contains(q.as_str()) {
                            Some(q)
                        } else {
                            by_simple.get(method_text).cloned()
                        }
                    } else {
                        by_simple.get(method_text).cloned()
                    }
                } else {
                    // Qualified: look up object type in usings
                    let alias_match = usings
                        .iter()
                        .find(|u| u.alias.as_deref() == Some(object_text));
                    let ns_match = usings
                        .iter()
                        .find(|u| u.fqn.ends_with(&format!(".{}", object_text)));

                    if let Some(binding) = alias_match.or(ns_match) {
                        // alias points to a type, so method is on that type
                        let q = format!("{}.{}", binding.fqn, method_text);
                        if known.contains(q.as_str()) || workspace.contains(&q) {
                            Some(q)
                        } else {
                            None
                        }
                    } else {
                        // Try workspace lookup with known usings as namespace prefixes
                        let mut found: Option<String> = None;
                        for u in usings {
                            let q = format!("{}.{}.{}", u.fqn, object_text, method_text);
                            if known.contains(q.as_str()) || workspace.contains(&q) {
                                found = Some(q);
                                break;
                            }
                        }
                        found
                    }
                };

                if let Some(callee_qname) = callee {
                    if callee_qname != sym.qname {
                        edges.insert(CallEdge {
                            caller_qname: sym.qname.clone(),
                            callee_qname,
                        });
                    }
                }
            }
        }
    }

    for i in 0..node.child_count() {
        collect_invocations(
            node.child(i).unwrap(),
            src,
            sym,
            by_simple,
            known,
            usings,
            workspace,
            enclosing_type,
            edges,
        );
    }
}

fn extract_call_edges_impl(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
    workspace: &WorkspaceSymbols,
) -> Vec<CallEdge> {
    let _ = file;
    let known: HashSet<&str> = symbols.iter().map(|s| s.qname.as_str()).collect();

    let mut by_simple: HashMap<String, String> = HashMap::new();
    for s in symbols {
        let simple = s.qname.rsplit('.').next().unwrap_or(&s.qname).to_string();
        by_simple.entry(simple).or_insert_with(|| s.qname.clone());
    }

    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let usings = parse_usings(source);
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
        let enclosing_type = enclosing_type_qname(&sym.qname, &known);
        collect_invocations(
            tree.root_node(),
            src_bytes,
            sym,
            &by_simple,
            &known,
            &usings,
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

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentstatedeveloper_core::adapter::{LanguageAdapter, WorkspaceSymbols};

    fn adapter() -> CSharpAdapter {
        CSharpAdapter::new()
    }

    #[test]
    fn parses_namespace_class_method_enum() {
        let src = r#"
namespace MyApp.Payments
{
    public class PaymentService
    {
        public PaymentService() {}

        public async Task<Receipt> ChargeAsync(string customerId, decimal amount)
        {
            return new Receipt();
        }

        private void LogCharge(string msg) { }
    }

    public interface IGateway
    {
        void Process(Payment p);
    }

    public enum Currency { USD, EUR, GBP }
}
"#;
        let syms = adapter().parse_symbols("PaymentService.cs", src).unwrap();
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(
            qnames.contains(&"MyApp.Payments.PaymentService"),
            "{qnames:?}"
        );
        assert!(
            qnames.contains(&"MyApp.Payments.PaymentService.ChargeAsync"),
            "{qnames:?}"
        );
        assert!(
            qnames.contains(&"MyApp.Payments.PaymentService.LogCharge"),
            "{qnames:?}"
        );
        assert!(qnames.contains(&"MyApp.Payments.IGateway"), "{qnames:?}");
        assert!(qnames.contains(&"MyApp.Payments.Currency"), "{qnames:?}");
    }

    #[test]
    fn infers_fs_read_and_net_out() {
        let src = r#"
using System.Net.Http;
namespace App {
    public class Fetcher {
        public async Task<string> Fetch() {
            string data = File.ReadAllText("/tmp/config.json");
            var client = new HttpClient();
            var resp = await client.GetAsync("https://api.example.com/v1/data");
            return data;
        }
    }
}
"#;
        let syms = adapter().parse_symbols("Fetcher.cs", src).unwrap();
        let fetch = syms.iter().find(|s| s.qname.ends_with(".Fetch")).unwrap();
        let effs = adapter().infer_effects("", fetch);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::IoFsRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::IoNetOut), "{cats:?}");
    }

    #[test]
    fn infers_db_read_and_write() {
        let src = r#"
namespace App {
    public class UserRepo {
        public User Find(int id) {
            return dbContext.Find<User>(id);
        }
        public void Save(User u) {
            dbContext.Add(u);
            dbContext.SaveChanges();
        }
    }
}
"#;
        let syms = adapter().parse_symbols("UserRepo.cs", src).unwrap();
        let find = syms.iter().find(|s| s.qname.ends_with(".Find")).unwrap();
        let save = syms.iter().find(|s| s.qname.ends_with(".Save")).unwrap();
        let find_effs = adapter().infer_effects("", find);
        let save_effs = adapter().infer_effects("", save);
        assert!(
            find_effs
                .iter()
                .any(|e| e.effect == EffectCategory::IoDbRead)
        );
        assert!(
            save_effs
                .iter()
                .any(|e| e.effect == EffectCategory::IoDbWrite)
        );
    }

    #[test]
    fn infers_log_and_env() {
        let src = r#"
namespace App {
    public class Config {
        private readonly ILogger<Config> _logger;
        public string Load() {
            string env = Environment.GetEnvironmentVariable("APP_ENV");
            _logger.LogInformation("Loading for {Env}", env);
            return env;
        }
    }
}
"#;
        let syms = adapter().parse_symbols("Config.cs", src).unwrap();
        let load = syms.iter().find(|s| s.qname.ends_with(".Load")).unwrap();
        let effs = adapter().infer_effects("", load);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::EnvRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::Log), "{cats:?}");
        // Check env var extraction
        let env_eff = effs
            .iter()
            .find(|e| e.effect == EffectCategory::EnvRead)
            .unwrap();
        if let Some(vars) = env_eff.qualifiers.get("vars") {
            let vars: Vec<&str> = vars
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(vars.contains(&"APP_ENV"), "{vars:?}");
        }
    }

    #[test]
    fn empty_when_no_patterns() {
        let src = r#"
namespace App {
    public class Math {
        public int Add(int a, int b) { return a + b; }
    }
}
"#;
        let syms = adapter().parse_symbols("Math.cs", src).unwrap();
        let add = syms.iter().find(|s| s.qname.ends_with(".Add")).unwrap();
        let effs = adapter().infer_effects("", add);
        assert!(effs.is_empty(), "{effs:?}");
    }

    #[test]
    fn extracts_cross_module_call_edges() {
        let src = r#"
using MyApp.Payments;

namespace MyApp.Orders {
    public class OrderService {
        public void PlaceOrder(Order order) {
            ChargeService.Charge(order.CustomerId, order.Total);
        }
    }

    public class ChargeService {
        public static void Charge(string customerId, decimal amount) { }
    }
}
"#;
        let ws = WorkspaceSymbols {
            qnames: ["MyApp.Payments.ChargeService.Charge".to_string()].into(),
            kinds: HashMap::new(),
            ..Default::default()
        };
        let syms = adapter().parse_symbols("OrderService.cs", src).unwrap();
        let edges = adapter().extract_call_edges("OrderService.cs", src, &syms, &ws);
        // Local ChargeService.Charge should be found
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".PlaceOrder") && e.callee_qname.ends_with(".Charge")
        });
        assert!(found, "expected call edge to Charge; got: {edges:?}");
    }
}

#[cfg(test)]
mod service_endpoint_tests {
    use super::*;
    use agentstatedeveloper_core::adapter::LanguageAdapter;

    fn detect(src: &str) -> Vec<DetectedEndpoint> {
        let a = CSharpAdapter;
        let symbols = a.parse_symbols("C.cs", src).unwrap();
        a.infer_service_endpoints("C.cs", src, &symbols)
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
    fn attribute_controller_with_controller_token_prefix() {
        let src = "\
[ApiController]
[Route(\"api/[controller]\")]
public class UsersController : ControllerBase {
    [HttpGet(\"{id}\")]
    public IActionResult Get(int id) { return Ok(); }

    [HttpPost]
    public IActionResult Create() { return Ok(); }
}
";
        let eps = detect(src);
        let mut got: Vec<String> = inbound(&eps).iter().map(|e| e.contract.clone()).collect();
        got.sort();
        // [controller] → "Users" → normalized lowercase /api/users.
        assert_eq!(
            got,
            vec![
                "http:GET /api/users/{}".to_string(),
                "http:POST /api/users".to_string()
            ],
            "{eps:?}"
        );
    }

    #[test]
    fn minimal_api_routes() {
        let src = "public class P {\n  void Cfg() {\n    app.MapGet(\"/health\", () => \"ok\");\n  }\n}\n";
        let inb_eps = detect(src);
        assert_eq!(inbound(&inb_eps)[0].contract, "http:GET /health");
    }

    #[test]
    fn httpclient_clients_incl_generic() {
        let src = "public class S {\n  async Task Call() {\n    await client.GetFromJsonAsync<User>(\"https://users.svc/users/{id}\");\n    await client.PostAsync(\"/charge\", body);\n  }\n}\n";
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
    fn cs_server_matches_other_language_client() {
        let server = detect(
            "[Route(\"api/[controller]\")]\npublic class UsersController {\n  [HttpGet(\"{id}\")]\n  public IActionResult Get() { return Ok(); }\n}\n",
        );
        assert_eq!(inbound(&server)[0].contract, "http:GET /api/users/{}");
        assert_eq!(
            inbound(&server)[0].contract,
            http_contract("get", "https://svc/api/users/:id")
        );
    }
}
