//! Swift language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-swift`. Parses classes, structs, enums, protocols,
//! extensions, and functions/methods, then runs substring-based effect inference.

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
use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_swift() -> *const ();
}
const SWIFT: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_swift) };

/// Swift language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct SwiftAdapter;

impl SwiftAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for SwiftAdapter {
    fn language(&self) -> &str {
        "swift"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["swift"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&SWIFT.into())
            .map_err(|e| AsdError::Parse(format!("failed to set swift language: {e}")))?;

        let src_bytes = source.as_bytes();
        let tree = parser
            .parse(src_bytes, None)
            .ok_or_else(|| AsdError::Parse(format!("failed to parse {file}")))?;

        let file_prefix = file_qname_prefix(file);
        let root = tree.root_node();
        let mut out = Vec::new();
        walk(root, src_bytes, &file_prefix, &mut out);
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
        infer_service_endpoints_in_swift(file, source, symbols)
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

    fn extract_property_types(
        &self,
        symbols: &[ParsedSymbol],
    ) -> std::collections::HashMap<String, String> {
        build_property_type_map(symbols)
    }
}

// -----------------------------------------------------------------------------
// qname helpers
// -----------------------------------------------------------------------------

/// Walk path components and return the tail after the first `Sources`,
/// `Source`, or `src` segment.  Falls back to `path` unchanged when no such
/// segment is present (non-SPM layouts, flat directories, etc.).
///
/// This makes qnames stable across different `asd index` invocation roots: for
/// SPM projects the stable anchor is the SPM target name (the directory
/// immediately inside `Sources/`), not the filesystem path from the index root.
///
/// Examples:
/// - `Packages/SequencerCore/Sources/Engine/DriftCompiler`
///   → `Engine/DriftCompiler`
/// - `App/ExampleFlow/Sources/ExampleFlow/ExampleFlowViewModel`
///   → `ExampleFlow/ExampleFlowViewModel`
/// - `Sources/Engine/DriftCompiler`  (already at Sources)
///   → `Engine/DriftCompiler`
/// - `Engine/DriftCompiler`  (no Sources segment; no-op)
///   → `Engine/DriftCompiler`
fn strip_sources_prefix(path: &str) -> &str {
    let mut offset = 0usize;
    for part in path.split('/') {
        if matches!(part, "Sources" | "Source" | "src") {
            let after = offset + part.len() + 1; // skip segment + trailing slash
            if after < path.len() {
                return &path[after..];
            }
        }
        offset += part.len() + 1;
    }
    path
}

/// Derive the stable qname prefix from a Swift file path.
///
/// Strips everything up to and including the first `Sources/` segment so that
/// the prefix is anchored to the SPM target name, not the indexing root.
///
/// `App/ExampleFlow/Sources/ExampleFlow/ExampleFlowViewModel.swift`
///   → `ExampleFlow.ExampleFlowViewModel`
fn file_qname_prefix(file: &str) -> String {
    let s = file.strip_prefix("./").unwrap_or(file);
    let s = s.strip_suffix(".swift").unwrap_or(s);
    let s = strip_sources_prefix(s);
    let joined = s.replace('/', ".");
    dedupe_consecutive_segments(&joined)
}

/// Collapse consecutive identical path segments after the `/` → `.`
/// join. Xcode projects routinely nest the project name (no SPM
/// `Sources/` marker to strip), producing paths like
/// `App/ExampleFlow/ExampleFlow/ExampleFlow/Views/DriftPad/...`
/// which yield qnames like
/// `App.ExampleFlow.ExampleFlow.ExampleFlow.Views.DriftPad.DriftPadView`.
/// The triple `ExampleFlow` is noise: it inflates token count for
/// every Swift result, distorts BM25 against multi-word queries
/// that match the duplicated stem, and lengthens display output.
///
/// Dedup is conservative — only CONSECUTIVE identical segments
/// collapse. A legitimate qname like `Foo.bar.Foo.baz` (non-
/// adjacent repeat) is unaffected. Cross-language safe: Python /
/// Rust / TS adapters use their own qname builders.
fn dedupe_consecutive_segments(qname: &str) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(qname.split('.').count());
    for seg in qname.split('.') {
        if out.last() != Some(&seg) {
            out.push(seg);
        }
    }
    out.join(".")
}

fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn child_by_field<'a>(node: Node<'a>, field: &str) -> Option<Node<'a>> {
    node.child_by_field_name(field)
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    for i in 0..node.child_count() {
        let c = node.child(i).unwrap();
        if c.kind() == kind {
            return Some(c);
        }
    }
    None
}

fn join_qname(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", prefix, name)
    }
}

