//! Swift language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-swift`. Parses classes, structs, enums, protocols,
//! extensions, and functions/methods, then runs substring-based effect inference.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Language, Node, Parser};

/// Bridge tree-sitter-swift 0.5.0's older Language type to our workspace 0.24 Language.
fn swift_language() -> Language {
    let old = tree_sitter_swift::language();
    // SAFETY: both types are single-pointer wrappers over the same C struct.
    unsafe { std::mem::transmute(old) }
}

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
            .set_language(&swift_language())
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

/// `Sources/Payments/ChargeService.swift` → `Sources.Payments.ChargeService`
fn file_qname_prefix(file: &str) -> String {
    let s = file.strip_prefix("./").unwrap_or(file);
    let s = s.strip_suffix(".swift").unwrap_or(s);
    s.replace('/', ".")
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

fn make_symbol(node: Node<'_>, src: &[u8], qname: String, kind: SymbolKind) -> ParsedSymbol {
    ParsedSymbol {
        qname,
        kind,
        start_line: node.start_position().row as u32 + 1,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32 + 1,
        end_col: node.end_position().column as u32,
        body: node_text(node, src).to_string(),
        signature: None,
    }
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
                out.push(make_symbol(node, src, qname.clone(), SymbolKind::Class));
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
            out.push(make_symbol(node, src, qname.clone(), SymbolKind::Class));
            if let Some(body) = child_by_field(node, "body")
                .or_else(|| find_child_by_kind(node, "protocol_body"))
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
            out.push(make_symbol(node, src, qname, kind));
        }
        "init_declaration" => {
            let qname = join_qname(scope, "init");
            out.push(make_symbol(node, src, qname, SymbolKind::Function));
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
            });
        }
        if has_write || (!has_read && !has_write) {
            effects.push(Effect {
                effect: EffectCategory::IoDbWrite,
                qualifiers: serde_json::Value::Null,
                note: Some(note),
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
    ];
    if let Some(note) = first_match_note(body, &time_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeRead,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Random
    let rand_needles = [
        ".random(",
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
        });
    }

    // Throw
    let throw_needles = ["throw ", "fatalError(", "preconditionFailure(", "assertionFailure("];
    if let Some(note) = first_match_note(body, &throw_needles) {
        effects.push(Effect {
            effect: EffectCategory::Throw,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    effects
}

// -----------------------------------------------------------------------------
// Call-edge extraction
// -----------------------------------------------------------------------------

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
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    edges: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call_expression" {
        if let Some(func_node) = node.child(0) {
            let (receiver, method) = if func_node.kind() == "navigation_expression"
                || func_node.kind() == "member_expression"
            {
                let recv = func_node.child(0).map(|n| node_text(n, src)).unwrap_or("");
                let method = child_by_field(func_node, "name")
                    .map(|n| node_text(n, src))
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
                            by_simple.get(method).cloned()
                        }
                    } else {
                        by_simple.get(method).cloned()
                    }
                } else {
                    let q = format!("{}.{}", receiver, method);
                    if known.contains(q.as_str()) || workspace.contains(&q) {
                        Some(q)
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
    if parser.set_language(&swift_language()).is_err() {
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
        let enclosing_type = enclosing_type_qname(&sym.qname, &known);
        collect_calls(
            tree.root_node(),
            src_bytes,
            sym,
            &by_simple,
            &known,
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
        let syms = adapter().parse_symbols("Sources/Payments/PaymentService.swift", src).unwrap();
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(qnames.iter().any(|q| q.ends_with("PaymentService")), "{qnames:?}");
        assert!(qnames.iter().any(|q| q.ends_with("PaymentService.charge")), "{qnames:?}");
        assert!(qnames.iter().any(|q| q.ends_with("Receipt")), "{qnames:?}");
        assert!(qnames.iter().any(|q| q.ends_with("Gateway")), "{qnames:?}");
        assert!(qnames.iter().any(|q| q.ends_with("Currency")), "{qnames:?}");
    }

    #[test]
    fn file_prefix_strips_swift_extension() {
        assert_eq!(file_qname_prefix("Sources/Payments/ChargeService.swift"), "Sources.Payments.ChargeService");
        assert_eq!(file_qname_prefix("./App/Models/User.swift"), "App.Models.User");
        assert_eq!(file_qname_prefix("main.swift"), "main");
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
        let ws = WorkspaceSymbols {
            qnames: HashSet::new(),
            kinds: HashMap::new(),
        };
        let syms = adapter().parse_symbols("OrderService.swift", src).unwrap();
        let edges = adapter().extract_call_edges("OrderService.swift", src, &syms, &ws);
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".placeOrder") && e.callee_qname.ends_with(".charge")
        });
        assert!(found, "expected intra-class edge; got: {edges:?}");
    }
}
