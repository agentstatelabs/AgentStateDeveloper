//! Python language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-python`. Parses module-level functions, methods,
//! and classes, and runs a small substring-based effect inference pass.

use agentstatedeveloper_core::adapter::{LanguageAdapter, ParsedSymbol};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Node, Parser};

/// Python language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct PythonAdapter;

impl PythonAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for PythonAdapter {
    fn language(&self) -> &str {
        "python"
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|e| AsdError::Parse(format!("failed to set python language: {e}")))?;

        let src_bytes = source.as_bytes();
        let tree = parser
            .parse(src_bytes, None)
            .ok_or_else(|| AsdError::Parse(format!("failed to parse {file}")))?;

        let module_prefix = module_qname_prefix(file);
        let root = tree.root_node();
        let mut out = Vec::new();
        walk(root, src_bytes, &module_prefix, &[], &mut out);
        Ok(out)
    }

    fn infer_effects(&self, _source: &str, symbol: &ParsedSymbol) -> Vec<Effect> {
        infer_effects_from_body(&symbol.body)
    }
}

/// Derive the dotted module prefix for a file path.
///
/// `foo/bar.py` -> `foo.bar`
/// `./foo/bar.py` -> `foo.bar`
/// `bar.py` -> `bar`
fn module_qname_prefix(file: &str) -> String {
    let mut s = file;
    if let Some(stripped) = s.strip_prefix("./") {
        s = stripped;
    }
    let s = s.strip_suffix(".py").unwrap_or(s);
    // Normalize both slash styles to dots.
    s.replace('\\', "/").replace('/', ".")
}

/// Recursive descent over the Python tree. We enumerate:
/// - `function_definition` -> Function or Method depending on scope
/// - `class_definition` -> Class
///
/// Nested functions get their enclosing function/class names in their qname.
fn walk(
    node: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    scope: &[(String, ScopeKind)],
    out: &mut Vec<ParsedSymbol>,
) {
    let kind = node.kind();
    match kind {
        "function_definition" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            let symbol_kind = if scope.last().map(|s| s.1) == Some(ScopeKind::Class) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let signature = extract_function_signature(node, src, &name);
            out.push(make_parsed_symbol(node, src, qname, symbol_kind, signature));

            // Recurse into the body with this function pushed on the scope.
            let mut new_scope = scope.to_vec();
            new_scope.push((name, ScopeKind::Function));
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk(child, src, module_prefix, &new_scope, out);
                }
            }
        }
        "class_definition" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            let signature = extract_class_signature(node, src, &name);
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Class,
                signature,
            ));

            let mut new_scope = scope.to_vec();
            new_scope.push((name, ScopeKind::Class));
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk(child, src, module_prefix, &new_scope, out);
                }
            }
        }
        _ => {
            // Descend into other containers (module root, decorated_definition, etc.).
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk(child, src, module_prefix, scope, out);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Class,
    Function,
}

fn build_qname(module_prefix: &str, scope: &[(String, ScopeKind)], name: &str) -> String {
    let mut parts = Vec::with_capacity(scope.len() + 2);
    if !module_prefix.is_empty() {
        parts.push(module_prefix.to_string());
    }
    for (s, _) in scope {
        parts.push(s.clone());
    }
    parts.push(name.to_string());
    parts.join(".")
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
        // tree-sitter positions are 0-based; ASD wants 1-based.
        start_line: (start.row as u32) + 1,
        start_col: (start.column as u32) + 1,
        end_line: (end.row as u32) + 1,
        end_col: (end.column as u32) + 1,
        body,
        signature,
    }
}

fn extract_function_signature(node: Node<'_>, src: &[u8], name: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_text = node_text(params, src)?;
    Some(format!("def {name}{params_text}"))
}

fn extract_class_signature(node: Node<'_>, src: &[u8], name: &str) -> Option<String> {
    // `superclasses` is optional in the grammar.
    if let Some(sup) = node.child_by_field_name("superclasses") {
        if let Some(sup_text) = node_text(sup, src) {
            return Some(format!("class {name}{sup_text}"));
        }
    }
    Some(format!("class {name}"))
}

