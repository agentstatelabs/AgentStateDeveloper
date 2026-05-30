//! Ruby language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-ruby`. Parses modules, classes, instance methods,
//! singleton methods (def self.x), then runs substring-based effect inference.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Node, Parser};

/// Ruby language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct RubyAdapter;

impl RubyAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for RubyAdapter {
    fn language(&self) -> &str {
        "ruby"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["rb"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_ruby::LANGUAGE.into())
            .map_err(|e| AsdError::Parse(format!("failed to set ruby language: {e}")))?;

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

/// Walk path components and return the tail after the first `lib` segment.
/// Falls back to the full path for non-gem layouts (e.g. Rails `app/`).
///
/// Examples:
/// - `lib/myapp/parser.rb`      → `myapp/parser.rb`
/// - `gems/foo/lib/foo/bar.rb`  → `foo/bar.rb`
/// - `app/models/user.rb`       → `app/models/user.rb`  (no `lib`, unchanged)
fn strip_lib_prefix(path: &str) -> &str {
    let mut offset = 0usize;
    for part in path.split('/') {
        if part == "lib" {
            let after = offset + part.len() + 1;
            if after < path.len() {
                return &path[after..];
            }
        }
        offset += part.len() + 1;
    }
    path
}

/// `lib/myapp/parser.rb`   → `myapp.parser`  (gem convention, anchored at lib/)
/// `app/models/payment.rb` → `app.models.payment`  (Rails — no lib/, fallback)
fn file_qname_prefix(file: &str) -> String {
    let s = file.strip_prefix("./").unwrap_or(file);
    let s = s.strip_suffix(".rb").unwrap_or(s);
    let s = strip_lib_prefix(s);
    s.replace('/', ".")
}

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

/// Extract the Ruby method signature: the first line of the `def` declaration.
/// Ruby uses `end` as the body terminator (not `{`), so we take everything
/// up to the first newline.
fn extract_ruby_sig(node: Node<'_>, src: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).ok()?;
    let first_line = text.lines().next()?.trim().to_string();
    if first_line.is_empty() {
        None
    } else {
        Some(first_line)
    }
}

// -----------------------------------------------------------------------------
// Symbol walking
// -----------------------------------------------------------------------------

fn walk(node: Node<'_>, src: &[u8], scope: &str, out: &mut Vec<ParsedSymbol>) {
    match node.kind() {
        "program" => {
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, out);
            }
        }
        "module" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            out.push(make_symbol(node, src, qname.clone(), SymbolKind::Module));
            // Walk body
            if let Some(body) = child_by_field(node, "body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "class" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            out.push(make_symbol(node, src, qname.clone(), SymbolKind::Class));
            if let Some(body) = child_by_field(node, "body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "method" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let sig = extract_ruby_sig(node, src);
            out.push(make_symbol_sig(node, src, qname, SymbolKind::Method, sig));
        }
        "singleton_method" => {
            // def self.method_name — treat as a class-level Function
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let sig = extract_ruby_sig(node, src);
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

fn extract_first_string_arg(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
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

fn infer_effects_from_body(body: &str) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();

    // FS Read
    let fs_read_needles = [
        "File.read(",
        "File.readlines(",
        "File.open(",
        "IO.read(",
        "IO.readlines(",
        "CSV.read(",
        "CSV.foreach(",
        "File.foreach(",
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
        "File.write(",
        "File.open(", // also write mode — covered by read; separate write markers:
        "IO.write(",
        "FileUtils.cp(",
        "FileUtils.mv(",
        "FileUtils.mkdir",
        "FileUtils.rm",
        "File.delete(",
        "File.rename(",
    ];
    // Only fire write if we see a write-mode marker not already covered by read
    let write_markers = [
        "IO.write(",
        "FileUtils.cp(",
        "FileUtils.mv(",
        "FileUtils.mkdir",
        "FileUtils.rm",
        "File.delete(",
        "File.rename(",
        "\"w\"",
        "\"w+\"",
        "\"a\"",
        "\"a+\"",
        ":write",
        ", 'w'",
        ", 'a'",
    ];
    let has_write_marker = write_markers.iter().any(|m| body.contains(m));
    let _ = fs_write_needles; // suppress unused warning; logic uses write_markers directly
    if has_write_marker {
        if let Some(note) = first_match_note(body, &write_markers) {
            effects.push(Effect {
                effect: EffectCategory::IoFsWrite,
                qualifiers: serde_json::Value::Null,
                note: Some(note),
                ..Default::default()
            });
        }
    }

    // Network
    let net_needles = [
        "Net::HTTP",
        "Net::HTTP.get(",
        "Net::HTTP.post(",
        "Faraday.new(",
        "Faraday.get(",
        "Faraday.post(",
        "HTTParty.get(",
        "HTTParty.post(",
        "RestClient.get(",
        "RestClient.post(",
        "Typhoeus.get(",
        "open-uri",
        "open(\"http",
        "open('http",
        "URI.open(",
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
        ".where(",
        ".find(",
        ".find_by(",
        ".first",
        ".all",
        ".create(",
        ".save(",
        ".update(",
        ".destroy(",
        ".delete(",
        "ActiveRecord::",
        "Sequel::",
        ".execute(",
        ".query(",
        "DB[",
        "connection.exec(",
        "PG.connect(",
        "Mysql2::Client.new(",
        "SQLite3::Database.new(",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        let write_markers = [
            ".create(",
            ".save(",
            ".update(",
            ".destroy(",
            ".delete(",
            ".execute(",
            ".insert(",
        ];
        let read_markers = [
            ".where(",
            ".find(",
            ".find_by(",
            ".first",
            ".all",
            ".query(",
            ".exec(",
            "SELECT",
        ];
        let has_write = write_markers.iter().any(|m| body.contains(m));
        let has_read = read_markers.iter().any(|m| body.contains(m));
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
    let proc_needles = [
        "system(",
        "exec(",
        "spawn(",
        "IO.popen(",
        "Open3.",
        "%x{",
        "Kernel.system(",
        "Kernel.exec(",
    ];
    // Avoid false-positive on common method calls like "exec" in ActiveRecord
    let has_proc = proc_needles.iter().any(|n| body.contains(n));
    if has_proc {
        if let Some(note) = first_match_note(body, &proc_needles) {
            effects.push(Effect {
                effect: EffectCategory::ProcSpawn,
                qualifiers: serde_json::Value::Null,
                note: Some(note),
                ..Default::default()
            });
        }
    }

    // Env read
    let env_needles = ["ENV[", "ENV.fetch(", "ENV.key?(", "ENV.values"];
    if let Some(note) = first_match_note(body, &env_needles) {
        let mut vars: Vec<String> = Vec::new();
        for off in find_occurrences(body, "ENV[") {
            let args = &body[off + "ENV[".len()..];
            if let Some(v) = extract_first_string_arg(args) {
                if !vars.contains(&v) {
                    vars.push(v);
                }
            }
        }
        for off in find_occurrences(body, "ENV.fetch(") {
            let args = &body[off + "ENV.fetch(".len()..];
            if let Some(v) = extract_first_string_arg(args) {
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
        "puts ",
        "print ",
        "p ",
        "pp ",
        "Rails.logger.",
        "logger.",
        "Logger.new(",
        "Sentry.",
        "Bugsnag.",
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
    let sleep_needles = ["sleep(", "sleep ", "Kernel.sleep("];
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
        "Time.now",
        "Time.current",
        "Time.at(",
        "DateTime.now",
        "DateTime.current",
        "Date.today",
        "Time.zone.now",
        "Process.clock_gettime(",
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
        "rand(",
        "rand ",
        "Random.rand(",
        "Random.new(",
        "SecureRandom.",
        "SecureRandom.uuid",
        "SecureRandom.hex",
        ".sample",
        ".shuffle",
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
    let throw_needles = ["raise ", "raise(", "fail ", "fail("];
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

/// Require binding: local constant name → dotted qname prefix.
#[derive(Debug, Clone)]
struct RequireBinding {
    /// Constant name as used in code, e.g. `PaymentsClient`.
    constant: String,
    /// Dotted qname prefix for the required file, e.g. `payments.client`.
    qname_prefix: String,
}

fn parse_requires(source: &str) -> Vec<RequireBinding> {
    let mut out = Vec::new();
    for line in source.lines() {
        let t = line.trim();
        // require 'payments/client' or require_relative '../payments/client'
        for kw in ["require ", "require_relative "] {
            if let Some(rest) = t.strip_prefix(kw) {
                let path = rest
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim_start_matches("../");
                // Convert path to dotted prefix and derive a constant name
                let prefix = path.replace('/', ".");
                // Guess the constant: last segment, CamelCase
                let constant = path
                    .rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .split('_')
                    .map(|w| {
                        let mut chars = w.chars();
                        match chars.next() {
                            None => String::new(),
                            Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                        }
                    })
                    .collect::<String>();
                if !constant.is_empty() {
                    out.push(RequireBinding {
                        constant,
                        qname_prefix: prefix,
                    });
                }
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

fn collect_calls(
    node: Node<'_>,
    src: &[u8],
    sym: &ParsedSymbol,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    requires: &[RequireBinding],
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    edges: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call" {
        // receiver.method or bare method
        let receiver_node = child_by_field(node, "receiver");
        let method_node = child_by_field(node, "method");

        let method_text = method_node.map(|n| node_text(n, src)).unwrap_or("");
        let receiver_text = receiver_node.map(|n| node_text(n, src)).unwrap_or("");

        if !method_text.is_empty() {
            let callee = if receiver_text.is_empty() {
                // Bare call
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
                // Qualified: check requires for the receiver constant
                let req = requires.iter().find(|r| r.constant == receiver_text);
                if let Some(binding) = req {
                    let q = format!("{}.{}", binding.qname_prefix, method_text);
                    if known.contains(q.as_str()) || workspace.contains(&q) {
                        Some(q)
                    } else {
                        None
                    }
                } else {
                    // Try known workspace symbols
                    let q = format!("{}.{}", receiver_text, method_text);
                    if known.contains(q.as_str()) || workspace.contains(&q) {
                        Some(q)
                    } else {
                        None
                    }
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

    for i in 0..node.child_count() {
        collect_calls(
            node.child(i).unwrap(),
            src,
            sym,
            by_simple,
            known,
            requires,
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
        .set_language(&tree_sitter_ruby::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let requires = parse_requires(source);
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
        collect_calls(
            tree.root_node(),
            src_bytes,
            sym,
            &by_simple,
            &known,
            &requires,
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

    fn adapter() -> RubyAdapter {
        RubyAdapter::new()
    }

    #[test]
    fn parses_module_class_method_singleton() {
        let src = r#"
module Payments
  class PaymentService
    def charge(customer_id, amount)
      puts "charging #{amount}"
    end

    def self.create_client
      new
    end
  end
end
"#;
        let syms = adapter()
            .parse_symbols("payments/payment_service.rb", src)
            .unwrap();
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(
            qnames.contains(&"payments.payment_service.Payments"),
            "{qnames:?}"
        );
        assert!(
            qnames.contains(&"payments.payment_service.Payments.PaymentService"),
            "{qnames:?}"
        );
        assert!(
            qnames.contains(&"payments.payment_service.Payments.PaymentService.charge"),
            "{qnames:?}"
        );
        assert!(
            qnames.contains(&"payments.payment_service.Payments.PaymentService.create_client"),
            "{qnames:?}"
        );
    }

    #[test]
    fn file_prefix_strips_rb_extension() {
        // lib/ anchor — stable for gem convention
        assert_eq!(file_qname_prefix("lib/myapp/parser.rb"), "myapp.parser");
        assert_eq!(file_qname_prefix("gems/foo/lib/foo/bar.rb"), "foo.bar");
        assert_eq!(file_qname_prefix("./lib/helpers.rb"), "helpers");
        // no lib segment — full relative path (Rails app/ or bare files)
        assert_eq!(file_qname_prefix("app/models/user.rb"), "app.models.user");
        assert_eq!(file_qname_prefix("charge.rb"), "charge");
    }

    #[test]
    fn strip_lib_prefix_variants() {
        assert_eq!(strip_lib_prefix("lib/foo/bar.rb"), "foo/bar.rb");
        assert_eq!(strip_lib_prefix("gems/foo/lib/foo.rb"), "foo.rb");
        assert_eq!(strip_lib_prefix("app/models/user.rb"), "app/models/user.rb");
    }

    #[test]
    fn infers_fs_read_and_net_out() {
        let src = r#"
class Fetcher
  def fetch
    data = File.read("/tmp/config.yml")
    response = HTTParty.get("https://api.example.com/v1/data")
    data
  end
end
"#;
        let syms = adapter().parse_symbols("fetcher.rb", src).unwrap();
        let fetch = syms.iter().find(|s| s.qname.ends_with(".fetch")).unwrap();
        let effs = adapter().infer_effects("", fetch);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::IoFsRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::IoNetOut), "{cats:?}");
    }

    #[test]
    fn infers_db_and_env() {
        let src = r#"
class UserService
  def find_user(id)
    db_url = ENV["DATABASE_URL"]
    User.find(id)
  end

  def create_user(attrs)
    User.create(attrs)
  end
end
"#;
        let syms = adapter().parse_symbols("user_service.rb", src).unwrap();
        let find = syms
            .iter()
            .find(|s| s.qname.ends_with(".find_user"))
            .unwrap();
        let create = syms
            .iter()
            .find(|s| s.qname.ends_with(".create_user"))
            .unwrap();
        let find_effs = adapter().infer_effects("", find);
        let create_effs = adapter().infer_effects("", create);
        assert!(
            find_effs
                .iter()
                .any(|e| e.effect == EffectCategory::IoDbRead)
        );
        assert!(
            find_effs
                .iter()
                .any(|e| e.effect == EffectCategory::EnvRead)
        );
        assert!(
            create_effs
                .iter()
                .any(|e| e.effect == EffectCategory::IoDbWrite)
        );

        // Check env var extraction
        let env_eff = find_effs
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
            assert!(vars.contains(&"DATABASE_URL"), "{vars:?}");
        }
    }

    #[test]
    fn empty_when_no_patterns() {
        let src = r#"
class MathHelper
  def add(a, b)
    a + b
  end
end
"#;
        let syms = adapter().parse_symbols("math.rb", src).unwrap();
        let add = syms.iter().find(|s| s.qname.ends_with(".add")).unwrap();
        let effs = adapter().infer_effects("", add);
        assert!(effs.is_empty(), "{effs:?}");
    }

    #[test]
    fn extracts_intra_class_call_edges() {
        let src = r#"
class OrderService
  def place_order(order)
    charge(order.customer_id, order.total)
  end

  def charge(customer_id, amount)
    # process
  end
end
"#;
        let ws = WorkspaceSymbols {
            qnames: HashSet::new(),
            kinds: HashMap::new(),
            ..Default::default()
        };
        let syms = adapter().parse_symbols("order_service.rb", src).unwrap();
        let edges = adapter().extract_call_edges("order_service.rb", src, &syms, &ws);
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".place_order") && e.callee_qname.ends_with(".charge")
        });
        assert!(found, "expected intra-class edge; got: {edges:?}");
    }
}