fn make_symbol(
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

/// Extract the function signature: everything from the declaration start up to
/// (but not including) the opening `{` of the function body.
///
/// Tracks `()` and `[]` depth to avoid splitting on closure literals used as
/// default parameter values, e.g. `func foo(cb: () -> Void = { })`.
/// For protocol requirements with no body, the full declaration text is returned.
fn extract_function_signature(node: Node<'_>, src: &[u8]) -> Option<String> {
    let start = node.start_byte();
    let end = node.end_byte();
    let text = std::str::from_utf8(&src[start..end]).ok()?;
    let bytes = text.as_bytes();

    let mut depth: i32 = 0;
    let mut i = 0;
    let mut sig_end = text.len(); // default: whole text (no body brace found)

    while i < bytes.len() {
        match bytes[i] {
            // Skip string literals to avoid confusing `{` inside them.
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
        "source_file" => {
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, out);
            }
        }
        // In swift 0.5.0 grammar, class/struct/enum/extension all use
        // `class_declaration` with a `declaration_kind` field distinguishing them.
        "class_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let decl_kind = child_by_field(node, "declaration_kind")
                .map(|n| node_text(n, src))
                .unwrap_or("class");
            let qname = join_qname(scope, name);
            // Skip extension re-declarations to avoid duplicating the type symbol.
            if decl_kind != "extension" {
                out.push(make_symbol(
                    node,
                    src,
                    qname.clone(),
                    SymbolKind::Class,
                    None,
                ));
            }
            // Walk body
            if let Some(body) = child_by_field(node, "body")
                .or_else(|| find_child_by_kind(node, "class_body"))
                .or_else(|| find_child_by_kind(node, "enum_class_body"))
            {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "protocol_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            out.push(make_symbol(
                node,
                src,
                qname.clone(),
                SymbolKind::Class,
                None,
            ));
            if let Some(body) =
                child_by_field(node, "body").or_else(|| find_child_by_kind(node, "protocol_body"))
            {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "function_declaration" | "protocol_function_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let kind = if scope.contains('.') {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sig = extract_function_signature(node, src);
            out.push(make_symbol(node, src, qname, kind, sig));
        }
        "init_declaration" => {
            let qname = join_qname(scope, "init");
            let sig = extract_function_signature(node, src);
            out.push(make_symbol(node, src, qname, SymbolKind::Function, sig));
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
// Cross-service endpoint detection (t-015) — Vapor routes.
//
// Vapor routes are `app.<verb>(component, component, …)` where components are
// string path segments (`"users"`, `":id"`), joined with '/'. A route is
// distinguished from an ordinary `.get("key")` by a handler indicator: a
// trailing `{` closure or a `use:` argument. Clients (URLSession) are indirect
// and deferred.
// -----------------------------------------------------------------------------

const SW_VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];

fn infer_service_endpoints_in_swift(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
) -> Vec<DetectedEndpoint> {
    let mut out = Vec::new();
    for (idx, raw) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let line = raw.trim();
        if let Some((method, path)) = vapor_route(line) {
            if let Some(owner) = sw_owner_for_body(symbols, line_no) {
                out.push(DetectedEndpoint {
                    transport: Transport::Http,
                    direction: Direction::Inbound,
                    contract: http_contract(&method, &path),
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

fn sw_owner_for_body(symbols: &[ParsedSymbol], line: u32) -> Option<&ParsedSymbol> {
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .max_by_key(|s| s.start_line)
}

/// `app.get("users", ":id") { … }` → (GET, /users/:id). Requires a handler
/// indicator (`use:` or a trailing `{`) so plain `.get("key")` isn't a route.
fn vapor_route(line: &str) -> Option<(String, String)> {
    for verb in SW_VERBS {
        let needle = format!(".{verb}(");
        let Some(pos) = line.find(&needle) else {
            continue;
        };
        let args = &line[pos + needle.len()..];
        // A route has a handler: a `use:` argument or a `{ … }` closure (which
        // may be `{ req in` mid-line, not just a trailing brace).
        let is_route = args.contains("use:") || args.contains('{');
        if !is_route {
            continue;
        }
        let comps = swift_path_components(args);
        if comps.is_empty() {
            continue;
        }
        return Some((verb.to_uppercase(), format!("/{}", comps.join("/"))));
    }
    None
}

/// Leading string-literal path components, stopping at the first non-string
/// argument (`use:`, a closure, etc.).
fn swift_path_components(args: &str) -> Vec<String> {
    let mut comps = Vec::new();
    for part in args.split(',') {
        let p = part.trim();
        if p.starts_with('"') {
            match first_swift_string(p) {
                Some(lit) => comps.push(lit),
                None => break,
            }
        } else {
            break;
        }
    }
    comps
}

/// First `"…"` literal; rejects Swift interpolation `\(…)`.
fn first_swift_string(s: &str) -> Option<String> {
    let s = s.trim_start();
    if !s.starts_with('"') {
        return None;
    }
    let inner = &s[1..];
    let end = inner.find('"')?;
    let val = &inner[..end];
    if val.contains("\\(") {
        return None;
    }
    Some(val.to_string())
}

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // FS Read
    let fs_read_needles = [
        "FileManager.default",
        "contentsOfFile:",
        "contentsOfDirectory(",
        "String(contentsOf:",
        "Data(contentsOf:",
        "FileHandle(forReadingAtPath:",
        "FileHandle.init(forReadingFrom:",
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
        "write(to:",
        "write(toFile:",
        "createFile(atPath:",
        "createDirectory(",
        "moveItem(",
        "copyItem(",
        "removeItem(",
        "FileHandle(forWritingAtPath:",
        "FileHandle.init(forWritingTo:",
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
        "URLSession.",
        "URLSession.shared",
        "dataTask(with:",
        "AF.request(",
        "Alamofire.",
        "URLRequest(",
        "session.dataTask(",
        "session.downloadTask(",
        "session.uploadTask(",
        "Moya",
        "Apollo.",
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
        "CoreData",
        "NSManagedObject",
        "NSFetchRequest(",
        "viewContext.",
        "context.save(",
        "context.fetch(",
        "GRDB",
        "SQLite.",
        "Realm(",
        "realm.write {",
        "realm.objects(",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        let has_write = body.contains("context.save(")
            || body.contains("realm.write {")
            || body.contains(".insert(")
            || body.contains(".delete(");
        let has_read = body.contains("context.fetch(")
            || body.contains("NSFetchRequest(")
            || body.contains("realm.objects(")
            || body.contains(".filter(");
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
    let proc_needles = ["Process(", "Process.launchedProcess(", "NSTask("];
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
        "ProcessInfo.processInfo.environment",
        "ProcessInfo.processInfo.environment[",
        "getenv(",
    ];
    if let Some(note) = first_match_note(body, &env_needles) {
        let mut vars: Vec<String> = Vec::new();
        for off in find_occurrences(body, "ProcessInfo.processInfo.environment[") {
            let args = &body[off + "ProcessInfo.processInfo.environment[".len()..];
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
        "print(",
        "debugPrint(",
        "NSLog(",
        "os.log",
        "Logger(",
        "os_log(",
        "logger.debug(",
        "logger.info(",
        "logger.error(",
        "Logger.shared",
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
    let sleep_needles = [
        "Thread.sleep(",
        "usleep(",
        "sleep(",
        "Task.sleep(",
        "try await Task.sleep(",
    ];
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
        "Date()",
        "Date.now",
        "Date.timeIntervalSinceReferenceDate",
        "CFAbsoluteTimeGetCurrent()",
        "DispatchTime.now()",
        "Clock.now",
        "ProcessInfo.processInfo.systemUptime",
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
        ".random(",
        ".shuffled(",
        "Int.random(",
        "Double.random(",
        "Float.random(",
        "Bool.random()",
        "arc4random(",
        "arc4random_uniform(",
        "UUID()",
        "SystemRandomNumberGenerator()",
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
    let throw_needles = [
        "throw ",
        "fatalError(",
        "preconditionFailure(",
        "assertionFailure(",
    ];
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
// Property-to-type map (for instance property call resolution)
// -----------------------------------------------------------------------------

/// Strip leading Swift property modifiers/attributes so `parse_property_line`
/// sees `let`/`var` at the start.
fn strip_property_prefixes(s: &str) -> &str {
    let mut rest = s;
    loop {
        let prev = rest;
        // @Attribute / @Attribute(args)
        if rest.starts_with('@') {
            let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let after_attr = rest[end..].trim_start();
            if after_attr.starts_with('(') {
                let close = after_attr
                    .find(')')
                    .unwrap_or(after_attr.len().saturating_sub(1));
                rest = after_attr[close + 1..].trim_start();
            } else {
                rest = after_attr;
            }
            continue;
        }
        for prefix in &[
            "private(set) ",
            "public(set) ",
            "internal(set) ",
            "private ",
            "public ",
            "internal ",
            "fileprivate ",
            "open ",
            "weak ",
            "unowned(safe) ",
            "unowned(unsafe) ",
            "unowned ",
            "lazy ",
            "static ",
            "override ",
            "final ",
            "nonisolated ",
            "isolated ",
        ] {
            if let Some(after) = rest.strip_prefix(prefix) {
                rest = after.trim_start();
                break;
            }
        }
        if rest == prev {
            break;
        }
    }
    rest
}

/// Try to parse a single line as a stored Swift property declaration.
/// Returns `(property_name, base_type_name)` on success.
///
/// Handled patterns:
/// ```swift
/// let pool: DriftSynthPool
/// var compiler: DriftCompiler?
/// @Published var items: [Item]
/// private weak var delegate: SomeDelegate?
/// let pool = DriftSynthPool()          // inferred from initializer
/// let scheduler = BatchScheduler.make() // inferred from factory call
/// ```
fn parse_property_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("func ")
        || trimmed.starts_with("init(")
        || trimmed.starts_with("deinit")
        || trimmed.starts_with("class ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("enum ")
        || trimmed.starts_with("case ")
        || trimmed.starts_with("typealias ")
        || trimmed.starts_with("subscript")
    {
        return None;
    }

    let rest = strip_property_prefixes(trimmed);

    // Must start with `let` or `var`
    let after_binding = rest
        .strip_prefix("let ")
        .or_else(|| rest.strip_prefix("var "))
        .or_else(|| rest.strip_prefix("let\t"))
        .or_else(|| rest.strip_prefix("var\t"))?
        .trim_start();

    // Property name: identifier chars
    let name_end = after_binding
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(after_binding.len());
    let name = &after_binding[..name_end];
    if name.is_empty() {
        return None;
    }

    let rest_after_name = after_binding[name_end..].trim_start();

    // --- Path A: explicit type annotation `let name: TypeName` ---
    if let Some(after_colon) = rest_after_name.strip_prefix(':') {
        let type_str = after_colon.trim_start();

        // Stop at `=` or `{`
        let type_end = type_str
            .find(|c: char| c == '=' || c == '{')
            .unwrap_or(type_str.len());
        let raw_type = type_str[..type_end].trim();

        // Strip optional/implicit-unwrap markers
        let type_name = raw_type.trim_end_matches('?').trim_end_matches('!').trim();

        // Strip Swift 5.7+ `any`/`some` existential/opaque type markers so
        // `var scheduler: any SchedulerProtocol` → `SchedulerProtocol`.
        let type_name = type_name
            .strip_prefix("any ")
            .or_else(|| type_name.strip_prefix("some "))
            .unwrap_or(type_name)
            .trim();

        // Take just the base name before any generic brackets
        let base_type = type_name
            .find('<')
            .map(|i| &type_name[..i])
            .unwrap_or(type_name)
            .trim();

        // Swift type names start with uppercase
        if base_type.is_empty() || !base_type.starts_with(|c: char| c.is_uppercase()) {
            return None;
        }

        return Some((name.to_string(), base_type.to_string()));
    }

    // --- Path B: inferred type from initializer `let name = TypeName(...)` ---
    // e.g. `let driftSynthPool = DriftSynthPool()`
    if let Some(after_eq) = rest_after_name.strip_prefix('=') {
        let after_eq = after_eq.trim_start();

        // Extract the first token (potential type name or expression)
        let token_end = after_eq
            .find(|c: char| c == '(' || c == '.' || c == ' ' || c == '\t' || c == '{')
            .unwrap_or(after_eq.len());
        let token = &after_eq[..token_end];

        // Must start with uppercase to be a type constructor (not a variable/literal)
        if token.is_empty() || !token.starts_with(|c: char| c.is_uppercase()) {
            return None;
        }

        return Some((name.to_string(), token.to_string()));
    }

    None
}

/// Build a flat map `"EnclosingTypeSimpleName.propertyName" → "TypeSimpleName"`
/// by scanning each class/struct symbol body for stored property declarations.
fn build_property_type_map(symbols: &[ParsedSymbol]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for sym in symbols {
        if !matches!(sym.kind, SymbolKind::Class) {
            continue;
        }
        let type_simple = sym.qname.rsplit('.').next().unwrap_or(&sym.qname);
        for line in sym.body.lines() {
            if let Some((prop_name, type_name)) = parse_property_line(line) {
                map.insert(format!("{}.{}", type_simple, prop_name), type_name);
            }
        }
    }
    map
}

// -----------------------------------------------------------------------------
// Call-edge extraction
// -----------------------------------------------------------------------------

/// Derive the enclosing type qname from a method qname.
///
/// For a method declared inside the same file (non-extension), the parent
/// qname is present in the file-local `known` set.  For extension methods the
/// class is defined in a *different* file, so `known` won't contain it.
///
/// Two fallback tiers:
///
/// 1. **Workspace exact**: check `workspace.qnames` for the direct parent.
///    Handles cases where the class file used the same structure.
///
/// 2. **Suffix search**: extension files embed the source file name in the
///    scope, producing qnames like:
///      `"ExampleFlow.ExampleFlowViewModel+DriftPad.ExampleFlowViewModel.method"`
///    The canonical class qname in the workspace is:
///      `"ExampleFlow.ExampleFlowViewModel.ExampleFlowViewModel"`
///    We recover it by searching for the class simple name (last component of
///    the parent) via the suffix index.
///
/// Returns `None` only when no match is found (free functions, truly unknown
/// contexts).
fn enclosing_type_qname(
    qname: &str,
    known: &HashSet<&str>,
    workspace: &WorkspaceSymbols,
) -> Option<String> {
    let idx = qname.rfind('.')?;
    let parent = &qname[..idx];

    // Tier 1: file-local set or direct workspace lookup.
    if known.contains(parent) || workspace.contains(parent) {
        return Some(parent.to_string());
    }

    // Tier 2: extension files embed the file name in the scope, making the
    // parent not directly present.  Recover the canonical class qname from
    // the workspace suffix index using just the class simple name.
    let class_name = parent.rsplit('.').next()?;
    // Guard against single-component parents (module-level free functions)
    // where `class_name` would equal the entire parent — avoid false matches.
    if !parent.contains('.') {
        return None;
    }
    workspace.find_by_suffix(class_name).map(|s| s.to_string())
}

/// Try to find a callee qname for `type_name.method` in the workspace.
///
/// Two-pass strategy to handle suffix ambiguity in SPM packages where the
/// module directory and class share the same name (e.g. `Scheduler/Scheduler.swift`
/// → qname `Engine.Scheduler.Scheduler.laneLoopPositions`):
///
/// 1. `"Scheduler.laneLoopPositions"` — simple 2-component suffix.
///    May be ambiguous if a test mock or protocol also defines the method.
///
/// 2. `"Scheduler.Scheduler.laneLoopPositions"` — the same-name-as-module
///    3-component suffix.  Much less likely to collide with test fixtures
///    and uniquely identifies the concrete class in the package.
fn resolve_instance_method(
    type_name: &str,
    method: &str,
    workspace: &WorkspaceSymbols,
) -> Option<String> {
    workspace
        .find_by_suffix(&format!("{type_name}.{method}"))
        .map(|s| s.to_string())
        .or_else(|| {
            workspace
                .find_by_suffix(&format!("{type_name}.{type_name}.{method}"))
                .map(|s| s.to_string())
        })
}

fn collect_calls(
    node: Node<'_>,
    src: &[u8],
    sym: &ParsedSymbol,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    prop_map: &HashMap<String, String>,
    edges: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call_expression" {
        if let Some(func_node) = node.child(0) {
            let (receiver, method) = if func_node.kind() == "navigation_expression"
                || func_node.kind() == "member_expression"
            {
                let recv = func_node.child(0).map(|n| node_text(n, src)).unwrap_or("");
                // In tree-sitter-swift the member name lives inside a
                // `navigation_suffix` child as a `simple_identifier`, not in
                // a field named "name".  Try that first, then fall back to a
                // "name" field for any grammar that does use it.
                let method = find_child_by_kind(func_node, "navigation_suffix")
                    .and_then(|suffix| find_child_by_kind(suffix, "simple_identifier"))
                    .map(|n| node_text(n, src))
                    .or_else(|| child_by_field(func_node, "name").map(|n| node_text(n, src)))
                    .unwrap_or("");
                (recv, method)
            } else {
                ("", node_text(func_node, src))
            };

            if !method.is_empty() {
                let callee = if receiver.is_empty() {
                    if let Some(et) = enclosing_type {
                        let q = format!("{}.{}", et, method);
                        if known.contains(q.as_str()) {
                            Some(q)
                        } else {
                            by_simple.get(method).cloned().or_else(|| {
                                workspace
                                    .find_by_suffix(&format!("{}.{}", et, method))
                                    .map(|s| s.to_string())
                            })
                        }
                    } else {
                        by_simple.get(method).cloned()
                    }
                } else {
                    // Receiver may be a chained expression like `self.pool` — extract
                    // just the last component as the simple receiver name.
                    let simple_recv = receiver.rsplit('.').next().unwrap_or(receiver);
                    let q = format!("{}.{}", simple_recv, method);
                    if known.contains(q.as_str()) || workspace.contains(&q) {
                        Some(q)
                    } else if let Some(s) = workspace.find_by_suffix(&q) {
                        // Suffix fallback: handles file-path qname prefixes.
                        Some(s.to_string())
                    } else if simple_recv == "Self" || simple_recv == "self" {
                        // `Self.method()` — treat exactly like a bare call from the
                        // enclosing type.  No qname in the workspace ends in
                        // `Self.<something>`, so suffix lookup above always fails.
                        if let Some(et) = enclosing_type {
                            let q2 = format!("{}.{}", et, method);
                            if known.contains(q2.as_str()) {
                                Some(q2)
                            } else {
                                workspace
                                    .find_by_suffix(&format!("{}.{}", et, method))
                                    .map(|s| s.to_string())
                            }
                        } else {
                            None
                        }
                    } else if simple_recv.starts_with(|c: char| c.is_uppercase()) {
                        // Type-qualified static call: `DriftCompiler.filterByMuteSolo()`
                        // The workspace.find_by_suffix above already ran and missed
                        // (ambiguous or not found).  Secondary attempt: if the receiver
                        // matches the enclosing type's simple name, treat it as a
                        // same-type call and resolve via the full enclosing-type qname.
                        let et_simple = sym.qname.split('.').rev().nth(1).unwrap_or("");
                        if !et_simple.is_empty() && simple_recv == et_simple {
                            if let Some(et) = enclosing_type {
                                workspace
                                    .find_by_suffix(&format!("{}.{}", et, method))
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        } else {
                            // Cross-type static call: already tried find_by_suffix(q)
                            // above; nothing more to try without a type registry.
                            None
                        }
                    } else if simple_recv.starts_with(|c: char| c.is_lowercase()) {
                        // Instance property call: `pool.resolve()` where `pool` is a
                        // stored property.  Look up its declared type in the property
                        // map and retry suffix lookup on `ActualType.method`.
                        //
                        // Derive the simple enclosing type from `sym.qname` directly
                        // (second-to-last component) so this works for extension methods
                        // where the class isn't in the file-local `known` set.
                        let et_simple = sym.qname.split('.').rev().nth(1).unwrap_or("");
                        if !et_simple.is_empty() {
                            let prop_key = format!("{}.{}", et_simple, simple_recv);

                            // Primary path: explicit type from property map.
                            // `resolve_instance_method` also tries the doubled-suffix
                            // ("Scheduler.Scheduler.method") to handle SPM packages
                            // where the module dir and class share a name and a single
                            // suffix lookup would be ambiguous.
                            let via_prop_map = prop_map.get(&prop_key).and_then(|actual_type| {
                                resolve_instance_method(actual_type, method, workspace)
                            });

                            if via_prop_map.is_some() {
                                via_prop_map
                            } else {
                                // Naming-convention fallback: Swift convention is that
                                // a property `scheduler` typically has type `Scheduler`.
                                // Activates when the prop_map has no entry (e.g. the
                                // declaration used `any`/`some` with a protocol that
                                // wasn't extracted, or the property was injected via
                                // init without a stored-property declaration).
                                let capitalized = {
                                    let mut s = simple_recv.to_string();
                                    if let Some(first) = s.get_mut(0..1) {
                                        first.make_ascii_uppercase();
                                    }
                                    s
                                };
                                resolve_instance_method(&capitalized, method, workspace)
                            }
                        } else {
                            None
                        }
                    } else {
                        None
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
        collect_calls(
            node.child(i).unwrap(),
            src,
            sym,
            by_simple,
            known,
            workspace,
            enclosing_type,
            prop_map,
            edges,
        );
    }
}

fn extract_call_edges_impl(
    file: &str,
    _source: &str,
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

    // Use the workspace-wide property map for resolving instance property calls
    // like `pool.resolve()` where `pool: DriftSynthPool` is declared in another
    // file.  `workspace.properties` is populated by the index pipeline from ALL
    // files via `extract_property_types` before call-edge extraction begins.
    // Fall back to a file-local map if the workspace map is empty (e.g., unit tests).
    let local_prop_map;
    let prop_map: &HashMap<String, String> = if !workspace.properties.is_empty() {
        &workspace.properties
    } else {
        local_prop_map = build_property_type_map(symbols);
        &local_prop_map
    };

    let mut parser = Parser::new();
    if parser.set_language(&SWIFT.into()).is_err() {
        return Vec::new();
    }

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
        let enclosing_type = enclosing_type_qname(&sym.qname, &known, workspace);
        collect_calls(
            tree.root_node(),
            src_bytes,
            sym,
            &by_simple,
            &known,
            workspace,
            enclosing_type.as_deref(),
            &prop_map,
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

    fn adapter() -> SwiftAdapter {
        SwiftAdapter::new()
    }

    #[test]
    fn parses_class_struct_enum_protocol_method() {
        let src = r#"
class PaymentService {
    init() {}

    func charge(customerId: String, amount: Decimal) -> Receipt {
        return Receipt()
    }
}

struct Receipt {
    let id: String
}

protocol Gateway {
    func process(_ payment: Payment) throws
}

enum Currency {
    case usd, eur, gbp
}
"#;
        let syms = adapter()
            .parse_symbols("Sources/Payments/PaymentService.swift", src)
            .unwrap();
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(
            qnames.iter().any(|q| q.ends_with("PaymentService")),
            "{qnames:?}"
        );
        assert!(
            qnames.iter().any(|q| q.ends_with("PaymentService.charge")),
            "{qnames:?}"
        );
        assert!(qnames.iter().any(|q| q.ends_with("Receipt")), "{qnames:?}");
        assert!(qnames.iter().any(|q| q.ends_with("Gateway")), "{qnames:?}");
        assert!(qnames.iter().any(|q| q.ends_with("Currency")), "{qnames:?}");
    }

    #[test]
    fn file_prefix_strips_swift_extension() {
        // SPM: strip Sources/ segment → stable target-name anchor
        assert_eq!(
            file_qname_prefix("Sources/Payments/ChargeService.swift"),
            "Payments.ChargeService"
        );
        // Deep SPM path: same result regardless of index root depth
        assert_eq!(
            file_qname_prefix("Packages/SequencerCore/Sources/Engine/DriftCompiler.swift"),
            "Engine.DriftCompiler"
        );
        // Xcode app target path
        assert_eq!(
            file_qname_prefix("App/ExampleFlow/Sources/ExampleFlow/ExampleFlowViewModel.swift"),
            "ExampleFlow.ExampleFlowViewModel"
        );
        // Flat file with no Sources segment — unchanged
        assert_eq!(
            file_qname_prefix("Engine/DriftCompiler.swift"),
            "Engine.DriftCompiler"
        );
        // Legacy ./prefix stripped, no Sources segment
        assert_eq!(
            file_qname_prefix("./App/Models/User.swift"),
            "App.Models.User"
        );
        // Top-level file
        assert_eq!(file_qname_prefix("main.swift"), "main");
    }

    #[test]
    fn file_prefix_dedupes_consecutive_repeated_segments() {
        // Refinement (1.0.74): Xcode-style nested project naming
        // produces paths like
        //   App/ExampleFlow/ExampleFlow/ExampleFlow/Views/DriftPad/DriftPadView.swift
        // (no SPM `Sources/` marker — strip_sources_prefix is a
        // no-op). Pre-fix this yielded
        //   App.ExampleFlow.ExampleFlow.ExampleFlow.Views.DriftPad.DriftPadView
        // — the triple ExampleFlow inflates tokens, distorts
        // BM25, and pollutes search output. Field-test surfaced
        // in ExampleProj 1.0.72.
        assert_eq!(
            file_qname_prefix(
                "App/ExampleFlow/ExampleFlow/ExampleFlow/Views/DriftPad/DriftPadView.swift"
            ),
            "App.ExampleFlow.Views.DriftPad.DriftPadView",
            "consecutive `ExampleFlow` segments should collapse to one"
        );
        // Double, not triple
        assert_eq!(file_qname_prefix("Foo/Bar/Bar/baz.swift"), "Foo.Bar.baz");
        // Non-consecutive duplicates left alone (could be a real
        // semantic structure: `App.Auth.User.Auth.helper`)
        assert_eq!(
            file_qname_prefix("Foo/Bar/Baz/Bar/qux.swift"),
            "Foo.Bar.Baz.Bar.qux"
        );
        // Doesn't collide with the SPM Sources-strip path
        assert_eq!(
            file_qname_prefix("App/ExampleFlow/Sources/ExampleFlow/ExampleFlow/foo.swift"),
            "ExampleFlow.foo",
            "after Sources-strip, the ExampleFlow/ExampleFlow/ also dedupes"
        );
    }

    #[test]
    fn dedupe_consecutive_segments_unit() {
        assert_eq!(dedupe_consecutive_segments("a.a.a.b.c"), "a.b.c");
        assert_eq!(dedupe_consecutive_segments("a.b.c"), "a.b.c");
        assert_eq!(dedupe_consecutive_segments(""), "");
        assert_eq!(dedupe_consecutive_segments("a"), "a");
        assert_eq!(dedupe_consecutive_segments("a.b.a"), "a.b.a"); // non-consecutive
        assert_eq!(
            dedupe_consecutive_segments("App.ExampleFlow.ExampleFlow.ExampleFlow.Views"),
            "App.ExampleFlow.Views"
        );
    }

    #[test]
    fn strip_sources_prefix_variants() {
        // Standard SPM Sources/
        assert_eq!(
            strip_sources_prefix("Packages/Core/Sources/Engine/Foo"),
            "Engine/Foo"
        );
        // Source (singular)
        assert_eq!(
            strip_sources_prefix("MyApp/Source/Models/Bar"),
            "Models/Bar"
        );
        // src (lowercase)
        assert_eq!(
            strip_sources_prefix("web/src/utils/helpers"),
            "utils/helpers"
        );
        // Already at Sources (no prefix to strip)
        assert_eq!(strip_sources_prefix("Sources/Engine/Foo"), "Engine/Foo");
        // No Sources segment → unchanged
        assert_eq!(
            strip_sources_prefix("Engine/DriftCompiler"),
            "Engine/DriftCompiler"
        );
        // Sources at the end with no tail → unchanged (edge case)
        assert_eq!(strip_sources_prefix("Pkg/Sources"), "Pkg/Sources");
    }

    #[test]
    fn static_method_calls_within_same_type() {
        let src = r#"
struct DriftCompiler {
    static func compile(clips: [String]) -> [String] {
        let filtered = filterByMuteSolo(clips: clips)
        let expanded = Self.expandClip(filtered[0])
        return DriftCompiler.postProcess(expanded)
    }
    static func filterByMuteSolo(clips: [String]) -> [String] { clips }
    static func expandClip(_ c: String) -> String { c }
    static func postProcess(_ c: String) -> [String] { [c] }
}
"#;
        let adapter = adapter();
        // Symbols parsed with a Sources/-anchored path
        let syms = adapter
            .parse_symbols("Sources/Engine/DriftCompiler.swift", src)
            .unwrap();
        // All four methods should be parsed
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(
            qnames.iter().any(|q| q.ends_with("DriftCompiler.compile")),
            "{qnames:?}"
        );
        assert!(
            qnames
                .iter()
                .any(|q| q.ends_with("DriftCompiler.filterByMuteSolo")),
            "{qnames:?}"
        );
        assert!(
            qnames
                .iter()
                .any(|q| q.ends_with("DriftCompiler.expandClip")),
            "{qnames:?}"
        );
        assert!(
            qnames
                .iter()
                .any(|q| q.ends_with("DriftCompiler.postProcess")),
            "{qnames:?}"
        );

        // Build workspace seeded from these symbols (simulates what index_pipeline does)
        let mut ws = WorkspaceSymbols::default();
        for s in &syms {
            ws.qnames.insert(s.qname.clone());
            ws.kinds.insert(s.qname.clone(), s.kind);
        }
        ws.build_suffix_index();

        let edges =
            adapter.extract_call_edges("Sources/Engine/DriftCompiler.swift", src, &syms, &ws);

        let caller: Vec<&str> = edges
            .iter()
            .filter(|e| e.caller_qname.ends_with("DriftCompiler.compile"))
            .map(|e| e.callee_qname.as_str())
            .collect();

        // Bare call: filterByMuteSolo(...)
        assert!(
            caller
                .iter()
                .any(|q| q.ends_with("DriftCompiler.filterByMuteSolo")),
            "missing bare intra-type call; edges from compile: {caller:?}"
        );
        // Self.expandClip(...)
        assert!(
            caller
                .iter()
                .any(|q| q.ends_with("DriftCompiler.expandClip")),
            "missing Self.method() call; edges from compile: {caller:?}"
        );
        // DriftCompiler.postProcess(...)
        assert!(
            caller
                .iter()
                .any(|q| q.ends_with("DriftCompiler.postProcess")),
            "missing TypeName.method() call; edges from compile: {caller:?}"
        );
    }

    #[test]
    fn self_dot_call_resolves_to_enclosing_type() {
        let src = r#"
class Scheduler {
    func start() {
        Self.validate()
        self.reset()
    }
    func validate() {}
    func reset() {}
}
"#;
        let adapter = adapter();
        let syms = adapter
            .parse_symbols("Sources/Core/Scheduler.swift", src)
            .unwrap();
        let mut ws = WorkspaceSymbols::default();
        for s in &syms {
            ws.qnames.insert(s.qname.clone());
            ws.kinds.insert(s.qname.clone(), s.kind);
        }
        ws.build_suffix_index();
        let edges = adapter.extract_call_edges("Sources/Core/Scheduler.swift", src, &syms, &ws);
        let from_start: Vec<&str> = edges
            .iter()
            .filter(|e| e.caller_qname.ends_with("Scheduler.start"))
            .map(|e| e.callee_qname.as_str())
            .collect();
        assert!(
            from_start.iter().any(|q| q.ends_with("Scheduler.validate")),
            "Self.validate() not resolved; edges: {from_start:?}"
        );
        assert!(
            from_start.iter().any(|q| q.ends_with("Scheduler.reset")),
            "self.reset() not resolved; edges: {from_start:?}"
        );
    }

    #[test]
    fn infers_fs_read_and_net_out() {
        let src = r#"
class Fetcher {
    func fetch() async throws -> String {
        let data = try Data(contentsOf: URL(fileURLWithPath: "/tmp/config"))
        let (respData, _) = try await URLSession.shared.data(from: URL(string: "https://api.example.com/v1")!)
        return String(data: data, encoding: .utf8)!
    }
}
"#;
        let syms = adapter().parse_symbols("Fetcher.swift", src).unwrap();
        let fetch = syms.iter().find(|s| s.qname.ends_with(".fetch")).unwrap();
        let effs = adapter().infer_effects("", fetch);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::IoFsRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::IoNetOut), "{cats:?}");
    }

    #[test]
    fn infers_log_and_env() {
        let src = r#"
class Config {
    func load() -> String {
        let env = ProcessInfo.processInfo.environment["APP_ENV"] ?? "dev"
        print("Loading config for \(env)")
        return env
    }
}
"#;
        let syms = adapter().parse_symbols("Config.swift", src).unwrap();
        let load = syms.iter().find(|s| s.qname.ends_with(".load")).unwrap();
        let effs = adapter().infer_effects("", load);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::EnvRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::Log), "{cats:?}");
    }

    #[test]
    fn empty_when_no_patterns() {
        let src = r#"
struct Math {
    func add(_ a: Int, _ b: Int) -> Int { a + b }
}
"#;
        let syms = adapter().parse_symbols("Math.swift", src).unwrap();
        let add = syms.iter().find(|s| s.qname.ends_with(".add")).unwrap();
        let effs = adapter().infer_effects("", add);
        assert!(effs.is_empty(), "{effs:?}");
    }

    #[test]
    fn extracts_intra_class_call_edges() {
        let src = r#"
class OrderService {
    func placeOrder(_ order: Order) {
        charge(customerId: order.customerId, amount: order.total)
    }

    func charge(customerId: String, amount: Decimal) {
        // process
    }
}
"#;
        let ws = WorkspaceSymbols::default();
        let syms = adapter().parse_symbols("OrderService.swift", src).unwrap();
        let edges = adapter().extract_call_edges("OrderService.swift", src, &syms, &ws);
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".placeOrder") && e.callee_qname.ends_with(".charge")
        });
        assert!(found, "expected intra-class edge; got: {edges:?}");
    }

    #[test]
    fn resolves_instance_property_call_via_prop_map() {
        // `pool` is a stored property of type `DriftSynthPool`.
        // `schedule()` calls `pool.resolve()` — the call graph must resolve
        // the receiver to `DriftSynthPool` and emit the cross-file edge.
        let src = r#"
class SynthScheduler {
    let pool: DriftSynthPool

    func schedule() {
        pool.resolve()
    }
}

class DriftSynthPool {
    func resolve() {}
}
"#;
        let mut ws = WorkspaceSymbols::default();
        ws.qnames
            .insert("Sources.Models.DriftSynthPool.resolve".to_string());
        ws.build_suffix_index();

        let syms = adapter()
            .parse_symbols("Sources/Models/SynthScheduler.swift", src)
            .unwrap();
        let edges =
            adapter().extract_call_edges("Sources/Models/SynthScheduler.swift", src, &syms, &ws);

        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".schedule")
                && e.callee_qname.ends_with("DriftSynthPool.resolve")
        });
        assert!(found, "expected property-map edge; got: {edges:?}");
    }

    #[test]
    fn parse_property_line_handles_variants() {
        assert_eq!(
            parse_property_line("    let pool: DriftSynthPool"),
            Some(("pool".into(), "DriftSynthPool".into()))
        );
        assert_eq!(
            parse_property_line("    var compiler: DriftCompiler?"),
            Some(("compiler".into(), "DriftCompiler".into()))
        );
        // Array/dictionary types start with `[`, not uppercase → not tracked
        assert_eq!(
            parse_property_line("    @Published var items: [Item]"),
            None
        );
        assert_eq!(
            parse_property_line("    private weak var delegate: SomeDelegate?"),
            Some(("delegate".into(), "SomeDelegate".into()))
        );
        // Initializer-inferred type
        assert_eq!(
            parse_property_line("    let driftSynthPool = DriftSynthPool()"),
            Some(("driftSynthPool".into(), "DriftSynthPool".into()))
        );
        assert_eq!(
            parse_property_line("    let scheduler = BatchScheduler.make()"),
            Some(("scheduler".into(), "BatchScheduler".into()))
        );
        // Computed property — has `{`, type ends before it → type_name should
        // still be extracted since the `{` cuts the type string.
        // But if the type string becomes empty we return None — this is fine.
        assert_eq!(parse_property_line("    func doSomething() {}"), None);
        assert_eq!(parse_property_line("    // let x: SomeType"), None);
        // `any`/`some` existential/opaque markers (Swift 5.7+)
        assert_eq!(
            parse_property_line("    var scheduler: any SchedulerProtocol"),
            Some(("scheduler".into(), "SchedulerProtocol".into()))
        );
        assert_eq!(
            parse_property_line("    var engine: some AudioEngineProtocol?"),
            Some(("engine".into(), "AudioEngineProtocol".into()))
        );
    }

    /// When `find_by_suffix("Scheduler.laneLoopPositions")` is ambiguous (e.g. a
    /// test mock also defines the method), the resolver must fall back to the
    /// longer `"Scheduler.Scheduler.laneLoopPositions"` suffix that uniquely
    /// identifies the concrete SPM class.
    #[test]
    fn resolve_instance_method_uses_doubled_suffix_on_ambiguity() {
        let mut ws = WorkspaceSymbols::default();
        // Real class (SPM package: Engine/Scheduler/Scheduler.swift)
        ws.qnames
            .insert("Engine.Scheduler.Scheduler.laneLoopPositions".to_string());
        // Test mock (creates ambiguity on the simple 2-component suffix)
        ws.qnames
            .insert("Tests.MockScheduler.laneLoopPositions".to_string());
        ws.build_suffix_index();

        // Simple suffix is ambiguous → resolve_instance_method should pick the
        // doubled-suffix variant instead.
        let result = resolve_instance_method("Scheduler", "laneLoopPositions", &ws);
        assert_eq!(
            result.as_deref(),
            Some("Engine.Scheduler.Scheduler.laneLoopPositions"),
            "should resolve via doubled suffix when simple suffix is ambiguous"
        );
    }

    /// Naming-convention fallback: when no explicit type annotation exists in
    /// the prop_map for the receiver, try capitalising the receiver name.
    /// `scheduler.restartLane(...)` → look for `Scheduler.restartLane`.
    #[test]
    fn naming_convention_fallback_resolves_property_call() {
        // Class with NO explicit type on the scheduler property (injected via init;
        // parse_property_line can't extract the type from the assignment).
        let src = r#"
class SessionViewModel {
    let scheduler: Scheduler

    func handlePad(laneID: Int, tick: Int) {
        scheduler.restartLane(laneID, at: tick)
    }
}
"#;
        let mut ws = WorkspaceSymbols::default();
        // Callee is in a different package with the same-name-as-module pattern.
        ws.qnames
            .insert("Engine.Scheduler.Scheduler.restartLane".to_string());
        // Inject ambiguity on the simple suffix to force the doubled-suffix path.
        ws.qnames
            .insert("Tests.MockScheduler.restartLane".to_string());
        ws.build_suffix_index();

        let syms = adapter()
            .parse_symbols("Sources/Session/SessionViewModel.swift", src)
            .unwrap();
        let edges =
            adapter().extract_call_edges("Sources/Session/SessionViewModel.swift", src, &syms, &ws);
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with("handlePad")
                && e.callee_qname == "Engine.Scheduler.Scheduler.restartLane"
        });
        assert!(
            found,
            "expected doubled-suffix resolution of scheduler.restartLane; got: {edges:?}"
        );
    }

    /// Regression: extension methods calling stored properties via labeled arguments
    /// must produce call edges even when the class declaration is in a different file.
    ///
    /// Real-world pattern: `ExampleFlowViewModel+DriftPad.swift` calls
    /// `scheduler.restartLane(laneID, at: tick)` and
    /// `scheduler.laneLoopPositions(currentTick: tick)`, but `scheduler: Scheduler`
    /// is declared in `ExampleFlowViewModel.swift`.
    #[test]
    fn extension_file_property_call_cross_file() {
        // ---- "class file" symbols (ExampleFlowViewModel.swift) ----
        let class_src = r#"
class ExampleFlowViewModel {
    let scheduler: Scheduler

    func mainMethod() {}
}
"#;
        let class_syms = adapter()
            .parse_symbols(
                "App/ExampleFlow/Sources/ExampleFlow/ExampleFlowViewModel.swift",
                class_src,
            )
            .unwrap();

        // ---- "extension file" symbols (ExampleFlowViewModel+DriftPad.swift) ----
        let ext_src = r#"
extension ExampleFlowViewModel {
    func driftPadMethod(laneID: Int, tick: Int) {
        let pos = scheduler.laneLoopPositions(currentTick: tick)
        scheduler.restartLane(laneID, at: tick)
        self.scheduler.laneLoopPositions(currentTick: tick)
    }
}
"#;
        let ext_syms = adapter()
            .parse_symbols(
                "App/ExampleFlow/Sources/ExampleFlow/ExampleFlowViewModel+DriftPad.swift",
                ext_src,
            )
            .unwrap();

        // ---- workspace: both files contribute qnames ----
        let mut ws = WorkspaceSymbols::default();
        // Callee lives in the Engine package
        ws.qnames
            .insert("Engine.Scheduler.Scheduler.laneLoopPositions".to_string());
        ws.qnames
            .insert("Engine.Scheduler.Scheduler.restartLane".to_string());
        // All parsed symbols from both files
        for s in class_syms.iter().chain(ext_syms.iter()) {
            ws.qnames.insert(s.qname.clone());
            ws.kinds.insert(s.qname.clone(), s.kind);
        }
        ws.build_suffix_index();

        // ---- populate workspace.properties from the class file ----
        ws.properties
            .extend(adapter().extract_property_types(&class_syms));

        // ---- extract edges from the extension file ----
        let edges = adapter().extract_call_edges(
            "App/ExampleFlow/Sources/ExampleFlow/ExampleFlowViewModel+DriftPad.swift",
            ext_src,
            &ext_syms,
            &ws,
        );
        let callees: Vec<&str> = edges
            .iter()
            .filter(|e| e.caller_qname.ends_with("driftPadMethod"))
            .map(|e| e.callee_qname.as_str())
            .collect();

        assert!(
            callees
                .iter()
                .any(|q| q.ends_with("Scheduler.laneLoopPositions")),
            "laneLoopPositions not resolved; callees from driftPadMethod: {callees:?}\n\
             All edges: {edges:?}\n\
             workspace.properties: {:?}",
            ws.properties
        );
        assert!(
            callees.iter().any(|q| q.ends_with("Scheduler.restartLane")),
            "restartLane not resolved; callees from driftPadMethod: {callees:?}\n\
             All edges: {edges:?}"
        );
    }

    /// `enclosing_type` must be resolved from the workspace for extension methods
    /// (the class symbol is not in the file-local `known` set for extension files).
    #[test]
    fn self_method_call_in_extension_resolves_via_workspace() {
        let class_src = r#"
class AudioEngine {
    func start() {}
    func stop() {}
}
"#;
        let ext_src = r#"
extension AudioEngine {
    func restart() {
        self.stop()
        self.start()
    }
}
"#;
        let class_syms = adapter()
            .parse_symbols("Sources/Audio/AudioEngine.swift", class_src)
            .unwrap();
        let ext_syms = adapter()
            .parse_symbols("Sources/Audio/AudioEngine+Restart.swift", ext_src)
            .unwrap();

        let mut ws = WorkspaceSymbols::default();
        for s in class_syms.iter().chain(ext_syms.iter()) {
            ws.qnames.insert(s.qname.clone());
            ws.kinds.insert(s.qname.clone(), s.kind);
        }
        ws.build_suffix_index();

        let edges = adapter().extract_call_edges(
            "Sources/Audio/AudioEngine+Restart.swift",
            ext_src,
            &ext_syms,
            &ws,
        );
        let from_restart: Vec<&str> = edges
            .iter()
            .filter(|e| e.caller_qname.ends_with("restart"))
            .map(|e| e.callee_qname.as_str())
            .collect();

        assert!(
            from_restart.iter().any(|q| q.ends_with("AudioEngine.stop")),
            "self.stop() not resolved in extension; edges: {from_restart:?}"
        );
        assert!(
            from_restart
                .iter()
                .any(|q| q.ends_with("AudioEngine.start")),
            "self.start() not resolved in extension; edges: {from_restart:?}"
        );
    }
}

#[cfg(test)]
mod service_endpoint_tests {
    use super::*;
    use agentstatedeveloper_core::adapter::LanguageAdapter;

    fn detect(src: &str) -> Vec<DetectedEndpoint> {
        let a = SwiftAdapter;
        let s = a.parse_symbols("routes.swift", src).unwrap();
        a.infer_service_endpoints("routes.swift", src, &s)
    }
    fn inb(e: &[DetectedEndpoint]) -> Vec<&DetectedEndpoint> {
        e.iter()
            .filter(|e| e.direction == Direction::Inbound)
            .collect()
    }

    #[test]
    fn vapor_route_with_components() {
        let src = "func routes(_ app: Application) throws {\n    app.get(\"users\", \":id\") { req in\n        return user\n    }\n}\n";
        let eps = detect(src);
        assert_eq!(inb(&eps)[0].contract, "http:GET /users/{}", "{eps:?}");
    }

    #[test]
    fn vapor_post_with_use_handler() {
        let src = "func routes(_ app: Application) throws {\n    app.post(\"charge\", use: chargeHandler)\n}\n";
        let eps = detect(src);
        assert_eq!(inb(&eps)[0].contract, "http:POST /charge");
    }

    #[test]
    fn plain_get_without_handler_is_not_a_route() {
        // dict.get("key") has no `use:`/trailing-`{`, so it isn't a route.
        let src = "func f() {\n    let v = cache.get(\"key\")\n    _ = v\n}\n";
        assert!(inb(&detect(src)).is_empty(), "{:?}", detect(src));
    }

    #[test]
    fn swift_server_matches_other_language_client() {
        let server = detect(
            "func r(_ app: Application) throws {\n    app.get(\"users\", \":id\") { req in return u }\n}\n",
        );
        assert_eq!(
            inb(&server)[0].contract,
            http_contract("get", "https://svc/users/:id")
        );
        assert_eq!(inb(&server)[0].contract, "http:GET /users/{}");
    }
}
