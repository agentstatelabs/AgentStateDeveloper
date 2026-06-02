//! Python language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-python`. Parses module-level functions, methods,
//! and classes, and runs a small substring-based effect inference pass.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, DynamicDispatchHint, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
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

    fn file_extensions(&self) -> &'static [&'static str] {
        &["py"]
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
        source: &str,
        symbols: &[ParsedSymbol],
        workspace: &WorkspaceSymbols,
    ) -> Vec<CallEdge> {
        extract_call_edges_impl(file, source, symbols, workspace)
    }

    fn scan_dynamic_dispatch(&self, file: &str, source: &str) -> Vec<DynamicDispatchHint> {
        scan_dynamic_dispatch_in_python(file, source)
    }
}

/// Walk path components and return the tail after the first `src` segment.
/// Falls back to the full path for projects without that convention.
///
/// Examples:
/// - `src/mypackage/module.py`  → `mypackage/module.py`
/// - `lib/src/utils.py`         → `utils.py`
/// - `mypackage/module.py`      → `mypackage/module.py`  (no `src`, unchanged)
fn strip_src_prefix(path: &str) -> &str {
    let mut offset = 0usize;
    for part in path.split('/') {
        if part == "src" {
            let after = offset + part.len() + 1;
            if after < path.len() {
                return &path[after..];
            }
        }
        offset += part.len() + 1;
    }
    path
}

/// Derive the dotted module prefix for a file path.
///
/// Anchors at the `src/` boundary for src-layout projects (PEP 517/518)
/// so the prefix is stable regardless of which directory `asd index` was
/// invoked from. Falls back to the full relative path for flat layouts.
///
/// `src/mypackage/module.py`        -> `mypackage.module`
/// `packages/mypkg/src/__init__.py` -> `mypkg.__init__`  (package-name disambiguates)
/// `foo/bar.py`                     -> `foo.bar`          (no src segment, fallback)
/// `./foo/bar.py`                   -> `foo.bar`
/// `bar.py`                         -> `bar`
fn module_qname_prefix(file: &str) -> String {
    let mut s = file;
    if let Some(stripped) = s.strip_prefix("./") {
        s = stripped;
    }
    let s = s.strip_suffix(".py").unwrap_or(s);
    let after_src = strip_src_prefix(s);
    // __init__.py at the root of a package's src/ dir is not unique across packages.
    // Prepend the package directory name (the segment before `src/`).
    if after_src == "__init__" {
        if let Some(pkg_name) = package_name_from_path(s) {
            return format!("{}.__init__", pkg_name.replace('-', "_"));
        }
    }
    after_src.replace('\\', "/").replace('/', ".")
}

/// Extract the package name from a path like `packages/my-pkg/src/__init__.py`
/// by finding the segment immediately before a `src` component.
fn package_name_from_path(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, &part) in parts.iter().enumerate() {
        if part == "src" && i > 0 {
            return Some(parts[i - 1]);
        }
    }
    None
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
    let doc = extract_python_doc(&body);
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
        doc,
    }
}

