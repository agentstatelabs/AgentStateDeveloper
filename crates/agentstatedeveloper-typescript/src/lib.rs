//! TypeScript / JavaScript language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-typescript`. Parses module-level functions,
//! methods, classes, and arrow-function-bound variable declarations, then
//! runs a small substring-based effect inference pass and extracts
//! intra- + cross-module call edges.
//!
//! Tree-sitter API: this crate targets tree-sitter-typescript 0.23.x, which
//! exposes `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX` as `LanguageFn`
//! constants. If you bump to a later 0.23.x and the build breaks, switch
//! to the `language_typescript()` / `language_tsx()` functions.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Language, Node, Parser};

/// TypeScript / JavaScript language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct TypeScriptAdapter;

impl TypeScriptAdapter {
    pub fn new() -> Self {
        Self
    }
}

/// Pick the right tree-sitter grammar for a file. TSX for `.tsx` / `.jsx`,
/// plain TypeScript for everything else.
fn grammar_for(file: &str) -> Language {
    let lower = file.to_ascii_lowercase();
    if lower.ends_with(".tsx") || lower.ends_with(".jsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }
}

impl LanguageAdapter for TypeScriptAdapter {
    fn language(&self) -> &str {
        "typescript"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "mts", "cts"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&grammar_for(file))
            .map_err(|e| AsdError::Parse(format!("failed to set typescript language: {e}")))?;

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
}

/// Walk path components and return the tail after the first `src` segment.
/// Falls back to the full path for projects without that convention.
///
/// Examples:
/// - `src/components/Button.tsx` → `components/Button.tsx`
/// - `packages/ui/src/index.ts`  → `index.ts`
/// - `lib/util.ts`               → `lib/util.ts`  (no `src` segment, unchanged)
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
/// Anchors at the `src/` boundary so the prefix is stable regardless of
/// which directory `asd index` was invoked from.
///
/// `src/components/Button.tsx`      -> `components.Button`
/// `packages/ui/src/index.ts`       -> `ui.index`   (package-name disambiguates)
/// `foo/bar.ts`                     -> `foo.bar`    (no src segment, fallback)
/// `./foo/bar.tsx`                  -> `foo.bar`
/// `bar.mts`                        -> `bar`
fn module_qname_prefix(file: &str) -> String {
    let mut s = file;
    if let Some(stripped) = s.strip_prefix("./") {
        s = stripped;
    }
    for ext in [".tsx", ".ts", ".mts", ".cts", ".jsx", ".js", ".mjs", ".cjs"] {
        if let Some(stripped) = s.strip_suffix(ext) {
            s = stripped;
            break;
        }
    }
    let after_src = strip_src_prefix(s);
    // index.ts at the root of a package's src/ dir is not unique across packages.
    // Prepend the package directory name (the segment before `src/`).
    if after_src == "index" {
        if let Some(pkg_name) = package_name_from_path(s) {
            return format!("{}.index", pkg_name.replace('-', "_"));
        }
    }
    after_src.replace('\\', "/").replace('/', ".")
}

/// Extract the package name from a path like `packages/my-pkg/src/index.ts`
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum ScopeKind {
    Class,
    Function,
    Namespace,
}

/// Recursive descent over a TS tree. We enumerate:
/// - `function_declaration` -> Function
/// - `method_definition` (inside `class_body`) -> Method
/// - `class_declaration` -> Class
/// - `lexical_declaration` / `variable_declaration` with an arrow-function
///   or function-expression initializer -> Function
/// - `export_statement` wrappers — unwrapped to the inner declaration
/// - `internal_module` / `module` (namespace X { ... }) -> walked with a
///   namespace-prefixed qname; the namespace itself is not emitted.
///
/// Nested arrow functions inside function bodies are NOT emitted — they'd
/// be anonymous and noisy. Interfaces and type aliases are skipped (not
/// callable).
fn walk(
    node: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    scope: &[(String, ScopeKind)],
    out: &mut Vec<ParsedSymbol>,
) {
    let kind = node.kind();
    match kind {
        "function_declaration" | "generator_function_declaration" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            let symbol_kind = if scope.last().map(|s| s.1) == Some(ScopeKind::Class) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let signature = extract_function_signature(node, src, &name);
            out.push(make_parsed_symbol(node, src, qname, symbol_kind, signature));
            // Don't recurse: nested arrow-in-variable and nested fn decls
            // inside function bodies are intentionally skipped.
        }
        "method_definition" => {
            let name = method_name_text(node, src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            let signature = extract_function_signature(node, src, &name);
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Method,
                signature,
            ));
            // Don't descend into method bodies for nested arrows.
        }
        "class_declaration" | "abstract_class_declaration" => {
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
        "lexical_declaration" | "variable_declaration" => {
            // `const foo = () => {}`, `let bar = function() {}` — one
            // declaration may bind multiple variables.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() != "variable_declarator" {
                    continue;
                }
                process_variable_declarator(child, src, module_prefix, scope, out);
            }
        }
        "export_statement" => {
            // Unwrap `export function foo()`, `export class C`,
            // `export const foo = () => {}`, `export default function()`.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_declaration"
                    | "generator_function_declaration"
                    | "class_declaration"
                    | "abstract_class_declaration"
                    | "lexical_declaration"
                    | "variable_declaration"
                    | "internal_module"
                    | "module" => {
                        walk(child, src, module_prefix, scope, out);
                    }
                    _ => {
                        // `export default function() { ... }` -> the function
                        // expression is a direct child. Treat as anonymous
                        // default export.
                        if matches!(
                            child.kind(),
                            "function_expression" | "arrow_function" | "generator_function"
                        ) {
                            let qname = build_qname(module_prefix, scope, "default");
                            let signature = extract_function_signature(child, src, "default");
                            out.push(make_parsed_symbol(
                                child,
                                src,
                                qname,
                                SymbolKind::Function,
                                signature,
                            ));
                        }
                    }
                }
            }
        }
        "internal_module" | "module" => {
            // `namespace Foo { ... }` — don't emit the namespace itself, but
            // walk its body with the namespace pushed onto the scope.
            let name = node_field_text(node, "name", src).unwrap_or_default();
            if !name.is_empty() {
                let mut new_scope = scope.to_vec();
                new_scope.push((name, ScopeKind::Namespace));
                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        walk(child, src, module_prefix, &new_scope, out);
                    }
                }
            }
        }
        _ => {
            // Descend through program root, statement lists, etc.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk(child, src, module_prefix, scope, out);
            }
        }
    }
}

