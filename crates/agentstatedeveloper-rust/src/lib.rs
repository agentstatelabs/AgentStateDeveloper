//! Rust language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-rust`. Parses top-level functions, methods (inside
//! `impl` blocks), structs, enums, and traits, and runs a substring-based
//! effect inference pass.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Node, Parser};

/// Rust language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustAdapter;

impl RustAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for RustAdapter {
    fn language(&self) -> &str {
        "rust"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| AsdError::Parse(format!("failed to set rust language: {e}")))?;

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
/// Falls back to the full path for crates without that convention.
///
/// Examples:
/// - `src/engine.rs`            → `engine.rs`
/// - `crates/foo/src/lib.rs`    → `lib.rs`
/// - `main.rs`                  → `main.rs`  (no `src` segment, unchanged)
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
/// `src/engine.rs`              -> `engine`
/// `crates/mylib/src/lib.rs`    -> `mylib.lib`   (crate-name disambiguates)
/// `crates/mylib/src/main.rs`   -> `mylib.main`
/// `./foo/bar.rs`               -> `foo.bar`  (no src segment, fallback)
/// `main.rs`                    -> `main`
/// `lib.rs`                     -> `lib`
fn module_qname_prefix(file: &str) -> String {
    let mut s = file;
    if let Some(stripped) = s.strip_prefix("./") {
        s = stripped;
    }
    let s = s.strip_suffix(".rs").unwrap_or(s);
    let after_src = strip_src_prefix(s);
    // For lib.rs / main.rs, the module name alone is not unique across crates.
    // Prepend the crate directory name (the segment before `src/`).
    if after_src == "lib" || after_src == "main" {
        if let Some(crate_name) = crate_name_from_path(s) {
            return format!("{}.{}", crate_name.replace('-', "_"), after_src);
        }
    }
    after_src.replace('\\', "/").replace('/', ".")
}

/// Extract the crate name from a path like `crates/my-crate/src/lib.rs`
/// by finding the segment immediately before a `src` component.
fn crate_name_from_path(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, &part) in parts.iter().enumerate() {
        if part == "src" && i > 0 {
            return Some(parts[i - 1]);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Impl,
    Trait,
    Mod,
}

/// Walk the tree. We enumerate:
/// - `function_item`          → Function (or Method when inside an impl/trait scope)
/// - `impl_item`              → emit the implementing type as Class; recurse into body
/// - `struct_item`            → Class
/// - `enum_item`              → Class
/// - `trait_item`             → Class; recurse to capture default methods
/// - `mod_item` (inline only) → push module name onto scope; recurse
fn walk(
    node: Node<'_>,
    src: &[u8],
    module_prefix: &str,
    scope: &[(String, ScopeKind)],
    out: &mut Vec<ParsedSymbol>,
) {
    match node.kind() {
        "function_item" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            let symbol_kind = if scope
                .last()
                .map(|s| s.1 == ScopeKind::Impl || s.1 == ScopeKind::Trait)
                .unwrap_or(false)
            {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let signature = extract_fn_signature(node, src, &name);
            out.push(make_parsed_symbol(node, src, qname, symbol_kind, signature));
            // Don't recurse into function bodies — nested fns are too noisy.
        }
        "impl_item" => {
            // Extract the name of the type being implemented.
            let type_name = impl_type_name(node, src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &type_name);
            // Emit the impl type as a Class symbol so the index has an entry for it.
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Class,
                Some(type_name.clone()),
            ));
            // Recurse into the impl body with the type on scope.
            let mut new_scope = scope.to_vec();
            new_scope.push((type_name, ScopeKind::Impl));
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk(child, src, module_prefix, &new_scope, out);
                }
            }
        }
        "struct_item" | "enum_item" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            let signature = Some(format!(
                "{} {name}",
                if node.kind() == "struct_item" {
                    "struct"
                } else {
                    "enum"
                }
            ));
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Class,
                signature,
            ));
            // Don't recurse — struct fields aren't callable symbols.
        }
        "trait_item" => {
            let name = node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
            let qname = build_qname(module_prefix, scope, &name);
            out.push(make_parsed_symbol(
                node,
                src,
                qname,
                SymbolKind::Class,
                Some(format!("trait {name}")),
            ));
            // Recurse to capture default method implementations.
            let mut new_scope = scope.to_vec();
            new_scope.push((name, ScopeKind::Trait));
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk(child, src, module_prefix, &new_scope, out);
                }
            }
        }
        "mod_item" => {
            // Only recurse into inline modules (`mod foo { ... }`); skip
            // module declarations (`mod foo;`) which have no body.
            if let Some(body) = node.child_by_field_name("body") {
                let mod_name =
                    node_field_text(node, "name", src).unwrap_or_else(|| "<anon>".to_string());
                let mut new_scope = scope.to_vec();
                new_scope.push((mod_name, ScopeKind::Mod));
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    walk(child, src, module_prefix, &new_scope, out);
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk(child, src, module_prefix, scope, out);
            }
        }
    }
}