// -----------------------------------------------------------------------------
// Effect inference
// -----------------------------------------------------------------------------

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // open(...) -> IoFsRead (+ IoFsWrite when mode contains 'w' or 'a')
    for call_site in find_calls(body, "open(") {
        let args = &body[call_site.args_start..call_site.args_end];
        effects.push(Effect {
            effect: EffectCategory::IoFsRead,
            qualifiers: extract_open_path(args)
                .map(|p| json!({ "paths": [p] }))
                .unwrap_or(serde_json::Value::Null),
            note: Some(trim_note(&body[call_site.call_start..call_site.args_end + 1])),
        });
        if mentions_write_mode(args) {
            effects.push(Effect {
                effect: EffectCategory::IoFsWrite,
                qualifiers: extract_open_path(args)
                    .map(|p| json!({ "paths": [p] }))
                    .unwrap_or(serde_json::Value::Null),
                note: Some(trim_note(&body[call_site.call_start..call_site.args_end + 1])),
            });
        }
    }

    // print(...) / sys.stdout / sys.stderr writes -> Log
    if contains_call(body, "print(")
        || body.contains("sys.stdout.write")
        || body.contains("sys.stderr.write")
        || body.contains("sys.stdout")
        || body.contains("sys.stderr")
    {
        let note = first_matching_line(
            body,
            &[
                "print(",
                "sys.stdout.write",
                "sys.stderr.write",
                "sys.stdout",
                "sys.stderr",
            ],
        );
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note,
        });
    }

    // Network libraries -> IoNetOut
    let net_prefixes = ["requests.", "urllib.", "httpx.", "aiohttp."];
    let mut net_hosts: Vec<String> = Vec::new();
    let mut net_note: Option<String> = None;
    for prefix in net_prefixes {
        for call_site in find_calls(body, prefix) {
            let args = &body[call_site.args_start..call_site.args_end];
            if let Some(host) = extract_url_host(args) {
                if !net_hosts.contains(&host) {
                    net_hosts.push(host);
                }
            }
            if net_note.is_none() {
                net_note = Some(trim_note(&body[call_site.call_start..call_site.args_end + 1]));
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
        });
    }

    // subprocess.* / os.system / os.exec* -> ProcSpawn
    let proc_patterns = ["subprocess.", "os.system(", "os.exec"];
    if let Some(note) = first_match_note(body, &proc_patterns) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // os.environ / os.getenv -> EnvRead
    let env_patterns = ["os.environ", "os.getenv"];
    if let Some(note) = first_match_note(body, &env_patterns) {
        let mut vars: Vec<String> = Vec::new();
        for call_site in find_calls(body, "os.getenv(") {
            let args = &body[call_site.args_start..call_site.args_end];
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
        });
    }

    // time.sleep -> TimeSleep
    if let Some(note) = first_match_note(body, &["time.sleep"]) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // time.time / time.monotonic / datetime.now -> TimeRead
    let time_read_patterns = ["time.time", "time.monotonic", "datetime.now"];
    if let Some(note) = first_match_note(body, &time_read_patterns) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // random.* / secrets.* -> Random
    let random_patterns = ["random.", "secrets."];
    if let Some(note) = first_match_note(body, &random_patterns) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // `raise ` statement -> Throw
    if has_raise_statement(body) {
        let note = first_matching_line(body, &["raise "]);
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note,
        });
    }

    effects
}

/// Scan for a literal needle and return the lines we recognize as actual call
/// sites. We do not attempt to skip string literals — for M1 this is good
/// enough and matches the "plain substring" guidance.
#[derive(Debug, Clone, Copy)]
struct CallSite {
    call_start: usize,
    args_start: usize,
    args_end: usize,
}

/// Find all call sites where `needle` (which must end in `(`) appears in
/// `body`, returning the byte offsets of the opening identifier, the byte
/// after the `(`, and the matching `)`.
fn find_calls(body: &str, needle: &str) -> Vec<CallSite> {
    let mut out = Vec::new();
    if !needle.ends_with('(') {
        return out;
    }
    let bytes = body.as_bytes();
    let mut search_from = 0;
    while let Some(idx) = body[search_from..].find(needle) {
        let abs = search_from + idx;
        let args_start = abs + needle.len();
        if let Some(args_end) = find_matching_paren(bytes, args_start) {
            out.push(CallSite {
                call_start: abs,
                args_start,
                args_end,
            });
            search_from = args_end + 1;
        } else {
            search_from = args_start;
        }
    }
    out
}

