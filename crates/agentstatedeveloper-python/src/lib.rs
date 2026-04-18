//! Python language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-python`. Parses module-level functions, methods,
//! and classes, and runs a small substring-based effect inference pass.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{CallEdge, LanguageAdapter, ParsedSymbol};
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

    fn extract_call_edges(
        &self,
        file: &str,
        _source: &str,
        symbols: &[ParsedSymbol],
    ) -> Vec<CallEdge> {
        extract_call_edges_impl(file, symbols)
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

    // print(...) / sys.stdout / sys.stderr / logging.* / log.* / logger.* -> Log
    let log_needles = [
        "print(",
        "sys.stdout",
        "sys.stderr",
        "logging.debug",
        "logging.info",
        "logging.warning",
        "logging.warn",
        "logging.error",
        "logging.critical",
        "logging.exception",
        "log.debug",
        "log.info",
        "log.warning",
        "log.warn",
        "log.error",
        "log.critical",
        "log.exception",
        "logger.debug",
        "logger.info",
        "logger.warning",
        "logger.warn",
        "logger.error",
        "logger.critical",
        "logger.exception",
    ];
    if log_needles.iter().any(|n| body.contains(n)) {
        let note = first_matching_line(body, &log_needles);
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note,
        });
    }

    // Database cursor/conn/db.execute(...) / .fetchone() / .commit()  -> IoDb*
    // Heuristic: inspect the SQL-like first arg of `.execute(` to decide
    // read vs write. Common names: db, conn, cursor, cur, session, c.
    let db_receivers = [
        "db.execute(",
        "conn.execute(",
        "cursor.execute(",
        "cur.execute(",
        "session.execute(",
        "c.execute(",
        "self.db.execute(",
        "self.conn.execute(",
    ];
    let mut seen_db_read = false;
    let mut seen_db_write = false;
    for recv in db_receivers {
        for call_site in find_calls(body, recv) {
            let args = &body[call_site.args_start..call_site.args_end];
            let sql = args.trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == 'f' || c == 'r' || c == 'b');
            let upper: String = sql.chars().take(16).collect::<String>().to_uppercase();
            let is_write = upper.starts_with("INSERT")
                || upper.starts_with("UPDATE")
                || upper.starts_with("DELETE")
                || upper.starts_with("REPLACE")
                || upper.starts_with("CREATE")
                || upper.starts_with("DROP")
                || upper.starts_with("ALTER")
                || upper.starts_with("TRUNCATE");
            let is_read = upper.starts_with("SELECT") || upper.starts_with("WITH") || upper.starts_with("SHOW");
            let note = Some(trim_note(&body[call_site.call_start..call_site.args_end + 1]));
            if is_write && !seen_db_write {
                effects.push(Effect {
                    effect: EffectCategory::IoDbWrite,
                    qualifiers: serde_json::Value::Null,
                    note,
                });
                seen_db_write = true;
            } else if is_read && !seen_db_read {
                effects.push(Effect {
                    effect: EffectCategory::IoDbRead,
                    qualifiers: serde_json::Value::Null,
                    note,
                });
                seen_db_read = true;
            } else if !is_write && !is_read && !seen_db_read && !seen_db_write {
                // Unrecognized SQL — conservatively emit both.
                effects.push(Effect {
                    effect: EffectCategory::IoDbRead,
                    qualifiers: serde_json::Value::Null,
                    note: note.clone(),
                });
                effects.push(Effect {
                    effect: EffectCategory::IoDbWrite,
                    qualifiers: serde_json::Value::Null,
                    note,
                });
                seen_db_read = true;
                seen_db_write = true;
            }
        }
    }
    // .commit() implies a preceding write
    if !seen_db_write && (body.contains(".commit()") || body.contains("conn.commit")) {
        effects.push(Effect {
            effect: EffectCategory::IoDbWrite,
            qualifiers: serde_json::Value::Null,
            note: first_matching_line(body, &[".commit()", "conn.commit"]),
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

#[allow(dead_code)]
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

// -----------------------------------------------------------------------------
// Call-edge extraction (intra-module)
// -----------------------------------------------------------------------------

/// Build the (caller_qname -> callee_qname) edge set for one file. Resolution
/// is intra-module only and best-effort:
///
/// - `identifier()` -> resolve via the module's `simple_name -> qname` map.
/// - `Class.method()` -> resolve `<module>.Class.method`.
/// - `self.method()` inside `<module>.Class.<fn>` -> resolve to `<module>.Class.method`.
///
/// Anything we can't resolve (subscripts, lambdas, cross-module calls, dynamic
/// dispatch) is silently skipped.
fn extract_call_edges_impl(file: &str, symbols: &[ParsedSymbol]) -> Vec<CallEdge> {
    let module_prefix = module_qname_prefix(file);

    // All known qnames in this file.
    let known: HashSet<&str> = symbols.iter().map(|s| s.qname.as_str()).collect();

    // simple_name -> qname for single-identifier resolution. If two symbols
    // share a simple name (e.g. method/function collision), we keep the first
    // module-level one we see; this is best-effort by design.
    let mut by_simple: HashMap<String, String> = HashMap::new();
    for s in symbols {
        let simple = s.qname.rsplit('.').next().unwrap_or(&s.qname).to_string();
        // Prefer module-level functions/classes over nested entries: only fill
        // if the qname has exactly module_prefix + "." + simple.
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
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let mut edges: HashSet<CallEdge> = HashSet::new();

    for sym in symbols {
        // Only function and method bodies can contain calls in a meaningful
        // sense; skip class/module symbols (their bodies' calls are usually
        // attributed to a wrapping function anyway).
        if !matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }

        let src_bytes = sym.body.as_bytes();
        let tree = match parser.parse(src_bytes, None) {
            Some(t) => t,
            None => continue,
        };

        // The parsed body is a `function_definition` at (or near) the root —
        // walk the whole tree and pick up every `call` node.
        let enclosing_class = enclosing_class_qname(&sym.qname, &module_prefix, &known);
        collect_calls(
            tree.root_node(),
            src_bytes,
            sym,
            &module_prefix,
            &by_simple,
            &known,
            enclosing_class.as_deref(),
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

/// For a method qname like `mod.Class.m`, return `Some("mod.Class")` provided
/// `mod.Class` is a known symbol in this file. Otherwise `None`.
fn enclosing_class_qname(
    qname: &str,
    module_prefix: &str,
    known: &HashSet<&str>,
) -> Option<String> {
    let parts: Vec<&str> = qname.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    // Drop the function/method name; look for the deepest ancestor that is a
    // known symbol AND lives below `module_prefix`.
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
    enclosing_class: Option<&str>,
    out: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call" {
        if let Some(func) = node.child_by_field_name("function") {
            if let Some(callee) =
                resolve_callee(func, src, module_prefix, by_simple, known, enclosing_class)
            {
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
            enclosing_class,
            out,
        );
    }
}

/// Resolve the function-position node of a `call` to a fully-qualified name
/// inside this module, when possible.
fn resolve_callee(
    func: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    enclosing_class: Option<&str>,
) -> Option<String> {
    match func.kind() {
        "identifier" => {
            let name = node_text(func, src)?;
            by_simple.get(&name).cloned()
        }
        "attribute" => {
            // `<object>.<attribute>` — we resolve `self.X` and `Class.X`.
            let object = func.child_by_field_name("object")?;
            let attr = func.child_by_field_name("attribute")?;
            let attr_name = node_text(attr, src)?;

            match object.kind() {
                "identifier" => {
                    let obj_name = node_text(object, src)?;
                    if obj_name == "self" {
                        let class = enclosing_class?;
                        let candidate = format!("{class}.{attr_name}");
                        if known.contains(candidate.as_str()) {
                            return Some(candidate);
                        }
                        None
                    } else {
                        // Treat `Foo.bar` as `<module>.Foo.bar` if we know it.
                        let class_qname = if module_prefix.is_empty() {
                            obj_name.clone()
                        } else {
                            format!("{module_prefix}.{obj_name}")
                        };
                        if known.contains(class_qname.as_str()) {
                            let candidate = format!("{class_qname}.{attr_name}");
                            if known.contains(candidate.as_str()) {
                                return Some(candidate);
                            }
                        }
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
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

    #[test]
    fn extracts_intra_module_call_edges() {
        let src = r#"
def helper(x):
    return x

def caller():
    return helper(1)

class C:
    def __init__(self):
        self.x = helper(2)

    def m(self):
        return self.__init__()
"#;
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("m.py", src).unwrap();
        let edges = a.extract_call_edges("m.py", src, &syms);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("m.caller".to_string(), "m.helper".to_string())),
            "missing caller -> helper edge; got {pairs:?}",
        );
        assert!(
            pairs.contains(&("m.C.__init__".to_string(), "m.helper".to_string())),
            "missing C.__init__ -> helper edge; got {pairs:?}",
        );
        assert!(
            pairs.contains(&("m.C.m".to_string(), "m.C.__init__".to_string())),
            "missing C.m -> C.__init__ edge; got {pairs:?}",
        );
    }

    #[test]
    fn default_extract_call_edges_returns_empty_for_unrelated_adapter() {
        // Sanity: ensure the trait method exists and the default returns empty.
        // This pins the behavior so callers can rely on it.
        struct Dummy;
        impl LanguageAdapter for Dummy {
            fn language(&self) -> &str {
                "dummy"
            }
            fn parse_symbols(&self, _f: &str, _s: &str) -> Result<Vec<ParsedSymbol>> {
                Ok(Vec::new())
            }
            fn infer_effects(&self, _s: &str, _p: &ParsedSymbol) -> Vec<Effect> {
                Vec::new()
            }
        }
        let d = Dummy;
        assert!(d.extract_call_edges("x", "", &[]).is_empty());
    }
}