/// Inspect a `variable_declarator` node to see if its initializer is a
/// function/arrow expression. If so, emit a Function symbol.
fn process_variable_declarator(
    node: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    scope: &[(String, ScopeKind)],
    out: &mut Vec<ParsedSymbol>,
) {
    let name_node = match node.child_by_field_name("name") {
        Some(n) => n,
        None => return,
    };
    // Skip destructuring patterns — binding names aren't a single identifier.
    if name_node.kind() != "identifier" {
        return;
    }
    let value = match node.child_by_field_name("value") {
        Some(v) => v,
        None => return,
    };
    match value.kind() {
        "arrow_function" | "function_expression" | "generator_function" => {
            let name = match node_text(name_node, src) {
                Some(n) => n,
                None => return,
            };
            let qname = build_qname(module_prefix, scope, &name);
            let signature = extract_function_signature(value, src, &name);
            // Use the declarator node's span so callers see the whole
            // `foo = () => {...}` in the body; that makes effect inference
            // line up with how Python handles `def foo():`.
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Function,
                signature,
            ));
        }
        _ => {}
    }
}

/// Extract the method name from a `method_definition`. The grammar's
/// `name` field may be an identifier, a property_identifier, or a computed
/// property — we only handle the plain identifier forms.
fn method_name_text(node: Node<'_>, src: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    match name.kind() {
        "property_identifier" | "identifier" | "private_property_identifier" => node_text(name, src),
        _ => None,
    }
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
    let doc = extract_preceding_doc(src, node.start_byte());
    ParsedSymbol {
        qname,
        kind,
        start_line: (start.row as u32) + 1,
        start_col: (start.column as u32) + 1,
        end_line: (end.row as u32) + 1,
        end_col: (end.column as u32) + 1,
        body,
        signature,
        doc,
    }
}