fn extract_python_doc(body: &str) -> Option<String> {
    for quote in &[r#"""""#, "'''"] {
        if let Some(start) = body.find(quote) {
            let after = start + quote.len();
            if let Some(end_rel) = body[after..].find(quote) {
                let content = &body[after..after + end_rel];
                let cleaned = content
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !cleaned.is_empty() {
                    return Some(truncate_doc(&cleaned));
                }
            }
        }
    }
    None
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

    // Plan L t-002: detection runs on a copy with comments + string
    // literals masked to spaces, so an effect-like substring inside a
    // `# os.open(…)` comment or a `"os.open(…)"` literal no longer
    // produces a false-positive inference. We preserve byte length so
    // all the existing offset-based slicing (`&body[args_start..]`,
    // notes, path extraction) continues to read from the original
    // source — the mask only suppresses *matches*, not data.
    let masked = mask_comments_and_literals(body);
    let scan = masked.as_str();

    // open(...) -> IoFsRead (+ IoFsWrite when mode contains 'w' or 'a')
    for call_site in find_calls(scan, "open(") {
        let args = &body[call_site.args_start..call_site.args_end];
        effects.push(Effect {
            effect: EffectCategory::IoFsRead,
            qualifiers: extract_open_path(args)
                .map(|p| json!({ "paths": [p] }))
                .unwrap_or(serde_json::Value::Null),
            note: Some(trim_note(
                &body[call_site.call_start..call_site.args_end + 1],
            )),
            ..Default::default()
        });
        if mentions_write_mode(args) {
            effects.push(Effect {
                effect: EffectCategory::IoFsWrite,
                qualifiers: extract_open_path(args)
                    .map(|p| json!({ "paths": [p] }))
                    .unwrap_or(serde_json::Value::Null),
                note: Some(trim_note(
                    &body[call_site.call_start..call_site.args_end + 1],
                )),
                ..Default::default()
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
    if log_needles.iter().any(|n| scan.contains(n)) {
        let note = first_matching_line(scan, body, &log_needles);
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note,
            ..Default::default()
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
        for call_site in find_calls(scan, recv) {
            let args = &body[call_site.args_start..call_site.args_end];
            let sql = args.trim_start_matches(|c: char| {
                c.is_whitespace() || c == '"' || c == '\'' || c == 'f' || c == 'r' || c == 'b'
            });
            let upper: String = sql.chars().take(16).collect::<String>().to_uppercase();
            let is_write = upper.starts_with("INSERT")
                || upper.starts_with("UPDATE")
                || upper.starts_with("DELETE")
                || upper.starts_with("REPLACE")
                || upper.starts_with("CREATE")
                || upper.starts_with("DROP")
                || upper.starts_with("ALTER")
                || upper.starts_with("TRUNCATE");
            let is_read = upper.starts_with("SELECT")
                || upper.starts_with("WITH")
                || upper.starts_with("SHOW");
            let note = Some(trim_note(
                &body[call_site.call_start..call_site.args_end + 1],
            ));
            if is_write && !seen_db_write {
                effects.push(Effect {
                    effect: EffectCategory::IoDbWrite,
                    qualifiers: serde_json::Value::Null,
                    note,
                    ..Default::default()
                });
                seen_db_write = true;
            } else if is_read && !seen_db_read {
                effects.push(Effect {
                    effect: EffectCategory::IoDbRead,
                    qualifiers: serde_json::Value::Null,
                    note,
                    ..Default::default()
                });
                seen_db_read = true;
            } else if !is_write && !is_read && !seen_db_read && !seen_db_write {
                // Unrecognized SQL — conservatively emit both.
                effects.push(Effect {
                    effect: EffectCategory::IoDbRead,
                    qualifiers: serde_json::Value::Null,
                    note: note.clone(),
                    ..Default::default()
                });
                effects.push(Effect {
                    effect: EffectCategory::IoDbWrite,
                    qualifiers: serde_json::Value::Null,
                    note,
                    ..Default::default()
                });
                seen_db_read = true;
                seen_db_write = true;
            }
        }
    }
    // .commit() implies a preceding write
    if !seen_db_write && (scan.contains(".commit()") || scan.contains("conn.commit")) {
        effects.push(Effect {
            effect: EffectCategory::IoDbWrite,
            qualifiers: serde_json::Value::Null,
            note: first_matching_line(scan, body, &[".commit()", "conn.commit"]),
            ..Default::default()
        });
    }

    // Network libraries -> IoNetOut
    let net_prefixes = ["requests.", "urllib.", "httpx.", "aiohttp."];
    let mut net_hosts: Vec<String> = Vec::new();
    let mut net_note: Option<String> = None;
    for prefix in net_prefixes {
        for call_site in find_calls(scan, prefix) {
            let args = &body[call_site.args_start..call_site.args_end];
            if let Some(host) = extract_url_host(args) {
                if !net_hosts.contains(&host) {
                    net_hosts.push(host);
                }
            }
            if net_note.is_none() {
                net_note = Some(trim_note(
                    &body[call_site.call_start..call_site.args_end + 1],
                ));
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

    // subprocess.* / os.system / os.exec* -> ProcSpawn
    let proc_patterns = ["subprocess.", "os.system(", "os.exec"];
    if let Some(note) = first_match_note(scan, body, &proc_patterns) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // os.environ / os.getenv -> EnvRead
    let env_patterns = ["os.environ", "os.getenv"];
    if let Some(note) = first_match_note(scan, body, &env_patterns) {
        let mut vars: Vec<String> = Vec::new();
        for call_site in find_calls(scan, "os.getenv(") {
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
            ..Default::default()
        });
    }

    // time.sleep -> TimeSleep
    if let Some(note) = first_match_note(scan, body, &["time.sleep"]) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // time.time / time.monotonic / datetime.now -> TimeRead
    let time_read_patterns = [
        "time.time",
        "time.monotonic",
        "time.perf_counter",
        "datetime.now",
        "datetime.utcnow",
        "datetime.today",
    ];
    if let Some(note) = first_match_note(scan, body, &time_read_patterns) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // random.* / secrets.* -> Random
    let random_patterns = ["random.", "secrets.", "os.urandom("];
    if let Some(note) = first_match_note(scan, body, &random_patterns) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // `raise ` statement -> Throw
    if has_raise_statement(scan) {
        let note = first_matching_line(scan, body, &["raise "]);
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note,
            ..Default::default()
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
                if lit.len() <= 4 && (lit.contains('w') || lit.contains('a') || lit.contains('x')) {
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
/// Cap a doc string at 512 characters, trimming at a word boundary when possible.
fn truncate_doc(s: &str) -> String {
    const MAX: usize = 512;
    if s.len() <= MAX {
        return s.to_string();
    }
    // Try to break at a space before the limit.
    let truncated = &s[..MAX];
    match truncated.rfind(' ') {
        Some(pos) => format!("{}…", &truncated[..pos]),
        None => format!("{}…", truncated),
    }
}

fn trim_note(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or("").trim();
    if first_line.len() > 120 {
        format!("{}...", &first_line[..120])
    } else {
        first_line.to_string()
    }
}

/// Plan L t-002: scan `scan` for a match (so comments / string literals
/// previously masked to spaces don't trigger), but return the matching
/// line from `body` so the note reflects the real source text. `scan`
/// and `body` MUST be the same byte length and split into the same
/// line count for the alignment to hold.
fn first_matching_line(scan: &str, body: &str, needles: &[&str]) -> Option<String> {
    for (i, scan_line) in scan.lines().enumerate() {
        for n in needles {
            if scan_line.contains(n) {
                return body
                    .lines()
                    .nth(i)
                    .map(|l| l.trim().to_string());
            }
        }
    }
    None
}

/// Check each needle against the masked `scan`; return the first
/// matching line from the real `body` as a note.
fn first_match_note(scan: &str, body: &str, needles: &[&str]) -> Option<String> {
    for n in needles {
        if scan.contains(n) {
            return first_matching_line(scan, body, needles);
        }
    }
    None
}

/// Detect a `raise` statement (not `raises` in a docstring or attribute).
/// We check for `raise ` preceded by start-of-line or whitespace.
/// Operates on the masked `scan` so a `raise` inside a comment or
/// string literal doesn't trip the detector.
fn has_raise_statement(scan: &str) -> bool {
    for line in scan.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("raise ") || trimmed == "raise" {
            return true;
        }
    }
    false
}

/// Plan L t-005: scan Python source for call patterns the static
/// call-graph walker can't resolve. Returns one hint per detected
/// site so the indexer can surface a warning. The patterns we
/// flag are deliberately conservative — only the ones that are
/// unambiguously dynamic dispatch:
///
/// - `getattr(obj, "name")(args)` and `getattr(obj, name_var)(args)`
///   — runtime attribute lookup feeding a call.
/// - `__getattr__` / `__getattribute__` method definitions — the
///   class promises to resolve unknown attributes at runtime.
///
/// We deliberately do NOT flag plain `getattr(obj, "name")` (no
/// trailing call) — that's a read, not a dispatch, and gets
/// resolved by the property/attribute pass.
///
/// Detection runs against the masked source (Plan L t-002) so a
/// `# getattr(...)` in a comment doesn't generate a phantom warning.
pub fn scan_dynamic_dispatch_in_python(file: &str, source: &str) -> Vec<DynamicDispatchHint> {
    let masked = mask_comments_and_literals(source);
    let mut hints = Vec::new();
    let bytes = masked.as_bytes();
    let body = source.as_bytes();
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in body.iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_of = |off: usize| -> u32 {
        match line_starts.binary_search(&off) {
            Ok(i) => i as u32 + 1,
            Err(i) => i.max(1) as u32,
        }
    };
    let snippet_at = |off: usize| -> String {
        let line_idx = line_of(off).saturating_sub(1) as usize;
        let start = line_starts.get(line_idx).copied().unwrap_or(0);
        let end = line_starts
            .get(line_idx + 1)
            .copied()
            .unwrap_or(body.len())
            .saturating_sub(1);
        std::str::from_utf8(&body[start..end])
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    // Pattern 1: getattr(obj, …)(…)  — the trailing `(` after the
    // outer `)` is the giveaway. We use find_calls + check the byte
    // right after args_end.
    for site in find_calls(&masked, "getattr(") {
        // Peek past the closing `)` for an opening `(` (allowing
        // whitespace / chained attribute reads in between is more
        // ambitious — for v1 we require an immediate call).
        let mut j = site.args_end + 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'(' {
            hints.push(DynamicDispatchHint {
                file: file.to_string(),
                line: line_of(site.call_start),
                pattern: "getattr".into(),
                snippet: snippet_at(site.call_start),
            });
        }
    }

    // Pattern 2: `def __getattr__(self, …)` / `def __getattribute__(…)`
    // method definitions. Scan line by line; `mask_comments_and_literals`
    // preserves line splits so offsets align.
    for (i, line) in masked.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("def __getattr__(") || trimmed.starts_with("def __getattribute__(") {
            let off = line_starts.get(i).copied().unwrap_or(0);
            hints.push(DynamicDispatchHint {
                file: file.to_string(),
                line: (i + 1) as u32,
                pattern: "__getattr__".into(),
                snippet: snippet_at(off),
            });
        }
    }

    hints
}

/// Plan L t-002: walk Python source and replace bytes inside `#`
/// line comments and string literals (single/double/triple, with
/// optional `r` / `b` / `f` prefixes) with ASCII space. Newlines and
/// every other byte stay where they are, so the output has the same
/// byte length AND the same line split as the input. Callers can
/// detect call patterns against the masked output and use byte
/// offsets to extract real text from the original body.
///
/// Limitations:
/// - F-string interpolations inside `f"{expr}"` are masked along
///   with the rest of the literal. An effect inside an f-string
///   interpolation will be missed. Acceptable trade — the bug being
///   fixed is comments and plain strings producing false positives;
///   under-detecting f-string interpolations is conservative.
/// - Raw strings (`r"..."`) are masked normally; backslashes don't
///   escape the closing quote.
fn mask_comments_and_literals(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out: Vec<u8> = bytes.to_vec();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        let c = bytes[i];
        // Line comment: `#` to end-of-line, but not inside an f-string
        // (we're not in a string here because we'd have entered the
        // literal branch below).
        if c == b'#' {
            let mut j = i;
            while j < n && bytes[j] != b'\n' {
                out[j] = b' ';
                j += 1;
            }
            i = j;
            continue;
        }
        // String literal start. Optional prefix: r/R/b/B/f/F (and
        // combinations like rb, fr, Rb…). The prefix bytes themselves
        // are NOT masked — they're identifiers as far as effect
        // detection is concerned, and erasing them could turn
        // `rb"…"` into `  "…"` which still parses as a literal but
        // changes column counts in error paths. Leave the prefix,
        // mask the literal body.
        let quote_start = find_quote_after_optional_prefix(bytes, i);
        if let Some(qi) = quote_start {
            let q = bytes[qi];
            // Triple quote?
            let triple = qi + 2 < n && bytes[qi + 1] == q && bytes[qi + 2] == q;
            let (literal_start, end_pos) = if triple {
                let inner_start = qi + 3;
                let end = find_triple_close(bytes, inner_start, q);
                (inner_start, end)
            } else {
                let inner_start = qi + 1;
                let end = find_single_close(bytes, inner_start, q);
                (inner_start, end)
            };
            // Mask the literal body (between the open and close
            // quotes). Quotes themselves stay so paren-matching and
            // any human-eyeballing of the masked source still reads.
            for k in literal_start..end_pos.min(n) {
                if out[k] != b'\n' {
                    out[k] = b' ';
                }
            }
            // Advance past the closing quote(s).
            i = end_pos + if triple { 3 } else { 1 };
            continue;
        }
        i += 1;
    }
    // Safety: byte length must be preserved for offset-based slicing
    // upstream. `out` was built from `bytes.to_vec()` and only mutated
    // in place, so length is identical to `body.len()`. The mutations
    // only ever swap a byte for ASCII space (0x20) or leave it alone,
    // both of which keep the buffer valid UTF-8.
    debug_assert_eq!(out.len(), bytes.len());
    String::from_utf8(out).unwrap_or_else(|_| body.to_string())
}

/// Skip optional Python string prefix bytes (`r`, `R`, `b`, `B`, `f`,
/// `F`, in 1- or 2-char combinations like `rb`/`Rb`/`fr`) starting at
/// `i`, returning the index of the opening quote if found. Only
/// activates when `bytes[i]` is one of the prefix bytes AND a quote
/// follows within two bytes — otherwise the caller's `i` is on a
/// normal identifier byte and we return None.
fn find_quote_after_optional_prefix(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    if i >= n {
        return None;
    }
    let c = bytes[i];
    if c == b'"' || c == b'\'' {
        return Some(i);
    }
    let is_prefix = |b: u8| matches!(b, b'r' | b'R' | b'b' | b'B' | b'f' | b'F');
    if !is_prefix(c) {
        return None;
    }
    // The byte before `i` must NOT be an identifier byte — otherwise
    // we're in the middle of a word like `_renderRequest`, not at a
    // string prefix. Treat any ASCII alphanumeric or underscore as
    // identifier-continuing.
    if i > 0 {
        let prev = bytes[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    // 1-char prefix: `r"…"`
    if i + 1 < n && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'') {
        return Some(i + 1);
    }
    // 2-char prefix: `rb"…"` / `fr"…"` / etc.
    if i + 2 < n && is_prefix(bytes[i + 1]) && (bytes[i + 2] == b'"' || bytes[i + 2] == b'\'') {
        return Some(i + 2);
    }
    None
}

/// Find the closing single-quote `q` starting from `start`. Honors
/// backslash escapes. Returns the index of the closing quote, or
/// `bytes.len()` if no close found (treat as "string runs to EOF").
/// A bare newline closes a non-triple string in Python — we honor
/// that so an unterminated literal doesn't mask the rest of the file.
fn find_single_close(bytes: &[u8], start: usize, q: u8) -> usize {
    let n = bytes.len();
    let mut j = start;
    while j < n {
        let b = bytes[j];
        if b == b'\\' {
            j += 2;
            continue;
        }
        if b == b'\n' {
            return j; // unterminated; stop at newline
        }
        if b == q {
            return j;
        }
        j += 1;
    }
    n
}

/// Find the closing `qqq` triple-quote sequence starting from `start`.
/// Returns the index of the FIRST quote in the closing triple, or
/// `bytes.len()` if the literal runs to EOF.
fn find_triple_close(bytes: &[u8], start: usize, q: u8) -> usize {
    let n = bytes.len();
    let mut j = start;
    while j + 2 < n {
        if bytes[j] == b'\\' {
            j += 2;
            continue;
        }
        if bytes[j] == q && bytes[j + 1] == q && bytes[j + 2] == q {
            return j;
        }
        j += 1;
    }
    n
}

// -----------------------------------------------------------------------------
// Call-edge extraction (intra- and cross-module)
// -----------------------------------------------------------------------------

/// Build the (caller_qname -> callee_qname) edge set for one file. Resolution
/// is best-effort and covers:
///
/// - `identifier()` -> intra-module `simple_name -> qname`, then imports
///   (`from foo import bar` / `import foo as f`).
/// - `Class.method()` -> resolve `<module>.Class.method`.
/// - `self.method()` inside `<module>.Class.<fn>` -> resolve to `<module>.Class.method`.
/// - `foo.bar()` where `foo` is an imported module -> resolve via the import
///   map to `<foo_prefix>.bar`, and emit only if that qname exists in the
///   workspace. This is how we capture cross-module calls like
///   `logger.write_log` from `_driver.py`.
///
/// Anything we can't resolve (subscripts, lambdas, star imports, relative
/// imports, dynamic dispatch) is silently skipped.
fn extract_call_edges_impl(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
    workspace: &WorkspaceSymbols,
) -> Vec<CallEdge> {
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

    // Parse the full source once to extract top-level imports. We need the
    // file-level tree (not individual symbol bodies) so we can pick up
    // `import foo` / `from foo import bar` at module scope. module_prefix
    // is passed so relative imports (`from . import x`, `from .foo import y`)
    // can be resolved to absolute qnames — Plan D t-004.
    let imports = parse_imports(source, &mut parser, &module_prefix);

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
            &imports,
            workspace,
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

/// Imports discovered at module scope, keyed by the local binding.
///
/// Each entry records:
/// - `qname_prefix`: the qname (or qname prefix) the local name resolves to.
/// - `is_module`: whether the local name refers to a *module* (resolve
///   `local.attr` -> `qname_prefix.attr`) vs. a single imported symbol
///   (resolve `local.attr` -> `qname_prefix.attr` as well, but `local()`
///   resolves to `qname_prefix` directly).
#[derive(Debug, Clone)]
struct ImportBinding {
    qname_prefix: String,
    /// `true` for `import foo` / `import foo as f`; `false` for
    /// `from foo import bar`. Currently both forms resolve attribute access
    /// via the same path (`qname_prefix` + `.` + `attr`), but we keep the
    /// distinction so future resolvers (e.g., re-exports) can tell apart
    /// module handles from direct symbol bindings.
    #[allow(dead_code)]
    is_module: bool,
}

/// Walk the file's top-level statements and collect import bindings.
///
/// Supports:
/// - `import foo` -> `foo -> ("foo", module)`
/// - `import foo.bar` -> `foo -> ("foo", module)` (only the first segment binds)
/// - `import foo as f` -> `f -> ("foo", module)`
/// - `from foo import bar` -> `bar -> ("foo.bar", symbol)`
/// - `from foo import bar as b` -> `b -> ("foo.bar", symbol)`
/// - `from foo import (a, b, c)` -> each as above
/// - Plan D t-004: relative imports (`from . import x`, `from .foo import y`,
///   `from ..pkg import z`) — resolved against `module_prefix`.
///
/// - Plan L t-004: imports nested inside function bodies, class
///   bodies, and conditional blocks (`if TYPE_CHECKING:`, `try: …
///   except ImportError: …`) are also collected and merged into the
///   same flat map.
///
/// Skips:
/// - `from foo import *` — can't statically resolve members.
///
/// Nested-import scoping policy: we deliberately use a single flat
/// map rather than per-function scoping. Real Python code rarely
/// shadows module-scope imports inside a function under the same
/// local name; when it does (typically `try/except ImportError`
/// fallbacks), either branch resolving to the same prefix is the
/// behavior callers want. The trade-off: a function that genuinely
/// imports a different module under a colliding name would resolve
/// to whichever was inserted last. Acceptable for the call-edge
/// signal we're building.
fn parse_imports(
    source: &str,
    parser: &mut Parser,
    module_prefix: &str,
) -> HashMap<String, ImportBinding> {
    let mut out: HashMap<String, ImportBinding> = HashMap::new();
    let src_bytes = source.as_bytes();
    let tree = match parser.parse(src_bytes, None) {
        Some(t) => t,
        None => return out,
    };
    walk_imports(tree.root_node(), src_bytes, module_prefix, &mut out);
    out
}

/// Plan L t-004: recursively walk the tree picking up every
/// `import_statement` / `import_from_statement` node — module-scope
/// AND nested. Skipping non-import nodes is implicit; we just
/// recurse into every child and dispatch on `kind()`.
///
/// We don't prune at function/class boundaries on purpose: a
/// `if TYPE_CHECKING:` block sits at module scope, a body import
/// sits inside a function, a `try: import cPickle / except: import
/// pickle` sits inside a `try_statement`. Recursing everywhere
/// catches all three with one walk.
fn walk_imports(
    node: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    out: &mut HashMap<String, ImportBinding>,
) {
    match node.kind() {
        "import_statement" => {
            collect_import_statement(node, src, out);
            return;
        }
        "import_from_statement" => {
            collect_import_from_statement(node, src, module_prefix, out);
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, module_prefix, out);
    }
}

/// Plan D t-004: resolve a relative-import dot prefix against the importing
/// file's `module_prefix`. Returns the absolute module qname (e.g.
/// "crucible.agents.base") that the dots + optional suffix point to, or
/// None when the relative import escapes the workspace.
fn resolve_relative_import(module_prefix: &str, raw: &str) -> Option<String> {
    let dot_count = raw.chars().take_while(|c| *c == '.').count();
    if dot_count == 0 {
        return None;
    }
    let suffix = raw[dot_count..].trim().trim_matches('.');
    let parts: Vec<&str> = if module_prefix.is_empty() {
        Vec::new()
    } else {
        module_prefix.split('.').collect()
    };
    if dot_count > parts.len() {
        return None;
    }
    let keep = parts.len() - dot_count;
    let mut base = parts[..keep].join(".");
    if !suffix.is_empty() {
        if !base.is_empty() {
            base.push('.');
        }
        base.push_str(suffix);
    }
    if base.is_empty() { None } else { Some(base) }
}

/// Handle `import a, b as c, d.e`.
fn collect_import_statement(node: Node<'_>, src: &[u8], out: &mut HashMap<String, ImportBinding>) {
    // Tree-sitter-python emits each imported name as a `name` field (which
    // may appear multiple times), each being either `dotted_name` or
    // `aliased_import`.
    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        match child.kind() {
            "dotted_name" => {
                // `import foo.bar.baz` -> local binding is the first segment,
                // but resolves to just that first segment (not the tail —
                // Python only binds the top package name).
                let full = node_text(child, src).unwrap_or_default();
                let full = full.trim().to_string();
                if full.is_empty() {
                    continue;
                }
                let first = full.split('.').next().unwrap_or(&full).to_string();
                out.insert(
                    first.clone(),
                    ImportBinding {
                        qname_prefix: first,
                        is_module: true,
                    },
                );
                // Also register the fully-dotted form so `foo.bar.baz(...)`
                // written explicitly can resolve to a module qname prefix.
                if full.contains('.') {
                    out.insert(
                        full.clone(),
                        ImportBinding {
                            qname_prefix: full,
                            is_module: true,
                        },
                    );
                }
            }
            "aliased_import" => {
                // `import foo as f` / `import foo.bar as fb`
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, src))
                    .unwrap_or_default();
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| node_text(n, src))
                    .unwrap_or_default();
                let name = name.trim().to_string();
                let alias = alias.trim().to_string();
                if name.is_empty() || alias.is_empty() {
                    continue;
                }
                out.insert(
                    alias,
                    ImportBinding {
                        qname_prefix: name,
                        is_module: true,
                    },
                );
            }
            _ => {}
        }
    }
}

/// Handle `from foo import a, b as c` and `from foo import (a, b)`.
fn collect_import_from_statement(
    node: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    out: &mut HashMap<String, ImportBinding>,
) {
    // `module_name` field holds a `dotted_name` or `relative_import`.
    let module_node = node.child_by_field_name("module_name");
    let module_kind = module_node.map(|n| n.kind()).unwrap_or("");
    let module = if module_kind == "relative_import" {
        // Plan D t-004: resolve `.`/`..`/etc. against the importing file's
        // module_prefix. `from . import x` from crucible/agents/foo.py
        // resolves to module "crucible.agents".
        let raw = module_node
            .and_then(|n| node_text(n, src))
            .unwrap_or_default();
        match resolve_relative_import(module_prefix, raw.trim()) {
            Some(m) => m,
            None => return,
        }
    } else {
        let m = module_node
            .and_then(|n| node_text(n, src))
            .unwrap_or_default()
            .trim()
            .to_string();
        if m.is_empty() {
            return;
        }
        m
    };

    // Detect `from foo import *` — tree-sitter-python represents the `*` as
    // a `wildcard_import` child (not a `name` field). Skip in that case.
    let mut cursor_all = node.walk();
    for child in node.children(&mut cursor_all) {
        if child.kind() == "wildcard_import" {
            return;
        }
    }

    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let name = node_text(child, src).unwrap_or_default();
                let name = name.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let qname = format!("{module}.{name}");
                out.insert(
                    name,
                    ImportBinding {
                        qname_prefix: qname,
                        is_module: false,
                    },
                );
            }
            "aliased_import" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, src))
                    .unwrap_or_default();
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| node_text(n, src))
                    .unwrap_or_default();
                let name = name.trim().to_string();
                let alias = alias.trim().to_string();
                if name.is_empty() || alias.is_empty() {
                    continue;
                }
                let qname = format!("{module}.{name}");
                out.insert(
                    alias,
                    ImportBinding {
                        qname_prefix: qname,
                        is_module: false,
                    },
                );
            }
            _ => {}
        }
    }
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
    imports: &HashMap<String, ImportBinding>,
    workspace: &WorkspaceSymbols,
    enclosing_class: Option<&str>,
    out: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call" {
        if let Some(func) = node.child_by_field_name("function") {
            if let Some(callee) = resolve_callee(
                func,
                src,
                module_prefix,
                by_simple,
                known,
                imports,
                workspace,
                enclosing_class,
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
            enclosing_class,
            out,
        );
    }
}

