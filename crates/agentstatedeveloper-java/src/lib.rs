//! Java language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-java`. Parses top-level classes, interfaces, enums,
//! and their methods/constructors, then runs substring-based effect inference.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Node, Parser};

/// Java language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct JavaAdapter;

impl JavaAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for JavaAdapter {
    fn language(&self) -> &str {
        "java"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["java"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .map_err(|e| AsdError::Parse(format!("failed to set java language: {e}")))?;

        let src_bytes = source.as_bytes();
        let tree = parser
            .parse(src_bytes, None)
            .ok_or_else(|| AsdError::Parse(format!("failed to parse {file}")))?;

        let pkg_prefix = package_prefix(source);
        let root = tree.root_node();
        let mut out = Vec::new();
        walk(root, src_bytes, &pkg_prefix, None, &mut out);
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

/// Extract the package declaration as a dotted prefix, e.g. `com.example.payments`.
fn package_prefix(source: &str) -> String {
    for line in source.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("package ") {
            let pkg = rest.trim_end_matches(';').trim();
            if !pkg.is_empty() {
                return pkg.to_string();
            }
        }
    }
    String::new()
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

// -----------------------------------------------------------------------------
// Symbol walking
// -----------------------------------------------------------------------------

fn walk(
    node: Node<'_>,
    src: &[u8],
    scope: &str,
    enclosing_type: Option<&str>,
    out: &mut Vec<ParsedSymbol>,
) {
    match node.kind() {
        "program" => {
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, None, out);
            }
        }
        "class_declaration" | "interface_declaration" | "enum_declaration"
        | "record_declaration" | "annotation_type_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let kind = SymbolKind::Class;
            out.push(ParsedSymbol {
                qname: qname.clone(),
                kind,
                start_line: node.start_position().row as u32 + 1,
                start_col: node.start_position().column as u32,
                end_line: node.end_position().row as u32 + 1,
                end_col: node.end_position().column as u32,
                body: node_text(node, src).to_string(),
                signature: None,
            });
            // Walk body for nested members
            if let Some(body) = child_by_field(node, "body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, Some(&qname), out);
                }
            }
        }
        "method_declaration" | "constructor_declaration" => {
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let kind = if node.kind() == "constructor_declaration" {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            out.push(ParsedSymbol {
                qname,
                kind,
                start_line: node.start_position().row as u32 + 1,
                start_col: node.start_position().column as u32,
                end_line: node.end_position().row as u32 + 1,
                end_col: node.end_position().column as u32,
                body: node_text(node, src).to_string(),
                signature: extract_sig_before_brace(node, src),
            });
            // Don't recurse into method bodies for nested classes — Java allows
            // local class declarations but they're rare; skip for simplicity.
        }
        "enum_constant" => {
            // Enum constants are Class-level symbols
            let name = child_by_field(node, "name")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            out.push(ParsedSymbol {
                qname,
                kind: SymbolKind::Variable,
                start_line: node.start_position().row as u32 + 1,
                start_col: node.start_position().column as u32,
                end_line: node.end_position().row as u32 + 1,
                end_col: node.end_position().column as u32,
                body: node_text(node, src).to_string(),
                signature: None,
            });
        }
        _ => {
            // Walk children for anything we don't recognise
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, enclosing_type, out);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Signature extraction
// -----------------------------------------------------------------------------

/// Extract the method/constructor signature: text from declaration start up to
/// (but not including) the opening `{` of the body. Tracks `()` and `[]` depth.
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
                    if bytes[i] == b'\\' { i += 2; continue; }
                    if bytes[i] == b'"' { break; }
                    i += 1;
                }
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => { if depth > 0 { depth -= 1; } }
            b'{' if depth == 0 => { sig_end = i; break; }
            _ => {}
        }
        i += 1;
    }
    let sig = text[..sig_end].trim().to_string();
    if sig.is_empty() { None } else { Some(sig) }
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
        "new FileInputStream(",
        "new FileReader(",
        "Files.readAllBytes(",
        "Files.readAllLines(",
        "Files.newBufferedReader(",
        "Files.lines(",
        "new BufferedReader(",
        "new Scanner(new File(",
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
        "new FileOutputStream(",
        "new FileWriter(",
        "Files.write(",
        "Files.newBufferedWriter(",
        "Files.createFile(",
        "Files.createDirectory(",
        "new PrintWriter(new File(",
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
        "new URL(",
        "HttpURLConnection",
        "HttpClient.newHttpClient(",
        "HttpClient.newBuilder(",
        "OkHttpClient",
        "new OkHttpClient(",
        "Retrofit.Builder(",
        "RestTemplate(",
        "WebClient.",
        "CloseableHttpClient",
        "HttpGet(",
        "HttpPost(",
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
        "DriverManager.getConnection(",
        "dataSource.getConnection(",
        ".prepareStatement(",
        ".createStatement(",
        ".executeQuery(",
        ".executeUpdate(",
        "entityManager.",
        "session.save(",
        "session.get(",
        "repository.find",
        "repository.save",
        ".createQuery(",
        "jdbcTemplate.",
        "namedParameterJdbcTemplate.",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        let has_write = body.contains(".executeUpdate(")
            || body.contains("session.save(")
            || body.contains("repository.save")
            || body.contains(".persist(")
            || body.contains(".merge(")
            || body.contains(".delete(")
            || body.contains(".remove(");
        let has_read = body.contains(".executeQuery(")
            || body.contains("session.get(")
            || body.contains("repository.find")
            || body.contains(".getSingleResult(")
            || body.contains(".getResultList(");
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
    let proc_needles = [
        "Runtime.getRuntime().exec(",
        "new ProcessBuilder(",
        "ProcessBuilder(",
    ];
    if let Some(note) = first_match_note(body, &proc_needles) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Env read
    let env_needles = ["System.getenv(", "System.getProperties("];
    if let Some(note) = first_match_note(body, &env_needles) {
        let mut vars: Vec<String> = Vec::new();
        for off in find_occurrences(body, "System.getenv(") {
            let args = &body[off + "System.getenv(".len()..];
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
        "System.out.println(",
        "System.err.println(",
        "logger.info(",
        "logger.debug(",
        "logger.warn(",
        "logger.error(",
        "log.info(",
        "log.debug(",
        "log.warn(",
        "log.error(",
        "Logger.getLogger(",
        "LoggerFactory.getLogger(",
    ];
    if let Some(note) = first_match_note(body, &log_needles) {
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Time sleep
    let sleep_needles = ["Thread.sleep(", "TimeUnit.", ".sleep("];
    if let Some(note) = first_match_note(body, &sleep_needles) {
        effects.push(Effect {
            effect: EffectCategory::TimeSleep,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Time read
    let time_needles = [
        "System.currentTimeMillis()",
        "System.nanoTime()",
        "Instant.now()",
        "LocalDateTime.now()",
        "LocalDate.now()",
        "ZonedDateTime.now()",
        "new Date()",
        "Calendar.getInstance()",
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
        "new Random()",
        "new SecureRandom()",
        "Math.random()",
        "ThreadLocalRandom.current()",
        "UUID.randomUUID()",
    ];
    if let Some(note) = first_match_note(body, &rand_needles) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Throw
    let throw_needles = ["throw new ", "throw e;", "throw ex;", "throw t;"];
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

/// Import binding: fully-qualified class → simple name.
#[derive(Debug, Clone)]
struct ImportBinding {
    /// The fully-qualified class name (dotted), e.g. `com.example.payments.ChargeService`.
    fqn: String,
    /// Simple name used in code, e.g. `ChargeService`.
    simple: String,
}

fn parse_imports(source: &str) -> Vec<ImportBinding> {
    let mut out = Vec::new();
    for line in source.lines() {
        let t = line.trim();
        // skip static imports and wildcards
        if let Some(rest) = t.strip_prefix("import ") {
            let fqn = rest
                .trim_end_matches(';')
                .trim()
                .trim_start_matches("static ");
            if fqn.ends_with('*') || fqn.is_empty() {
                continue;
            }
            let simple = fqn.rsplit('.').next().unwrap_or(fqn).to_string();
            out.push(ImportBinding {
                fqn: fqn.to_string(),
                simple,
            });
        }
    }
    out
}

fn enclosing_type_qname<'a>(qname: &str, known: &HashSet<&'a str>) -> Option<String> {
    // Strip last segment to get enclosing class, e.g. com.example.Foo.bar -> com.example.Foo
    let idx = qname.rfind('.')?;
    let parent = &qname[..idx];
    if known.contains(parent) {
        Some(parent.to_string())
    } else {
        None
    }
}

fn collect_method_calls(
    node: Node<'_>,
    src: &[u8],
    sym: &ParsedSymbol,
    pkg_prefix: &str,
    by_simple: &HashMap<String, String>,
    known: &HashSet<&str>,
    imports: &[ImportBinding],
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    edges: &mut HashSet<CallEdge>,
) {
    if node.kind() == "method_invocation" {
        // object.method(args) → child 0 = object, field "name" = method
        let method_name = child_by_field(node, "name")
            .map(|n| node_text(n, src))
            .unwrap_or("");
        if !method_name.is_empty() {
            let object_node = node.child(0);
            let object_text = object_node
                .map(|n| node_text(n, src))
                .unwrap_or("");

            // Build candidate callee qname
            let callee = if object_text.is_empty() || object_text == method_name {
                // Bare method call — try within enclosing type then module
                if let Some(et) = enclosing_type {
                    let q = format!("{}.{}", et, method_name);
                    if known.contains(q.as_str()) {
                        Some(q)
                    } else if let Some(q2) = by_simple.get(method_name) {
                        Some(q2.clone())
                    } else {
                        None
                    }
                } else {
                    by_simple.get(method_name).cloned()
                }
            } else {
                // Qualified: look up object type via imports
                let import_fqn = imports.iter().find(|b| b.simple == object_text);
                if let Some(binding) = import_fqn {
                    // e.g. com.example.ChargeService.charge
                    let q = format!("{}.{}", binding.fqn, method_name);
                    if known.contains(q.as_str()) || workspace.contains(&q) {
                        Some(q)
                    } else {
                        None
                    }
                } else {
                    // Try package-local
                    let q = if pkg_prefix.is_empty() {
                        format!("{}.{}", object_text, method_name)
                    } else {
                        format!("{}.{}.{}", pkg_prefix, object_text, method_name)
                    };
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
        collect_method_calls(
            node.child(i).unwrap(),
            src,
            sym,
            pkg_prefix,
            by_simple,
            known,
            imports,
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
    let pkg_prefix = package_prefix(source);
    let known: HashSet<&str> = symbols.iter().map(|s| s.qname.as_str()).collect();

    let mut by_simple: HashMap<String, String> = HashMap::new();
    for s in symbols {
        let simple = s.qname.rsplit('.').next().unwrap_or(&s.qname).to_string();
        by_simple.entry(simple).or_insert_with(|| s.qname.clone());
    }

    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let imports = parse_imports(source);
    let mut edges: HashSet<CallEdge> = HashSet::new();

    for sym in symbols {
        if !matches!(
            sym.kind,
            SymbolKind::Function | SymbolKind::Method
        ) {
            continue;
        }
        let src_bytes = sym.body.as_bytes();
        let tree = match parser.parse(src_bytes, None) {
            Some(t) => t,
            None => continue,
        };
        let enclosing_type = enclosing_type_qname(&sym.qname, &known);
        collect_method_calls(
            tree.root_node(),
            src_bytes,
            sym,
            &pkg_prefix,
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

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentstatedeveloper_core::adapter::{LanguageAdapter, WorkspaceSymbols};

    fn adapter() -> JavaAdapter {
        JavaAdapter::new()
    }

    #[test]
    fn parses_class_method_interface_enum() {
        let src = r#"
package com.example;

public class PaymentService {
    public void charge(String customerId, long amount) {
        System.out.println("charging");
    }

    public static String format(long cents) {
        return String.format("$%.2f", cents / 100.0);
    }
}

public interface PaymentGateway {
    void process(Payment p);
}

public enum Currency { USD, EUR, GBP }
"#;
        let syms = adapter().parse_symbols("src/PaymentService.java", src).unwrap();
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(qnames.contains(&"com.example.PaymentService"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.PaymentService.charge"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.PaymentService.format"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.PaymentGateway"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.Currency"), "{qnames:?}");
    }

    #[test]
    fn module_prefix_strips_package() {
        assert_eq!(package_prefix("package com.example.payments;"), "com.example.payments");
        assert_eq!(package_prefix("// no package"), "");
        assert_eq!(package_prefix("package  org.acme ;"), "org.acme");
    }

    #[test]
    fn infers_fs_read_and_net_out() {
        let src = r#"
public class Fetcher {
    public String fetch() throws Exception {
        byte[] data = Files.readAllBytes(Paths.get("/tmp/config"));
        HttpClient client = HttpClient.newHttpClient();
        HttpRequest req = HttpRequest.newBuilder()
            .uri(URI.create("https://api.example.com/data"))
            .build();
        return new String(data);
    }
}
"#;
        let syms = adapter().parse_symbols("Fetcher.java", src).unwrap();
        let fetch = syms.iter().find(|s| s.qname.ends_with(".fetch")).unwrap();
        let effs = adapter().infer_effects("", fetch);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::IoFsRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::IoNetOut), "{cats:?}");
        // Check host extraction
        let net = effs.iter().find(|e| e.effect == EffectCategory::IoNetOut).unwrap();
        if let Some(hosts) = net.qualifiers.get("hosts") {
            let hosts: Vec<&str> = hosts.as_array().unwrap().iter()
                .map(|v| v.as_str().unwrap()).collect();
            assert!(hosts.contains(&"api.example.com"), "{hosts:?}");
        }
    }

    #[test]
    fn infers_db_read_and_write() {
        let src = r#"
public class UserRepo {
    public User find(long id) {
        return entityManager.find(User.class, id);
    }
    public void save(User u) {
        entityManager.persist(u);
    }
}
"#;
        let syms = adapter().parse_symbols("UserRepo.java", src).unwrap();
        let find_sym = syms.iter().find(|s| s.qname.ends_with(".find")).unwrap();
        let save_sym = syms.iter().find(|s| s.qname.ends_with(".save")).unwrap();
        let find_effs = adapter().infer_effects("", find_sym);
        let save_effs = adapter().infer_effects("", save_sym);
        assert!(find_effs.iter().any(|e| e.effect == EffectCategory::IoDbRead));
        assert!(save_effs.iter().any(|e| e.effect == EffectCategory::IoDbWrite));
    }

    #[test]
    fn infers_log_and_env() {
        let src = r#"
public class Config {
    private static final Logger logger = LoggerFactory.getLogger(Config.class);
    public String load() {
        String env = System.getenv("APP_ENV");
        logger.info("Loading config for env: {}", env);
        return env;
    }
}
"#;
        let syms = adapter().parse_symbols("Config.java", src).unwrap();
        let load = syms.iter().find(|s| s.qname.ends_with(".load")).unwrap();
        let effs = adapter().infer_effects("", load);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::EnvRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::Log), "{cats:?}");
        // Check env var extraction
        let env_eff = effs.iter().find(|e| e.effect == EffectCategory::EnvRead).unwrap();
        if let Some(vars) = env_eff.qualifiers.get("vars") {
            let vars: Vec<&str> = vars.as_array().unwrap().iter()
                .map(|v| v.as_str().unwrap()).collect();
            assert!(vars.contains(&"APP_ENV"), "{vars:?}");
        }
    }

    #[test]
    fn empty_when_no_patterns() {
        let src = r#"
public class Pure {
    public int add(int a, int b) { return a + b; }
}
"#;
        let syms = adapter().parse_symbols("Pure.java", src).unwrap();
        let add = syms.iter().find(|s| s.qname.ends_with(".add")).unwrap();
        let effs = adapter().infer_effects("", add);
        assert!(effs.is_empty(), "{effs:?}");
    }

    #[test]
    fn extracts_cross_module_call_edges() {
        let src = r#"
package com.example;

import com.payments.ChargeService;

public class OrderService {
    public void placeOrder(Order order) {
        ChargeService.charge(order.getCustomerId(), order.getTotal());
    }
}

public class ChargeService {
    public static void charge(String customerId, long amount) {
        // process payment
    }
}
"#;
        let ws = WorkspaceSymbols {
            qnames: ["com.payments.ChargeService.charge".to_string()].into(),
            kinds: HashMap::new(),
        };
        let syms = adapter().parse_symbols("src/OrderService.java", src).unwrap();
        let edges = adapter().extract_call_edges("src/OrderService.java", src, &syms, &ws);
        // Should find a call from placeOrder to com.payments.ChargeService.charge
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".placeOrder")
                && e.callee_qname == "com.payments.ChargeService.charge"
        });
        assert!(found, "expected cross-module edge; got: {edges:?}");
    }
}