fn extract_preceding_doc(src: &[u8], start_byte: usize) -> Option<String> {
    let before = std::str::from_utf8(&src[..start_byte]).ok()?;
    let trimmed_end = before.trim_end();
    if trimmed_end.ends_with("*/") {
        if let Some(block_start) = trimmed_end.rfind("/**") {
            let block = &trimmed_end[block_start + 3..trimmed_end.len() - 2];
            let cleaned = block
                .lines()
                .map(|l| l.trim().trim_start_matches('*').trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !cleaned.is_empty() {
                return Some(truncate_doc(&cleaned));
            }
        }
    }
    let lines: Vec<&str> = trimmed_end.lines().collect();
    let mut comment_lines: Vec<&str> = Vec::new();
    for line in lines.iter().rev() {
        let t = line.trim();
        if t.starts_with("//") {
            comment_lines.push(t.trim_start_matches("//").trim());
        } else {
            break;
        }
    }
    if !comment_lines.is_empty() {
        comment_lines.reverse();
        return Some(truncate_doc(&comment_lines.join(" ")));
    }
    None
}

fn truncate_doc(s: &str) -> String {
    const MAX: usize = 512;
    if s.len() <= MAX {
        return s.to_string();
    }
    let cut = s[..MAX].rfind(' ').unwrap_or(MAX);
    s[..cut].to_string()
}

fn extract_function_signature(node: Node<'_>, src: &[u8], name: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_text = node_text(params, src)?;
    let ret = node
        .child_by_field_name("return_type")
        .and_then(|n| node_text(n, src))
        .unwrap_or_default();
    Some(format!("function {name}{params_text}{ret}"))
}

fn extract_class_signature(node: Node<'_>, src: &[u8], name: &str) -> Option<String> {
    let heritage = node
        .child_by_field_name("heritage")
        .or_else(|| find_child_by_kind(node, "class_heritage"))
        .and_then(|n| node_text(n, src))
        .unwrap_or_default();
    if heritage.is_empty() {
        Some(format!("class {name}"))
    } else {
        Some(format!("class {name} {}", heritage.trim()))
    }
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Effect inference
// -----------------------------------------------------------------------------

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // console.* -> Log
    let console_needles = [
        "console.log",
        "console.info",
        "console.warn",
        "console.error",
        "console.debug",
    ];
    if console_needles.iter().any(|n| body.contains(n)) {
        let note = first_matching_line(body, &console_needles);
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note,
            ..Default::default()
        });
    }

    // fs read calls -> IoFsRead
    let fs_read_needles = [
        "fs.readFile(",
        "fs.readFileSync(",
        "fs.promises.readFile(",
        "fs.createReadStream(",
        "fs.open(",
    ];
    if let Some(note) = first_match_note(body, &fs_read_needles) {
        effects.push(Effect {
            effect: EffectCategory::IoFsRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // fs write calls -> IoFsWrite
    let fs_write_needles = [
        "fs.writeFile(",
        "fs.writeFileSync(",
        "fs.promises.writeFile(",
        "fs.createWriteStream(",
        "fs.appendFile(",
        "fs.appendFileSync(",
        "fs.unlink(",
        "fs.unlinkSync(",
        "fs.rename(",
        "fs.renameSync(",
    ];
    if let Some(note) = first_match_note(body, &fs_write_needles) {
        effects.push(Effect {
            effect: EffectCategory::IoFsWrite,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Network -> IoNetOut, with URL-host qualifier if we can find one.
    let net_needles = [
        "fetch(",
        "axios.",
        "http.request(",
        "https.request(",
        "http.get(",
        "https.get(",
    ];
    let mut net_hosts: Vec<String> = Vec::new();
    let mut net_note: Option<String> = None;
    for needle in net_needles {
        if needle.ends_with('(') {
            for call_site in find_calls(body, needle) {
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
        } else if body.contains(needle) && net_note.is_none() {
            net_note = first_matching_line(body, &[needle]);
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

    // child_process -> ProcSpawn. Include the bare `spawn(` / `exec(` /
    // `execFile(` forms too, which show up after destructured imports.
    let proc_needles = [
        "child_process.exec",
        "child_process.spawn",
        "child_process.execSync",
        "child_process.execFile",
        "spawn(",
        "exec(",
        "execSync(",
        "execFile(",
    ];
    if let Some(note) = first_match_note(body, &proc_needles) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // process.env.* -> EnvRead, with the var name if we can parse it.
    if body.contains("process.env") {
        let mut vars: Vec<String> = Vec::new();
        // `process.env.FOO` and `process.env["FOO"]`
        for (idx, _) in body.match_indices("process.env.") {
            let tail = &body[idx + "process.env.".len()..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let name = &tail[..end];
            if !name.is_empty() && !vars.contains(&name.to_string()) {
                vars.push(name.to_string());
            }
        }
        for (idx, _) in body.match_indices("process.env[") {
            let tail = &body[idx + "process.env[".len()..];
            if let Some(v) = extract_first_string_literal(tail) {
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
            note: first_matching_line(body, &["process.env"]),
            ..Default::default()
        });
    }

    // setTimeout / setInterval -> TimeSleep
    let sleep_needles = ["setTimeout(", "setInterval("];
    if let Some(note) = first_match_note(body, &sleep_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Date.now / new Date / performance.now -> TimeRead
    let time_read_needles = ["Date.now", "new Date(", "performance.now", "Date.parse(", "Temporal.Now"];
    if let Some(note) = first_match_note(body, &time_read_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Math.random / crypto.random* -> Random
    let random_needles = [
        "Math.random",
        "crypto.randomBytes",
        "crypto.randomUUID",
        "crypto.getRandomValues",
        "nanoid(",
        "uuidv4(",
        "uuid.v4(",
    ];
    if let Some(note) = first_match_note(body, &random_needles) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // DB calls — heuristic: `<receiver>.query(` / `.execute(` where the
    // receiver matches a db-ish name. We inspect the first arg for a SQL
    // keyword to classify read vs write.
    let db_receivers = ["db", "conn", "client", "pool", "knex", "prisma"];
    let db_methods = ["query", "execute"];
    let mut seen_db_read = false;
    let mut seen_db_write = false;
    for recv in &db_receivers {
        for method in &db_methods {
            let needle = format!("{recv}.{method}(");
            // Also check `this.<recv>.<method>(` form for methods.
            let this_needle = format!("this.{recv}.{method}(");
            for variant in &[needle.as_str(), this_needle.as_str()] {
                for call_site in find_calls(body, variant) {
                    let args = &body[call_site.args_start..call_site.args_end];
                    let sql = args
                        .trim_start_matches(|c: char| {
                            c.is_whitespace() || c == '"' || c == '\'' || c == '`'
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
                    let note = Some(trim_note(&body[call_site.call_start..call_site.args_end + 1]));
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
        }
    }

    // `throw ` statement -> Throw
    if has_throw_statement(body) {
        let note = first_matching_line(body, &["throw "]);
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note,
            ..Default::default()
        });
    }

    effects
}

/// Scan for a literal needle and return the lines we recognize as actual
/// call sites. We do not attempt to skip string literals — plain substring
/// is good enough for M6.
#[derive(Debug, Clone, Copy)]
struct CallSite {
    call_start: usize,
    args_start: usize,
    args_end: usize,
}

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

/// Given the position just after `(`, find the matching `)`. Handles
/// nested parens and single/double/backtick quoted strings.
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
                b'"' | b'\'' | b'`' => in_string = Some(c),
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

/// If `args` starts with a quoted string literal, return its contents.
fn extract_first_string_literal(args: &str) -> Option<String> {
    let trimmed = args.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let q = bytes[0];
    if q != b'"' && q != b'\'' && q != b'`' {
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
    for scheme in ["https://", "http://"] {
        if let Some(idx) = args.find(scheme) {
            let tail = &args[idx + scheme.len()..];
            let end = tail
                .find(|c: char| {
                    c == '/'
                        || c == '"'
                        || c == '\''
                        || c == '`'
                        || c == ')'
                        || c == '?'
                        || c.is_whitespace()
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

/// Return the first matching line if any needle appears in `body`.
fn first_match_note(body: &str, needles: &[&str]) -> Option<String> {
    for n in needles {
        if body.contains(n) {
            return first_matching_line(body, needles);
        }
    }
    None
}

/// Detect a `throw` statement (not the word "throw" mid-comment).
fn has_throw_statement(body: &str) -> bool {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("throw ") || trimmed == "throw" {
            return true;
        }
    }
    false
}

// -----------------------------------------------------------------------------
// Call-edge extraction (intra- and cross-module)
// -----------------------------------------------------------------------------

fn extract_call_edges_impl(
    file: &str,
    source: &str,
    symbols: &[ParsedSymbol],
    workspace: &WorkspaceSymbols,
) -> Vec<CallEdge> {
    let module_prefix = module_qname_prefix(file);

    // All known qnames in this file.
    let known: HashSet<&str> = symbols.iter().map(|s| s.qname.as_str()).collect();

    // simple_name -> qname for single-identifier resolution.
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
    if parser.set_language(&grammar_for(file)).is_err() {
        return Vec::new();
    }

    let imports = parse_imports(source, file, &mut parser);

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

/// Imports discovered at module scope, keyed by local binding.
#[derive(Debug, Clone)]
struct ImportBinding {
    /// The qname (or qname prefix) the local name resolves to.
    qname_prefix: String,
    /// `true` when the binding refers to a module handle (namespace import
    /// or default import mapped to module); `false` when it's a single
    /// imported symbol.
    #[allow(dead_code)]
    is_module: bool,
}

/// Walk top-level statements and collect import bindings.
///
/// Supports:
/// - `import { foo, bar as b } from 'mod'`
/// - `import foo from 'mod'` (default — bound as `mod.default`)
/// - `import * as ns from 'mod'` — namespace binding pointing at `mod`
/// - `import foo, { bar } from 'mod'` — both default and named
/// - `import 'mod'` — no bindings
///
/// Relative specifiers (`./foo`, `../foo`) are resolved against the
/// importing file's directory. Bare specifiers (`react`, `lodash`) are
/// kept as-is; the workspace lookup will filter them out.
///
/// Skipped for M6: re-exports (`export { X } from 'mod'`).
fn parse_imports(
    source: &str,
    file: &str,
    parser: &mut Parser,
) -> HashMap<String, ImportBinding> {
    let mut out: HashMap<String, ImportBinding> = HashMap::new();
    let src_bytes = source.as_bytes();
    let tree = match parser.parse(src_bytes, None) {
        Some(t) => t,
        None => return out,
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_statement" {
            collect_import_statement(child, src_bytes, file, &mut out);
        }
    }
    out
}

fn collect_import_statement(
    node: Node<'_>,
    src: &[u8],
    file: &str,
    out: &mut HashMap<String, ImportBinding>,
) {
    // `source` field holds the module specifier — a `string` node. Its text
    // is quoted; strip quotes then resolve relative to the current file.
    let source_node = match node.child_by_field_name("source") {
        Some(n) => n,
        None => return,
    };
    let raw = match node_text(source_node, src) {
        Some(s) => s,
        None => return,
    };
    let specifier = raw.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`');
    let module_qname = resolve_module_specifier(specifier, file);

    // Find the import_clause child (if any — `import 'mod'` has none).
    let clause = find_child_by_kind(node, "import_clause");
    let clause = match clause {
        Some(c) => c,
        None => return,
    };

    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                // Default import: `import foo from 'mod'`
                let name = match node_text(child, src) {
                    Some(n) => n,
                    None => continue,
                };
                // ASD doesn't index `default` symbols, so bind the local
                // name to a module handle; callers that use the bare name
                // will resolve via `by_simple`/imports anyway. This keeps
                // `foo.something` callable if `mod` is a namespace-like
                // thing, which aligns with how CommonJS default objects work.
                out.insert(
                    name,
                    ImportBinding {
                        qname_prefix: module_qname.clone(),
                        is_module: true,
                    },
                );
            }
            "namespace_import" => {
                // `import * as foo from 'mod'`
                // The identifier child holds the local name.
                if let Some(ident) = find_child_by_kind(child, "identifier") {
                    if let Some(name) = node_text(ident, src) {
                        out.insert(
                            name,
                            ImportBinding {
                                qname_prefix: module_qname.clone(),
                                is_module: true,
                            },
                        );
                    }
                }
            }
            "named_imports" => {
                // `{ foo, bar as b }`
                let mut c2 = child.walk();
                for spec in child.children(&mut c2) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    let name = spec
                        .child_by_field_name("name")
                        .and_then(|n| node_text(n, src));
                    let alias = spec
                        .child_by_field_name("alias")
                        .and_then(|n| node_text(n, src));
                    let name = match name {
                        Some(n) => n,
                        None => continue,
                    };
                    let local = alias.unwrap_or_else(|| name.clone());
                    let qname = if module_qname.is_empty() {
                        name.clone()
                    } else {
                        format!("{module_qname}.{name}")
                    };
                    out.insert(
                        local,
                        ImportBinding {
                            qname_prefix: qname,
                            is_module: false,
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

/// Resolve a module specifier relative to the importing file's directory.
/// Returns a dotted qname prefix suitable for matching workspace symbols.
///
/// - `./foo` in `src/a.ts` -> `src.foo`
/// - `./sub/bar` in `src/a.ts` -> `src.sub.bar`
/// - `../x` in `src/a/b.ts` -> `src.x`
/// - bare `'react'` stays `react` (won't match the workspace; dropped later)
fn resolve_module_specifier(spec: &str, importing_file: &str) -> String {
    // Strip common extensions — callers may or may not include them.
    let mut s = spec.trim();
    for ext in [".tsx", ".ts", ".mts", ".cts", ".jsx", ".js", ".mjs", ".cjs"] {
        if let Some(stripped) = s.strip_suffix(ext) {
            s = stripped;
            break;
        }
    }
    if !(s.starts_with("./") || s.starts_with("../") || s.starts_with('/')) {
        // Bare specifier — leave alone. It won't match workspace qnames
        // unless some other file happens to live at that qname, which is
        // correct behavior.
        return s.replace('/', ".");
    }

    // Compute the importing file's directory as a list of segments.
    let normalized = importing_file.replace('\\', "/");
    let mut dir_parts: Vec<&str> = normalized
        .strip_prefix("./")
        .unwrap_or(normalized.as_str())
        .split('/')
        .collect();
    dir_parts.pop(); // drop the filename itself

    // Collapse the spec against the directory.
    let spec_path = s.strip_prefix('/').unwrap_or(s);
    let mut parts: Vec<String> = dir_parts.iter().map(|s| s.to_string()).collect();
    for seg in spec_path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join(".")
}

fn enclosing_class_qname(
    qname: &str,
    module_prefix: &str,
    known: &HashSet<&str>,
) -> Option<String> {
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
    enclosing_class: Option<&str>,
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
                enclosing_class,
            ) {
                out.insert(CallEdge {
                    caller_qname: sym.qname.clone(),
                    callee_qname: callee,
                });
            }
        }
    }
    if node.kind() == "new_expression" {
        // `new Foo(...)` — treat as a call to `Foo`.
        if let Some(cons) = node.child_by_field_name("constructor") {
            if let Some(callee) = resolve_callee(
                cons,
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
            // 2) import map (e.g., `import { foo } from './mod'`)
            if let Some(binding) = imports.get(&name) {
                if workspace.contains(&binding.qname_prefix) {
                    return Some(binding.qname_prefix.clone());
                }
            }
            None
        }
        "member_expression" => {
            // `<object>.<property>`
            let object = func.child_by_field_name("object")?;
            let property = func.child_by_field_name("property")?;
            let prop_name = node_text(property, src)?;

            match object.kind() {
                "this" => {
                    let class = enclosing_class?;
                    let candidate = format!("{class}.{prop_name}");
                    if known.contains(candidate.as_str()) {
                        return Some(candidate);
                    }
                    None
                }
                "identifier" => {
                    let obj_name = node_text(object, src)?;

                    // Intra-module class reference: `Foo.bar` -> `mod.Foo.bar`
                    let class_qname = if module_prefix.is_empty() {
                        obj_name.clone()
                    } else {
                        format!("{module_prefix}.{obj_name}")
                    };
                    if known.contains(class_qname.as_str()) {
                        let candidate = format!("{class_qname}.{prop_name}");
                        if known.contains(candidate.as_str()) {
                            return Some(candidate);
                        }
                    }

                    // Imported module or symbol: `logger.writeLog` ->
                    // `<prefix>.writeLog`, validated against workspace.
                    if let Some(binding) = imports.get(&obj_name) {
                        let candidate = format!("{}.{}", binding.qname_prefix, prop_name);
                        if workspace.contains(&candidate) {
                            return Some(candidate);
                        }
                    }
                    None
                }
                "member_expression" => {
                    // Chained: `foo.bar.baz()`. Flatten and try imports.
                    let chain = flatten_member(object, src)?;
                    if let Some(binding) = imports.get(&chain) {
                        let candidate = format!("{}.{}", binding.qname_prefix, prop_name);
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

fn flatten_member(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node_text(node, src),
        "member_expression" => {
            let object = node.child_by_field_name("object")?;
            let property = node.child_by_field_name("property")?;
            let head = flatten_member(object, src)?;
            let tail = node_text(property, src)?;
            Some(format!("{head}.{tail}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_prefix_strips_extensions_and_leading_dot_slash() {
        // src/ anchor — stable regardless of index root
        assert_eq!(module_qname_prefix("src/components/Button.tsx"), "components.Button");
        assert_eq!(module_qname_prefix("packages/ui/src/index.ts"), "ui.index");
        assert_eq!(module_qname_prefix("packages/my-pkg/src/index.tsx"), "my_pkg.index");
        assert_eq!(module_qname_prefix("src/util.mts"), "util");
        // no src segment — full relative path (fallback)
        assert_eq!(module_qname_prefix("foo/bar.ts"), "foo.bar");
        assert_eq!(module_qname_prefix("./foo/bar.tsx"), "foo.bar");
        assert_eq!(module_qname_prefix("bar.mts"), "bar");
        assert_eq!(module_qname_prefix("a/b/c.cts"), "a.b.c");
    }

    #[test]
    fn strip_src_prefix_variants() {
        assert_eq!(strip_src_prefix("src/util.ts"), "util.ts");
        assert_eq!(strip_src_prefix("packages/ui/src/index.ts"), "index.ts");
        assert_eq!(strip_src_prefix("lib/util.ts"), "lib/util.ts");
        assert_eq!(strip_src_prefix("nosrc/foo.ts"), "nosrc/foo.ts");
    }

    #[test]
    fn parses_function_method_and_class() {
        let src = r#"
export function top(): number {
    return 1;
}

export class C {
    m(): number {
        return 2;
    }
}

export const arrow = (x: number) => x + 1;
"#;
        let a = TypeScriptAdapter::new();
        let syms = a.parse_symbols("x.ts", src).unwrap();
        let qnames: Vec<_> = syms.iter().map(|s| s.qname.clone()).collect();
        assert!(qnames.contains(&"x.top".to_string()), "got {qnames:?}");
        assert!(qnames.contains(&"x.C".to_string()), "got {qnames:?}");
        assert!(qnames.contains(&"x.C.m".to_string()), "got {qnames:?}");
        assert!(qnames.contains(&"x.arrow".to_string()), "got {qnames:?}");
        let top = syms.iter().find(|s| s.qname == "x.top").unwrap();
        assert_eq!(top.kind, SymbolKind::Function);
        let m = syms.iter().find(|s| s.qname == "x.C.m").unwrap();
        assert_eq!(m.kind, SymbolKind::Method);
        let arrow = syms.iter().find(|s| s.qname == "x.arrow").unwrap();
        assert_eq!(arrow.kind, SymbolKind::Function);
    }

    #[test]
    #[allow(non_snake_case)]
    fn infers_fs_write_from_writeFileSync() {
        let body = r#"
function f() {
    fs.writeFileSync("/tmp/a", "hi");
}
"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::IoFsWrite));
    }

    #[test]
    fn infers_log_from_console_log() {
        let body = r#"
function f() {
    console.log("hi");
}
"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::Log));
    }

    #[test]
    fn empty_when_no_patterns() {
        let body = "function f(x) { return x + 1; }";
        assert!(infer_effects_from_body(body).is_empty());
    }

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
function helper(x: number): number {
    return x;
}

function caller(): number {
    return helper(1);
}

class C {
    init(): number {
        return helper(2);
    }

    m(): number {
        return this.init();
    }
}
"#;
        let a = TypeScriptAdapter::new();
        let syms = a.parse_symbols("m.ts", src).unwrap();
        let ws = workspace_with(&["m.helper", "m.caller", "m.C", "m.C.init", "m.C.m"]);
        let edges = a.extract_call_edges("m.ts", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("m.caller".to_string(), "m.helper".to_string())),
            "missing caller -> helper; got {pairs:?}",
        );
        assert!(
            pairs.contains(&("m.C.init".to_string(), "m.helper".to_string())),
            "missing C.init -> helper; got {pairs:?}",
        );
        assert!(
            pairs.contains(&("m.C.m".to_string(), "m.C.init".to_string())),
            "missing C.m -> C.init; got {pairs:?}",
        );
    }

    #[test]
    fn extracts_cross_module_edges_via_named_import() {
        let src = r#"
import { y } from './other';

function x() {
    y();
}
"#;
        let a = TypeScriptAdapter::new();
        let syms = a.parse_symbols("caller.ts", src).unwrap();
        let ws = workspace_with(&["other.y", "caller.x"]);
        let edges = a.extract_call_edges("caller.ts", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("caller.x".to_string(), "other.y".to_string())),
            "missing x -> other.y; got {pairs:?}",
        );
    }

    #[test]
    fn extracts_cross_module_edges_via_namespace_import() {
        let src = r#"
import * as logger from './logger';

function foo() {
    logger.writeLog("hi");
}
"#;
        let a = TypeScriptAdapter::new();
        let syms = a.parse_symbols("caller.ts", src).unwrap();
        let ws = workspace_with(&["logger.writeLog", "caller.foo"]);
        let edges = a.extract_call_edges("caller.ts", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(
            pairs.contains(&("caller.foo".to_string(), "logger.writeLog".to_string())),
            "missing foo -> logger.writeLog; got {pairs:?}",
        );
    }

    #[test]
    fn drops_unknown_imports() {
        let src = r#"
import { thing } from 'stdlib-mod';

function foo() {
    thing();
}
"#;
        let a = TypeScriptAdapter::new();
        let syms = a.parse_symbols("caller.ts", src).unwrap();
        let ws = workspace_with(&["caller.foo"]);
        let edges = a.extract_call_edges("caller.ts", src, &syms, &ws);
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.caller_qname.clone(), e.callee_qname.clone()))
            .collect();
        assert!(pairs.is_empty(), "expected no edges; got {pairs:?}");
    }

    #[test]
    fn infers_throw() {
        let body = r#"
function f() {
    throw new Error("bad");
}
"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::Throw));
    }

    #[test]
    fn resolve_relative_specifier() {
        // './other' imported from 'src/a.ts' -> 'src.other'
        assert_eq!(resolve_module_specifier("./other", "src/a.ts"), "src.other");
        // '../x' imported from 'src/a/b.ts' -> 'src.x'
        assert_eq!(resolve_module_specifier("../x", "src/a/b.ts"), "src.x");
        // bare 'react' stays 'react'
        assert_eq!(resolve_module_specifier("react", "src/a.ts"), "react");
    }
}