/// Resolve the function-position node of a `call` to a fully-qualified name,
/// when possible. Precedence (LEGB-ish, for our purposes):
///
/// 1. Intra-module `simple_name -> qname` (local definitions shadow imports).
/// 2. Import map (`from foo import bar` / `import foo as f`).
/// 3. For attribute calls, try intra-module class resolution, then imports.
///
/// Cross-module resolutions are only emitted when the resolved qname is
/// present in `workspace.qnames`. This drops stdlib / third-party calls
/// whose symbols we didn't index.
#[allow(clippy::too_many_arguments)]
fn resolve_callee(
    func: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    imports: &HashMap<String, ImportBinding>,
    workspace: &WorkspaceSymbols,
    enclosing_class: Option<&str>,
) -> Option<String> {
    match func.kind() {
        "identifier" => {
            let name = node_text(func, src)?;
            // 1) intra-module first (shadows imports)
            if let Some(q) = by_simple.get(&name) {
                return Some(q.clone());
            }
            // 2) import map (e.g., `from logger import write_log`)
            if let Some(binding) = imports.get(&name) {
                if workspace.contains(&binding.qname_prefix) {
                    return Some(binding.qname_prefix.clone());
                }
            }
            None
        }
        "attribute" => {
            // `<object>.<attribute>` — resolve `self.X`, `Class.X`, and
            // `<imported_module>.X`.
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
                        return None;
                    }

                    // Intra-module class attribute: `Foo.bar` -> `mod.Foo.bar`
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

                    // Imported module or symbol: `logger.write_log` ->
                    // `<prefix>.write_log`, validated against workspace.
                    if let Some(binding) = imports.get(&obj_name) {
                        let candidate = format!("{}.{}", binding.qname_prefix, attr_name);
                        if workspace.contains(&candidate) {
                            return Some(candidate);
                        }
                    }
                    None
                }
                "attribute" => {
                    // Chained attribute like `foo.bar.baz(...)`. Try to
                    // flatten into a dotted path and match against imports.
                    let chain = flatten_attribute(object, src)?;
                    if let Some(binding) = imports.get(&chain) {
                        let candidate = format!("{}.{}", binding.qname_prefix, attr_name);
                        if workspace.contains(&candidate) {
                            return Some(candidate);
                        }
                    }
                    None
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Flatten a nested `attribute` node back into its dotted text form.
/// Returns `None` if any inner node isn't an identifier/attribute chain.
fn flatten_attribute(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node_text(node, src),
        "attribute" => {
            let object = node.child_by_field_name("object")?;
            let attr = node.child_by_field_name("attribute")?;
            let head = flatten_attribute(object, src)?;
            let tail = node_text(attr, src)?;
            Some(format!("{head}.{tail}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_prefix_strips_py_and_leading_dot_slash() {
        // src/ anchor — stable for PEP 517 src-layout projects
        assert_eq!(
            module_qname_prefix("src/mypackage/module.py"),
            "mypackage.module"
        );
        assert_eq!(module_qname_prefix("src/utils.py"), "utils");
        // __init__.py at root of package src/ gets package-name prefix
        assert_eq!(
            module_qname_prefix("packages/mypkg/src/__init__.py"),
            "mypkg.__init__"
        );
        assert_eq!(
            module_qname_prefix("packages/my-pkg/src/__init__.py"),
            "my_pkg.__init__"
        );
        // no src segment — full relative path (fallback)
        assert_eq!(module_qname_prefix("foo/bar.py"), "foo.bar");
        assert_eq!(module_qname_prefix("./foo/bar.py"), "foo.bar");
        assert_eq!(module_qname_prefix("bar.py"), "bar");
    }

    #[test]
    fn strip_src_prefix_variants() {
        assert_eq!(strip_src_prefix("src/pkg/mod.py"), "pkg/mod.py");
        assert_eq!(strip_src_prefix("lib/src/utils.py"), "utils.py");
        assert_eq!(strip_src_prefix("pkg/mod.py"), "pkg/mod.py");
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
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::IoFsRead));
        assert!(cats.contains(&EffectCategory::IoFsWrite));
    }

    #[test]
    fn empty_when_no_patterns() {
        let body = "def f(x):\n    return x + 1\n";
        assert!(infer_effects_from_body(body).is_empty());
    }

    // ----- Plan L t-002: comment + string-literal false-positive guards -----

    #[test]
    fn comment_mentioning_open_does_not_infer_fs_effects() {
        // `open(` appears only in a comment; we must NOT infer fs.read.
        let body = "def f(x):\n    # consider open('/tmp/x') for cache\n    return x\n";
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(
            !cats.contains(&EffectCategory::IoFsRead),
            "open() in a comment must not infer IoFsRead; got {cats:?}"
        );
    }

    #[test]
    fn string_literal_mentioning_open_does_not_infer_fs_effects() {
        // `open(` only inside a string literal — no real call.
        let body = "def f():\n    msg = 'use open(path) to load'\n    return msg\n";
        let effects = infer_effects_from_body(body);
        assert!(
            effects.is_empty(),
            "open() inside a string literal must not infer any effect; got {effects:?}"
        );
    }

    #[test]
    fn triple_string_docstring_mentioning_requests_does_not_infer_net() {
        // A docstring describing what `requests.get` would do — must
        // not infer IoNetOut.
        let body = r#"def f():
    """Pretend we call requests.get('https://example.com') here.

    But we don't actually.
    """
    return 42
"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(
            !cats.contains(&EffectCategory::IoNetOut),
            "requests.* in a docstring must not infer IoNetOut; got {cats:?}"
        );
    }

    #[test]
    fn real_open_still_fires_when_comment_also_mentions_it() {
        // The masking must not suppress a real call site that sits
        // next to a comment containing the same pattern.
        let body = "def f():\n    # open('/tmp/a') would also work\n    open('/tmp/b')\n";
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(
            cats.contains(&EffectCategory::IoFsRead),
            "real open() must still infer IoFsRead even when a comment also mentions it; got {cats:?}"
        );
    }

    #[test]
    fn raise_inside_string_literal_does_not_infer_throw() {
        let body = "def f():\n    msg = 'do not raise here'\n    return msg\n";
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(
            !cats.contains(&EffectCategory::Throw),
            "raise inside a string literal must not infer Throw; got {cats:?}"
        );
    }

    #[test]
    fn mask_preserves_byte_length_and_line_count() {
        // The downstream offset-based slicing relies on masked + body
        // being the same byte length AND splitting into the same lines.
        let body = "x = 'open(\"a\")'  # open(\"b\")\ny = 1\n";
        let masked = mask_comments_and_literals(body);
        assert_eq!(
            masked.len(),
            body.len(),
            "mask must preserve byte length"
        );
        assert_eq!(
            masked.lines().count(),
            body.lines().count(),
            "mask must preserve line count"
        );
        // Spot check: `open(` inside the literal AND the comment must
        // be erased from the masked view.
        assert!(
            !masked.contains("open("),
            "masked source must not contain `open(` from literal or comment"
        );
    }

    #[test]
    fn mask_leaves_identifier_chars_alone() {
        // `request_open` is an identifier, not a string prefix —
        // masking must not eat the `r` thinking it's `r"…"`.
        let body = "request_open = 1\n";
        let masked = mask_comments_and_literals(body);
        assert_eq!(
            masked, body,
            "no comments/literals here; masked must equal original"
        );
    }

    #[test]
    fn mask_handles_raw_and_byte_string_prefixes() {
        let body = "x = r\"open('/tmp/a')\"\ny = b'open(\"b\")'\nz = rb\"open(\"c\")\"\n";
        let effects = infer_effects_from_body(body);
        assert!(
            effects.is_empty(),
            "raw/byte/raw-byte literals must mask their contents; got {effects:?}"
        );
    }

    /// Build a WorkspaceSymbols with just a set of qnames (kinds defaulted
    /// to Function). Tests don't currently exercise the kinds map.
    fn workspace_with(qnames: &[&str]) -> WorkspaceSymbols {
        let mut ws = WorkspaceSymbols::default();
        for q in qnames {
            ws.qnames.insert((*q).to_string());
            ws.kinds.insert((*q).to_string(), SymbolKind::Function);
        }
        ws
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
        let ws = workspace_with(&["m.helper", "m.caller", "m.C", "m.C.__init__", "m.C.m"]);
        let edges = a.extract_call_edges("m.py", src, &syms, &ws);
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
    fn extracts_cross_module_edges_via_from_import() {
        // `from logger import write_log` — `write_log(...)` should resolve
        // to `logger.write_log`.
        let src = r#"
from logger import write_log

def foo():
    write_log("/tmp/x", "hi")
"#;
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("caller.py", src).unwrap();
        let ws = workspace_with(&["logger.write_log", "caller.foo"]);
        let edges = a.extract_call_edges("caller.py", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("caller.foo".to_string(), "logger.write_log".to_string())),
            "missing foo -> logger.write_log edge; got {pairs:?}",
        );
    }

    #[test]
    fn extracts_cross_module_edges_via_module_import() {
        // `import logger` — `logger.write_log(...)` should resolve.
        let src = r#"
import logger

def foo():
    logger.write_log("/tmp/x", "hi")
"#;
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("caller.py", src).unwrap();
        let ws = workspace_with(&["logger.write_log", "caller.foo"]);
        let edges = a.extract_call_edges("caller.py", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("caller.foo".to_string(), "logger.write_log".to_string())),
            "missing foo -> logger.write_log edge; got {pairs:?}",
        );
    }

    #[test]
    fn drops_unknown_imports() {
        // Workspace doesn't know about `stdlib_mod.thing` — no edge emitted.
        let src = r#"
import stdlib_mod

def foo():
    stdlib_mod.thing()
"#;
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("caller.py", src).unwrap();
        let ws = workspace_with(&["caller.foo"]);
        let edges = a.extract_call_edges("caller.py", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.is_empty(),
            "expected no edges for unknown imports; got {pairs:?}",
        );
    }

    #[test]
    fn import_alias_and_from_import_with_alias() {
        let src = r#"
import logger as lg
from greetings import hello as h

def foo():
    lg.write_log("/tmp/x", "hi")
    h("world")
"#;
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("caller.py", src).unwrap();
        let ws = workspace_with(&["logger.write_log", "greetings.hello", "caller.foo"]);
        let edges = a.extract_call_edges("caller.py", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("caller.foo".to_string(), "logger.write_log".to_string())),
            "missing lg.write_log edge; got {pairs:?}",
        );
        assert!(
            pairs.contains(&("caller.foo".to_string(), "greetings.hello".to_string())),
            "missing h() edge; got {pairs:?}",
        );
    }

    #[test]
    fn local_definition_shadows_import() {
        // `helper` defined locally should win over `from x import helper`.
        let src = r#"
from other import helper

def helper(x):
    return x

def foo():
    helper(1)
"#;
        let a = PythonAdapter::new();
        let syms = a.parse_symbols("m.py", src).unwrap();
        let ws = workspace_with(&["other.helper", "m.helper", "m.foo"]);
        let edges = a.extract_call_edges("m.py", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("m.foo".to_string(), "m.helper".to_string())),
            "expected local helper to shadow import; got {pairs:?}",
        );
        assert!(
            !pairs.contains(&("m.foo".to_string(), "other.helper".to_string())),
            "expected imported helper to be shadowed; got {pairs:?}",
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
        let ws = WorkspaceSymbols::default();
        assert!(d.extract_call_edges("x", "", &[], &ws).is_empty());
    }

    // -- Plan D t-004: relative-import resolver -----------------------------

    #[test]
    fn relative_dot_resolves_to_current_package() {
        // crucible/agents/litellm_agent.py → file qname = crucible.agents.litellm_agent
        // `from . import base` → "crucible.agents.base"
        let r = super::resolve_relative_import("crucible.agents.litellm_agent", ".").unwrap();
        assert_eq!(r, "crucible.agents");
    }

    #[test]
    fn relative_dot_with_suffix_appends_module() {
        let r = super::resolve_relative_import("crucible.agents.litellm_agent", ".base").unwrap();
        assert_eq!(r, "crucible.agents.base");
    }

    #[test]
    fn relative_double_dot_goes_up_one_more() {
        // `from ..util import x` from crucible/agents/litellm_agent.py
        // current package = crucible.agents; parent = crucible; "..util" → crucible.util
        let r = super::resolve_relative_import("crucible.agents.litellm_agent", "..util").unwrap();
        assert_eq!(r, "crucible.util");
    }

    #[test]
    fn relative_from_init_resolves_against_package() {
        // crucible/agents/__init__.py has module_prefix = crucible.agents.__init__
        // `from . import base` → drop 1 segment → crucible.agents; +"base" → crucible.agents.base
        let r = super::resolve_relative_import("crucible.agents.__init__", ".base").unwrap();
        assert_eq!(r, "crucible.agents.base");
    }

    #[test]
    fn relative_escaping_workspace_returns_none() {
        // Two dots when we only have one segment to drop.
        assert!(super::resolve_relative_import("crucible", "..foo").is_none());
    }

    // ---- Plan L t-005: dynamic-dispatch scanner -----------------------

    #[test]
    fn detects_getattr_call_pattern() {
        let src = "def dispatch(obj, name):\n    return getattr(obj, name)(42)\n";
        let hints = scan_dynamic_dispatch_in_python("app/dispatch.py", src);
        assert_eq!(hints.len(), 1, "expected 1 getattr hint; got {hints:?}");
        assert_eq!(hints[0].pattern, "getattr");
        assert_eq!(hints[0].line, 2);
        assert!(hints[0].snippet.contains("getattr(obj, name)(42)"));
    }

    #[test]
    fn getattr_without_trailing_call_is_not_flagged() {
        // A bare attribute read — not a dispatch — should NOT warn.
        let src = "def read(obj, name):\n    return getattr(obj, name, None)\n";
        let hints = scan_dynamic_dispatch_in_python("app/read.py", src);
        assert!(
            hints.is_empty(),
            "bare getattr (no trailing call) must not be flagged; got {hints:?}"
        );
    }

    #[test]
    fn detects_getattr_with_string_arg_then_call() {
        let src = "def go(obj):\n    return getattr(obj, 'method')(1, 2)\n";
        let hints = scan_dynamic_dispatch_in_python("app/go.py", src);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].pattern, "getattr");
    }

    #[test]
    fn detects_dunder_getattr_method_definition() {
        let src = "class Proxy:\n    def __getattr__(self, name):\n        return self._lookup(name)\n";
        let hints = scan_dynamic_dispatch_in_python("app/proxy.py", src);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].pattern, "__getattr__");
        assert_eq!(hints[0].line, 2);
    }

    #[test]
    fn detects_dunder_getattribute_method_definition() {
        let src = "class Strict:\n    def __getattribute__(self, name):\n        return super().__getattribute__(name)\n";
        let hints = scan_dynamic_dispatch_in_python("app/strict.py", src);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].pattern, "__getattr__");
    }

    #[test]
    fn getattr_inside_comment_or_string_is_masked() {
        // The scanner runs on masked source — a getattr in a comment
        // or string literal must NOT produce a warning.
        let src = "def f(obj):\n    # consider getattr(obj, 'x')(1) here\n    msg = 'use getattr(obj, x)(args) at runtime'\n    return msg\n";
        let hints = scan_dynamic_dispatch_in_python("app/f.py", src);
        assert!(
            hints.is_empty(),
            "getattr inside comment/string must not be flagged; got {hints:?}"
        );
    }

    // ---- Plan L t-004: nested-import resolution -----------------------

    #[test]
    fn extract_call_edges_resolves_function_body_import() {
        // `def f(): import requests; requests.get(...)` — the import
        // lives inside the function body, not at module scope. The
        // walker must descend into the body and pick it up.
        use crate::PythonAdapter;
        use agentstatedeveloper_core::{
            LanguageAdapter, ParsedSymbol, SymbolKind, WorkspaceSymbols,
        };

        let adapter = PythonAdapter::new();
        let src = "def f():\n    from pkg.util import helper\n    return helper()\n";

        let syms = vec![ParsedSymbol {
            qname: "app.main.f".into(),
            kind: SymbolKind::Function,
            start_line: 1,
            start_col: 0,
            end_line: 3,
            end_col: 22,
            signature: Some("def f()".into()),
            body: src.into(),
            doc: None,
        }];

        let mut ws = WorkspaceSymbols::default();
        ws.qnames.insert("pkg.util.helper".into());
        ws.kinds
            .insert("pkg.util.helper".into(), SymbolKind::Function);
        ws.qnames.insert("app.main.f".into());
        ws.kinds.insert("app.main.f".into(), SymbolKind::Function);
        ws.build_suffix_index();

        let edges = adapter.extract_call_edges("app/main.py", src, &syms, &ws);
        assert!(
            edges.iter().any(|e| e.callee_qname == "pkg.util.helper"),
            "function-body import must produce cross-module edge; got {edges:?}"
        );
    }

    #[test]
    fn extract_call_edges_resolves_if_typechecking_import() {
        // Imports inside `if TYPE_CHECKING:` blocks should also be
        // collected — the walker doesn't prune on conditional nodes.
        use crate::PythonAdapter;
        use agentstatedeveloper_core::{
            LanguageAdapter, ParsedSymbol, SymbolKind, WorkspaceSymbols,
        };

        let adapter = PythonAdapter::new();
        let src = "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from pkg.api import Client\n\ndef use():\n    return Client()\n";

        let syms = vec![ParsedSymbol {
            qname: "app.main.use".into(),
            kind: SymbolKind::Function,
            start_line: 6,
            start_col: 0,
            end_line: 7,
            end_col: 20,
            signature: Some("def use()".into()),
            body: src.into(),
            doc: None,
        }];

        let mut ws = WorkspaceSymbols::default();
        ws.qnames.insert("pkg.api.Client".into());
        ws.kinds
            .insert("pkg.api.Client".into(), SymbolKind::Class);
        ws.qnames.insert("app.main.use".into());
        ws.kinds.insert("app.main.use".into(), SymbolKind::Function);
        ws.build_suffix_index();

        let edges = adapter.extract_call_edges("app/main.py", src, &syms, &ws);
        assert!(
            edges.iter().any(|e| e.callee_qname == "pkg.api.Client"),
            "if TYPE_CHECKING import must resolve; got {edges:?}"
        );
    }

    #[test]
    fn extract_call_edges_resolves_try_except_import() {
        // `try: import cPickle as pickle / except: import pickle` —
        // the try-block import lives inside a `try_statement`. Walker
        // must descend.
        use crate::PythonAdapter;
        use agentstatedeveloper_core::{
            LanguageAdapter, ParsedSymbol, SymbolKind, WorkspaceSymbols,
        };

        let adapter = PythonAdapter::new();
        let src = "try:\n    from pkg.fast import loader\nexcept ImportError:\n    from pkg.slow import loader\n\ndef boot():\n    return loader()\n";

        let syms = vec![ParsedSymbol {
            qname: "app.main.boot".into(),
            kind: SymbolKind::Function,
            start_line: 6,
            start_col: 0,
            end_line: 7,
            end_col: 20,
            signature: Some("def boot()".into()),
            body: src.into(),
            doc: None,
        }];

        let mut ws = WorkspaceSymbols::default();
        ws.qnames.insert("pkg.fast.loader".into());
        ws.kinds
            .insert("pkg.fast.loader".into(), SymbolKind::Function);
        ws.qnames.insert("pkg.slow.loader".into());
        ws.kinds
            .insert("pkg.slow.loader".into(), SymbolKind::Function);
        ws.qnames.insert("app.main.boot".into());
        ws.kinds
            .insert("app.main.boot".into(), SymbolKind::Function);
        ws.build_suffix_index();

        let edges = adapter.extract_call_edges("app/main.py", src, &syms, &ws);
        // Either branch's resolution is acceptable — both are real
        // bindings in the same module. We just need at least one.
        let resolved = edges.iter().any(|e| {
            e.callee_qname == "pkg.fast.loader" || e.callee_qname == "pkg.slow.loader"
        });
        assert!(
            resolved,
            "try/except import must resolve to one of the branches; got {edges:?}"
        );
    }

    #[test]
    fn module_scope_imports_still_resolve_after_nested_walker() {
        // Regression guard: the recursive walker must not break the
        // already-working module-scope case.
        use crate::PythonAdapter;
        use agentstatedeveloper_core::{
            LanguageAdapter, ParsedSymbol, SymbolKind, WorkspaceSymbols,
        };

        let adapter = PythonAdapter::new();
        let src = "from pkg.util import helper\n\ndef f():\n    return helper()\n";

        let syms = vec![ParsedSymbol {
            qname: "app.main.f".into(),
            kind: SymbolKind::Function,
            start_line: 3,
            start_col: 0,
            end_line: 4,
            end_col: 22,
            signature: Some("def f()".into()),
            body: src.into(),
            doc: None,
        }];

        let mut ws = WorkspaceSymbols::default();
        ws.qnames.insert("pkg.util.helper".into());
        ws.kinds
            .insert("pkg.util.helper".into(), SymbolKind::Function);
        ws.qnames.insert("app.main.f".into());
        ws.kinds.insert("app.main.f".into(), SymbolKind::Function);
        ws.build_suffix_index();

        let edges = adapter.extract_call_edges("app/main.py", src, &syms, &ws);
        assert!(
            edges.iter().any(|e| e.callee_qname == "pkg.util.helper"),
            "module-scope import must still resolve; got {edges:?}"
        );
    }

    #[test]
    fn extract_call_edges_resolves_double_dot_relative_import() {
        // Plan L t-003 acceptance gate: the double-dot case
        // (`from ..pkg import x`) was tested in isolation
        // (`relative_double_dot_goes_up_one_more`) but never end-to-end.
        // This locks in the full pipeline behavior so future refactors
        // can't silently regress on the two-dot case.
        use crate::PythonAdapter;
        use agentstatedeveloper_core::{
            LanguageAdapter, ParsedSymbol, SymbolKind, WorkspaceSymbols,
        };

        let adapter = PythonAdapter::new();
        // crucible.agents.litellm_agent imports from crucible.util
        // (two dots up: agents → crucible, then .util).
        let caller_src =
            "from ..util import helper\n\ndef act():\n    return helper()\n";

        let caller_syms = vec![ParsedSymbol {
            qname: "crucible.agents.litellm_agent.act".into(),
            kind: SymbolKind::Function,
            start_line: 3,
            start_col: 0,
            end_line: 4,
            end_col: 22,
            signature: Some("def act()".into()),
            body: caller_src.into(),
            doc: None,
        }];

        let mut workspace = WorkspaceSymbols::default();
        workspace.qnames.insert("crucible.util.helper".into());
        workspace
            .kinds
            .insert("crucible.util.helper".into(), SymbolKind::Function);
        workspace
            .qnames
            .insert("crucible.agents.litellm_agent.act".into());
        workspace.kinds.insert(
            "crucible.agents.litellm_agent.act".into(),
            SymbolKind::Function,
        );
        workspace.build_suffix_index();

        let edges = adapter.extract_call_edges(
            "crucible/agents/litellm_agent.py",
            caller_src,
            &caller_syms,
            &workspace,
        );

        assert!(
            edges
                .iter()
                .any(|e| e.callee_qname == "crucible.util.helper"),
            "expected double-dot relative import to produce cross-module edge to crucible.util.helper; got {edges:?}"
        );
    }

    #[test]
    fn extract_call_edges_resolves_relative_import_to_cross_module() {
        // End-to-end: a relative import + a call should produce a cross-module
        // CallEdge. This is the Crucible reproducer collapsed to a unit test.
        use crate::PythonAdapter;
        use agentstatedeveloper_core::{
            LanguageAdapter, ParsedSymbol, SymbolKind, WorkspaceSymbols,
        };

        let adapter = PythonAdapter::new();
        let caller_src =
            "from .base import make_decision\n\ndef act():\n    return make_decision()\n";

        let caller_syms = vec![ParsedSymbol {
            qname: "crucible.agents.litellm_agent.act".into(),
            kind: SymbolKind::Function,
            start_line: 3,
            start_col: 0,
            end_line: 4,
            end_col: 27,
            signature: Some("def act()".into()),
            body: caller_src.into(),
            doc: None,
        }];

        let mut workspace = WorkspaceSymbols::default();
        workspace
            .qnames
            .insert("crucible.agents.base.make_decision".into());
        workspace.kinds.insert(
            "crucible.agents.base.make_decision".into(),
            SymbolKind::Function,
        );
        workspace
            .qnames
            .insert("crucible.agents.litellm_agent.act".into());
        workspace.kinds.insert(
            "crucible.agents.litellm_agent.act".into(),
            SymbolKind::Function,
        );
        workspace.build_suffix_index();

        let edges = adapter.extract_call_edges(
            "crucible/agents/litellm_agent.py",
            caller_src,
            &caller_syms,
            &workspace,
        );

        assert!(
            edges
                .iter()
                .any(|e| e.callee_qname == "crucible.agents.base.make_decision"),
            "expected cross-module edge to crucible.agents.base.make_decision; got {edges:?}"
        );
    }
}