/// Extract the implementing type name from an `impl_item` node.
///
/// Handles `impl Foo`, `impl Foo<T>`, `impl Trait for Foo`, and generic
/// `impl<T> Trait for Foo<T>` — we always return the *self type* (after
/// `for` when present, otherwise the `type` field).
fn impl_type_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    let type_node = node.child_by_field_name("type")?;
    base_type_name(type_node, src)
}

/// Collapse a potentially generic type node to its base identifier.
///
/// `Foo` → `"Foo"`, `Foo<T>` → `"Foo"`, `(dyn Trait)` → raw text.
fn base_type_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => node_text(node, src),
        "generic_type" => {
            // `generic_type` has a `type` field pointing at the base type.
            let inner = node.child_by_field_name("type")?;
            base_type_name(inner, src)
        }
        _ => node_text(node, src),
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

fn extract_fn_signature(node: Node<'_>, src: &[u8], name: &str) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_text = node_text(params, src)?;
    let ret = node
        .child_by_field_name("return_type")
        .and_then(|n| node_text(n, src))
        .map(|s| format!(" -> {}", s.trim_start_matches("-> ").trim()))
        .unwrap_or_default();
    Some(format!("fn {name}{params_text}{ret}"))
}

// -----------------------------------------------------------------------------
// Effect inference
// -----------------------------------------------------------------------------

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // File system — std::fs, tokio::fs, std::io, File::open / File::create
    let fs_read_needles = [
        "fs::read(",
        "fs::read_to_string(",
        "fs::read_dir(",
        "File::open(",
        "OpenOptions::new(",
        "BufReader::new(",
    ];
    let fs_write_needles = [
        "fs::write(",
        "fs::create_dir",
        "fs::remove_file(",
        "fs::rename(",
        "File::create(",
        "BufWriter::new(",
        ".write_all(",
        ".write_bytes(",
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

    // Network — reqwest, hyper, ureq, surf, isahc, tonic (gRPC)
    let net_needles = [
        "reqwest::",
        "hyper::",
        "ureq::",
        "surf::",
        "isahc::",
        "tonic::",
        "TcpStream::connect(",
        "UdpSocket::bind(",
    ];
    let mut net_hosts: Vec<String> = Vec::new();
    let mut net_note: Option<String> = None;
    for needle in net_needles {
        if body.contains(needle) {
            if net_note.is_none() {
                net_note = first_matching_line(body, &[needle]);
            }
            // Try to find URL literals near the call.
            for call_site in find_calls(body, &format!("{needle}")) {
                let snippet = &body[call_site..call_site.saturating_add(200)];
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

    // Database — sqlx, diesel, rusqlite, sea-orm, tokio-postgres
    let db_read_needles = [
        "sqlx::query",
        ".fetch(",
        ".fetch_one(",
        ".fetch_all(",
        ".fetch_optional(",
    ];
    let db_write_needles = [
        "sqlx::query",
        ".execute(",
        "diesel::insert",
        "diesel::update",
        "diesel::delete",
    ];
    // For DB we do a simpler presence check — SQL direction is hard to infer
    // from Rust's query builder APIs without evaluating the query string.
    let db_needles = [
        "sqlx::",
        "diesel::",
        "rusqlite::",
        "Connection::open(",
        "tokio_postgres::",
        "sea_orm::",
        "rbatis::",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        // Emit both read and write conservatively unless we can see clear direction.
        let has_read = first_match_note(body, &db_read_needles).is_some();
        let has_write = first_match_note(body, &db_write_needles).is_some();
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

    // Process spawn — std::process::Command, tokio::process::Command
    let proc_needles = ["Command::new(", "std::process::", "process::exit("];
    if let Some(note) = first_match_note(body, &proc_needles) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Env read — std::env::var, env::args
    let env_needles = ["env::var(", "env::var_os(", "env::args(", "std::env::"];
    if let Some(note) = first_match_note(body, &env_needles) {
        let mut vars: Vec<String> = Vec::new();
        for call_site in find_calls(body, "env::var(") {
            let args = &body[call_site..];
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

    // Logging — println!, eprintln!, log::*, tracing::*, slog::*
    let log_needles = [
        "println!(",
        "eprintln!(",
        "print!(",
        "eprint!(",
        "log::debug!",
        "log::info!",
        "log::warn!",
        "log::error!",
        "tracing::debug!",
        "tracing::info!",
        "tracing::warn!",
        "tracing::error!",
        "tracing::trace!",
        "debug!(",
        "info!(",
        "warn!(",
        "error!(",
        "trace!(",
    ];
    if let Some(note) = first_match_note(body, &log_needles) {
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Sleep — thread::sleep, tokio::time::sleep, async_std::task::sleep
    let sleep_needles = [
        "thread::sleep(",
        "tokio::time::sleep(",
        "async_std::task::sleep(",
        "time::sleep(",
    ];
    if let Some(note) = first_match_note(body, &sleep_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Time read — std::time::Instant, SystemTime, chrono::Utc
    let time_read_needles = [
        "Instant::now()",
        "SystemTime::now()",
        "chrono::Utc::now()",
        "Utc::now()",
        "Local::now()",
        "time::OffsetDateTime::now_utc(",
        "OffsetDateTime::now_utc(",
    ];
    if let Some(note) = first_match_note(body, &time_read_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Random — rand crate
    let random_needles = [
        "rand::",
        "thread_rng()",
        "rng.gen",
        "Rng::gen",
        "random::<",
        "OsRng",
        "Uuid::new_v4(",
        "uuid::Uuid::new_v4(",
    ];
    if let Some(note) = first_match_note(body, &random_needles) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
            ..Default::default()
        });
    }

    // Throw — panic!, explicit unwrap (as a proxy for "may abort on error")
    if has_panic_or_unwrap(body) {
        let note = first_matching_line(body, &["panic!(", ".unwrap(", ".expect("]);
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note,
            ..Default::default()
        });
    }

    effects
}

fn has_panic_or_unwrap(body: &str) -> bool {
    body.contains("panic!(") || body.contains(".unwrap()") || body.contains(".expect(")
}

// -----------------------------------------------------------------------------
// Simple call-site helpers (needle-based, no paren matching needed for effects)
// -----------------------------------------------------------------------------

/// Return the byte offsets of every occurrence of `needle` in `body`.
fn find_calls(body: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(idx) = body[from..].find(needle) {
        out.push(from + idx + needle.len());
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
                .find(|c: char| c == '/' || c == '"' || c == '\'' || c == ')' || c.is_whitespace())
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

    // simple_name -> qname for intra-module resolution.
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
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let imports = parse_use_declarations(source, &mut parser);

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

/// Import binding: local name → qname prefix in ASD dotted form.
#[derive(Debug, Clone)]
struct UseBinding {
    qname_prefix: String,
}

/// Walk top-level `use_declaration` nodes and collect bindings.
///
/// Supports:
/// - `use foo::bar;`              → `bar` → `foo.bar`
/// - `use foo::bar as b;`         → `b`   → `foo.bar`
/// - `use foo::{Bar, baz as z};`  → `Bar` → `foo.Bar`, `z` → `foo.baz`
/// - `use foo::*;`                → skipped (can't statically enumerate)
fn parse_use_declarations(source: &str, parser: &mut Parser) -> HashMap<String, UseBinding> {
    let mut out = HashMap::new();
    let src_bytes = source.as_bytes();
    let tree = match parser.parse(src_bytes, None) {
        Some(t) => t,
        None => return out,
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "use_declaration" {
            if let Some(arg) = child.child_by_field_name("argument") {
                collect_use_tree(arg, src_bytes, "", &mut out);
            }
        }
    }
    out
}

/// Recursively collect from a `use_tree` node.
fn collect_use_tree(
    node: Node<'_>,
    src: &[u8],
    prefix: &str,
    out: &mut HashMap<String, UseBinding>,
) {
    match node.kind() {
        "use_wildcard" => { /* skip */ }
        "scoped_use_list" => {
            // `foo::{Bar, baz}` — the `path` child is the prefix, the `list` child contains items.
            let path = node
                .child_by_field_name("path")
                .and_then(|n| node_text(n, src))
                .map(|s| s.replace("::", "."))
                .unwrap_or_default();
            let new_prefix = if prefix.is_empty() {
                path
            } else if path.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}.{path}")
            };
            if let Some(list) = node.child_by_field_name("list") {
                let mut cursor = list.walk();
                for child in list.children(&mut cursor) {
                    collect_use_tree(child, src, &new_prefix, out);
                }
            }
        }
        "use_as_clause" => {
            // `foo as bar` — bind `bar` to the resolved qname of `foo`.
            let name = node
                .child_by_field_name("path")
                .and_then(|n| node_text(n, src))
                .map(|s| s.replace("::", "."))
                .unwrap_or_default();
            let alias = node
                .child_by_field_name("alias")
                .and_then(|n| node_text(n, src))
                .unwrap_or_default();
            if !alias.is_empty() && !name.is_empty() {
                let qname = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}.{name}")
                };
                out.insert(
                    alias,
                    UseBinding {
                        qname_prefix: qname,
                    },
                );
            }
        }
        "scoped_identifier" => {
            // `foo::bar` — bind `bar` to `foo.bar`.
            let text = node_text(node, src)
                .map(|s| s.replace("::", "."))
                .unwrap_or_default();
            let full = if prefix.is_empty() {
                text.clone()
            } else {
                format!("{prefix}.{text}")
            };
            let local = text.rsplit('.').next().unwrap_or(&text).to_string();
            if !local.is_empty() {
                out.insert(local, UseBinding { qname_prefix: full });
            }
        }
        "identifier" => {
            let name = node_text(node, src).unwrap_or_default();
            if !name.is_empty() && name != "self" && name != "super" && name != "crate" {
                let qname = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                out.insert(
                    name,
                    UseBinding {
                        qname_prefix: qname,
                    },
                );
            }
        }
        _ => {
            // Recurse into other container nodes (use_list, etc.).
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_tree(child, src, prefix, out);
            }
        }
    }
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
    imports: &HashMap<String, UseBinding>,
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
    imports: &HashMap<String, UseBinding>,
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
) -> Option<String> {
    match func.kind() {
        "identifier" => {
            let name = node_text(func, src)?;
            // Intra-module first.
            if let Some(q) = by_simple.get(&name) {
                return Some(q.clone());
            }
            // Import map.
            if let Some(binding) = imports.get(&name) {
                if workspace.contains(&binding.qname_prefix) {
                    return Some(binding.qname_prefix.clone());
                }
            }
            None
        }
        "field_expression" => {
            // `self.method()` or `obj.method()`
            let value = func.child_by_field_name("value")?;
            let field = func.child_by_field_name("field")?;
            let field_name = node_text(field, src)?;

            if value.kind() == "self" {
                let class = enclosing_type?;
                let candidate = format!("{class}.{field_name}");
                if known.contains(candidate.as_str()) {
                    return Some(candidate);
                }
                return None;
            }
            if value.kind() == "identifier" {
                let obj_name = node_text(value, src)?;
                // Intra-module type reference: `Foo::bar` -> `mod.Foo.bar`
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
                // Imported binding.
                if let Some(binding) = imports.get(&obj_name) {
                    let candidate = format!("{}.{field_name}", binding.qname_prefix);
                    if workspace.contains(&candidate) {
                        return Some(candidate);
                    }
                }
            }
            None
        }
        "scoped_identifier" => {
            // `Type::associated_fn()` or `module::function()`
            let text = node_text(func, src)?.replace("::", ".");
            // Direct workspace hit.
            if workspace.contains(&text) {
                return Some(text.clone());
            }
            // Try prefixing with module.
            if !module_prefix.is_empty() {
                let candidate = format!("{module_prefix}.{text}");
                if workspace.contains(&candidate) {
                    return Some(candidate);
                }
            }
            // Local simple name (last segment) in by_simple.
            let simple = text.rsplit('.').next()?;
            if let Some(q) = by_simple.get(simple) {
                return Some(q.clone());
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
    fn module_prefix_strips_rs_and_leading_dot_slash() {
        // src/ anchor — stable regardless of index root. Path segments before
        // a nested `src/` (e.g. the crate dir) are retained as qname prefix.
        assert_eq!(module_qname_prefix("src/engine.rs"), "engine");
        assert_eq!(module_qname_prefix("crates/mylib/src/lib.rs"), "mylib.lib");
        assert_eq!(
            module_qname_prefix("src/payments/charge.rs"),
            "payments.charge"
        );
        // no src segment — full relative path (fallback)
        assert_eq!(module_qname_prefix("./foo/bar.rs"), "foo.bar");
        assert_eq!(module_qname_prefix("lib.rs"), "lib");
        assert_eq!(module_qname_prefix("main.rs"), "main");
    }

    #[test]
    fn strip_src_prefix_variants() {
        assert_eq!(strip_src_prefix("src/engine.rs"), "engine.rs");
        assert_eq!(strip_src_prefix("crates/foo/src/lib.rs"), "lib.rs");
        assert_eq!(strip_src_prefix("main.rs"), "main.rs");
        assert_eq!(strip_src_prefix("nosrc/foo.rs"), "nosrc/foo.rs");
    }

    #[test]
    fn parses_function_method_struct_enum() {
        let src = r#"
pub struct Engine { }

impl Engine {
    pub fn open() -> Self { Engine {} }
    pub fn run(&self) { }
}

pub fn top_level() { }

pub enum Status { Ok, Err }
"#;
        let a = RustAdapter::new();
        let syms = a.parse_symbols("src/engine.rs", src).unwrap();
        let qnames: Vec<_> = syms.iter().map(|s| s.qname.clone()).collect();
        // src/ is stripped — qnames are anchored to the crate interior
        assert!(
            qnames.contains(&"engine.Engine".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"engine.Engine.open".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"engine.Engine.run".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"engine.top_level".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"engine.Status".to_string()),
            "got {qnames:?}"
        );
        let open = syms
            .iter()
            .find(|s| s.qname == "engine.Engine.open")
            .unwrap();
        assert_eq!(open.kind, SymbolKind::Method);
        let top = syms.iter().find(|s| s.qname == "engine.top_level").unwrap();
        assert_eq!(top.kind, SymbolKind::Function);
    }

    #[test]
    fn infers_fs_read_and_write() {
        let body = r#"
fn f() {
    let _ = File::open("/tmp/a").unwrap();
    fs::write("/tmp/b", b"hi").unwrap();
}
"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::IoFsRead), "cats: {cats:?}");
        assert!(cats.contains(&EffectCategory::IoFsWrite), "cats: {cats:?}");
    }

    #[test]
    fn infers_log_from_tracing() {
        let body = r#"fn f() { tracing::info!("hello"); }"#;
        let effects = infer_effects_from_body(body);
        let cats: Vec<_> = effects.iter().map(|e| e.effect.clone()).collect();
        assert!(cats.contains(&EffectCategory::Log), "cats: {cats:?}");
    }

    #[test]
    fn empty_when_no_patterns() {
        let body = "fn f(x: i32) -> i32 { x + 1 }";
        assert!(infer_effects_from_body(body).is_empty());
    }

    #[test]
    fn parses_trait_with_default_method() {
        let src = r#"
pub trait Store {
    fn save(&self);
    fn load(&self) -> String { String::new() }
}
"#;
        let a = RustAdapter::new();
        let syms = a.parse_symbols("src/store.rs", src).unwrap();
        let qnames: Vec<_> = syms.iter().map(|s| s.qname.clone()).collect();
        // src/ is stripped
        assert!(
            qnames.contains(&"store.Store".to_string()),
            "got {qnames:?}"
        );
        assert!(
            qnames.contains(&"store.Store.load".to_string()),
            "got {qnames:?}"
        );
    }
}