fn contains_call(body: &str, needle: &str) -> bool {
    !find_calls(body, needle).is_empty()
}

/// Given the position just after `(`, find the matching `)`. Handles nested
/// parens and single/double quoted strings (non-raw). Returns the index of the
/// matching `)` byte.
fn find_matching_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: i32 = 1;
    let mut i = start;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_string {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_string = None;
            }
        } else {
            match c {
                b'"' | b'\'' => in_string = Some(c),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn mentions_write_mode(args: &str) -> bool {
    // Look for a mode string literal containing 'w' or 'a'. Very loose —
    // we just scan quoted sections.
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            let q = c;
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != q {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                j += 1;
            }
            if j <= bytes.len() {
                let lit = &args[start..j.min(bytes.len())];
                if lit.len() <= 4
                    && (lit.contains('w') || lit.contains('a') || lit.contains('x'))
                {
                    return true;
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// Extract the first positional argument if it is a plain string literal.
fn extract_open_path(args: &str) -> Option<String> {
    extract_first_string_literal(args)
}

/// If the args start with a string literal, return its contents.
fn extract_first_string_literal(args: &str) -> Option<String> {
    let trimmed = args.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let q = bytes[0];
    if q != b'"' && q != b'\'' {
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

/// Try to extract a host from args containing a URL literal.
fn extract_url_host(args: &str) -> Option<String> {
    // Find any `http://` or `https://` in the args text.
    let lower = args;
    for scheme in ["https://", "http://"] {
        if let Some(idx) = lower.find(scheme) {
            let tail = &lower[idx + scheme.len()..];
            let end = tail
                .find(|c: char| {
                    c == '/' || c == '"' || c == '\'' || c == ')' || c == '?' || c.is_whitespace()
                })
                .unwrap_or(tail.len());
            let host = &tail[..end];
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
}

/// Return a short, single-line note snippet.
fn trim_note(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or("").trim();
    if first_line.len() > 120 {
        format!("{}...", &first_line[..120])
    } else {
        first_line.to_string()
    }
}

/// Return the first line of `body` that contains any of `needles`.
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

/// Check each needle; return the first matching line as a note.
fn first_match_note(body: &str, needles: &[&str]) -> Option<String> {
    for n in needles {
        if body.contains(n) {
            return first_matching_line(body, needles);
        }
    }
    None
}

/// Detect a `raise` statement (not `raises` in a docstring or attribute).
/// We check for `raise ` preceded by start-of-line or whitespace.
fn has_raise_statement(body: &str) -> bool {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("raise ") || trimmed == "raise" {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_prefix_strips_py_and_leading_dot_slash() {
        assert_eq!(module_qname_prefix("foo/bar.py"), "foo.bar");
        assert_eq!(module_qname_prefix("./foo/bar.py"), "foo.bar");
        assert_eq!(module_qname_prefix("bar.py"), "bar");
    }

    #[test]
    fn parses_module_function_and_class() {
        let src = "def top():\n    return 1\n\nclass C:\n    def m(self):\n        return 2\n";
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("x.py", src).unwrap();
        let qnames: Vec<_> = syms.iter().map(|s| s.qname.clone()).collect();
        assert!(qnames.contains(&"x.top".to_string()));
        assert!(qnames.contains(&"x.C".to_string()));
        assert!(qnames.contains(&"x.C.m".to_string()));
        let top = syms.iter().find(|s| s.qname == "x.top").unwrap();
        assert_eq!(top.kind, SymbolKind::Function);
        let m = syms.iter().find(|s| s.qname == "x.C.m").unwrap();
        assert_eq!(m.kind, SymbolKind::Method);
    }

    #[test]
    fn infers_fs_read_and_write_from_open() {
        let body = "def f():\n    with open('/tmp/a.txt', 'w') as f:\n        f.write('hi')\n";
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect).collect();
        assert!(cats.contains(&EffectCategory::IoFsRead));
        assert!(cats.contains(&EffectCategory::IoFsWrite));
    }

    #[test]
    fn empty_when_no_patterns() {
        let body = "def f(x):\n    return x + 1\n";
        assert!(infer_effects_from_body(body).is_empty());
    }
}
