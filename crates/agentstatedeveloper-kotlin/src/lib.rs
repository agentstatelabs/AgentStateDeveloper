//! Kotlin language adapter for AgentStateDeveloper.
//!
//! Implements [`LanguageAdapter`](agentstatedeveloper_core::LanguageAdapter)
//! on top of `tree-sitter-kotlin`. Parses classes, objects, interfaces, and
//! functions/methods with package-qualified names, then runs substring-based
//! effect inference.

use std::collections::{HashMap, HashSet};

use agentstatedeveloper_core::adapter::{
    CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols,
};
use agentstatedeveloper_core::error::{AsdError, Result};
use agentstatedeveloper_core::schema::{Effect, EffectCategory, SymbolKind};
use serde_json::json;
use tree_sitter::{Node, Parser};
use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_kotlin() -> *const ();
}

const KOTLIN: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_kotlin) };

/// Kotlin language adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct KotlinAdapter;

impl KotlinAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageAdapter for KotlinAdapter {
    fn language(&self) -> &str {
        "kotlin"
    }

    fn file_extensions(&self) -> &'static [&'static str] {
        &["kt", "kts"]
    }

    fn parse_symbols(&self, file: &str, source: &str) -> Result<Vec<ParsedSymbol>> {
        let mut parser = Parser::new();
        parser
            .set_language(&KOTLIN.into())
            .map_err(|e| AsdError::Parse(format!("failed to set kotlin language: {e}")))?;

        let src_bytes = source.as_bytes();
        let tree = parser
            .parse(src_bytes, None)
            .ok_or_else(|| AsdError::Parse(format!("failed to parse {file}")))?;

        let pkg_prefix = package_prefix(source);
        let root = tree.root_node();
        let mut out = Vec::new();
        walk(root, src_bytes, &pkg_prefix, &mut out);
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

fn package_prefix(source: &str) -> String {
    for line in source.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("package ") {
            let pkg = rest.trim();
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

/// Find first child with a given node kind.
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
    make_symbol_sig(node, src, qname, kind, None)
}

fn make_symbol_sig(node: Node<'_>, src: &[u8], qname: String, kind: SymbolKind, signature: Option<String>) -> ParsedSymbol {
    ParsedSymbol {
        qname,
        kind,
        start_line: node.start_position().row as u32 + 1,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32 + 1,
        end_col: node.end_position().column as u32,
        body: node_text(node, src).to_string(),
        signature,
    }
}

/// Extract Kotlin function signature: text up to (not including) the body `{`.
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
// Symbol walking
// -----------------------------------------------------------------------------

fn walk(node: Node<'_>, src: &[u8], scope: &str, out: &mut Vec<ParsedSymbol>) {
    match node.kind() {
        "source_file" => {
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, scope, out);
            }
        }
        // Kotlin grammar (0.3.x) has no named fields — children are positional.
        // Class name is the first `type_identifier` child; body is `class_body`.
        "class_declaration" | "object_declaration" => {
            let name = find_child_by_kind(node, "type_identifier")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            out.push(make_symbol(node, src, qname.clone(), SymbolKind::Class));
            if let Some(body) = find_child_by_kind(node, "class_body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "companion_object" => {
            // companion object { ... } — name defaults to "Companion"
            let name = find_child_by_kind(node, "type_identifier")
                .map(|n| node_text(n, src))
                .unwrap_or("Companion");
            let qname = join_qname(scope, name);
            out.push(make_symbol(node, src, qname.clone(), SymbolKind::Class));
            if let Some(body) = find_child_by_kind(node, "class_body") {
                for i in 0..body.child_count() {
                    walk(body.child(i).unwrap(), src, &qname, out);
                }
            }
        }
        "function_declaration" => {
            // Name is the first `simple_identifier` child.
            let name = find_child_by_kind(node, "simple_identifier")
                .map(|n| node_text(n, src))
                .unwrap_or("");
            if name.is_empty() {
                return;
            }
            let qname = join_qname(scope, name);
            let kind = if scope.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            let sig = extract_sig_before_brace(node, src);
            out.push(make_symbol_sig(node, src, qname, kind, sig));
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
        "File(",
        ".readText(",
        ".readLines(",
        ".readBytes(",
        "Files.readString(",
        "Files.readAllBytes(",
        "BufferedReader(",
        "FileInputStream(",
        "FileReader(",
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
        ".writeText(",
        ".writeBytes(",
        ".appendText(",
        "Files.write(",
        "FileOutputStream(",
        "FileWriter(",
        ".createNewFile(",
        ".mkdir(",
        ".mkdirs(",
        ".delete(",
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
        "OkHttpClient(",
        "OkHttpClient.Builder(",
        ".newCall(",
        "Retrofit.Builder(",
        ".create(Service",
        "HttpURLConnection",
        "URL(",
        "ktor",
        "client.get(",
        "client.post(",
        "client.request(",
        "HttpClient(",
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
        "dataSource.connection",
        ".prepareStatement(",
        ".executeQuery(",
        ".executeUpdate(",
        "Room.databaseBuilder(",
        "dao.",
        "@Query(",
        "@Insert",
        "@Update",
        "@Delete",
        "Exposed.",
        "transaction {",
        ".from(",
        ".select(",
        ".insertAndGetId {",
    ];
    if let Some(note) = first_match_note(body, &db_needles) {
        let has_write = body.contains(".executeUpdate(")
            || body.contains("@Insert")
            || body.contains("@Update")
            || body.contains("@Delete")
            || body.contains(".insertAndGetId {")
            || body.contains(".update {")
            || body.contains(".deleteWhere {");
        let has_read = body.contains(".executeQuery(")
            || body.contains("@Query(")
            || body.contains(".select(")
            || body.contains(".from(");
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
        "ProcessBuilder(",
        "Runtime.getRuntime().exec(",
        "ProcessBuilder.start(",
    ];
    if let Some(note) = first_match_note(body, &proc_needles) {
        effects.push(Effect {
            effect: EffectCategory::ProcSpawn,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Env read
    let env_needles = ["System.getenv(", "System.getProperty(", "dotenv["];
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
        "println(",
        "print(",
        "Log.d(",
        "Log.i(",
        "Log.w(",
        "Log.e(",
        "logger.info(",
        "logger.debug(",
        "logger.warn(",
        "logger.error(",
        "Timber.",
        "slf4j",
    ];
    if let Some(note) = first_match_note(body, &log_needles) {
        effects.push(Effect {
            effect: EffectCategory::Log,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Time sleep
    let sleep_needles = ["Thread.sleep(", "delay(", "runBlocking { delay("];
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
        "LocalTime.now()",
        "ZonedDateTime.now()",
        "OffsetDateTime.now()",
        "Clock.System.now()",
        "measureTimeMillis",
        "measureNanoTime",
        "measureTimedValue",
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
        "Random.nextInt(",
        "Random.nextLong(",
        "Random.nextFloat(",
        "Random.nextDouble(",
        "Random.nextBoolean(",
        "Random.Default",
        "SecureRandom(",
        "UUID.randomUUID()",
        "kotlin.random.Random",
        ".random()",
    ];
    if let Some(note) = first_match_note(body, &rand_needles) {
        effects.push(Effect {
            effect: EffectCategory::Random,
            qualifiers: serde_json::Value::Null,
            note: Some(note),
        });
    }

    // Throw
    let throw_needles = ["throw ", "error(", "check(", "require("];
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

fn parse_imports(source: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for line in source.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("import ") {
            let fqn = rest.trim();
            if fqn.ends_with('*') {
                continue;
            }
            let simple = fqn.rsplit('.').next().unwrap_or(fqn);
            map.insert(simple.to_string(), fqn.to_string());
        }
    }
    map
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
    imports: &HashMap<String, String>,
    workspace: &WorkspaceSymbols,
    enclosing_type: Option<&str>,
    edges: &mut HashSet<CallEdge>,
) {
    if node.kind() == "call_expression" {
        // child[0] is either simple_identifier (bare call) or navigation_expression (qualified)
        let first = node.child(0);
        if let Some(cn) = first {
            let (receiver, method) = if cn.kind() == "navigation_expression" {
                // receiver.method(…) — receiver is first child, method is in navigation_suffix
                let recv = cn.child(0).map(|n| node_text(n, src)).unwrap_or("");
                let method = find_child_by_kind(cn, "navigation_suffix")
                    .and_then(|s| find_child_by_kind(s, "simple_identifier"))
                    .map(|n| node_text(n, src))
                    .unwrap_or("");
                (recv, method)
            } else if cn.kind() == "simple_identifier" {
                ("", node_text(cn, src))
            } else {
                ("", "")
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
                } else if let Some(fqn) = imports.get(receiver) {
                    let q = format!("{}.{}", fqn, method);
                    if known.contains(q.as_str()) || workspace.contains(&q) {
                        Some(q)
                    } else {
                        None
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
    let known: HashSet<&str> = symbols.iter().map(|s| s.qname.as_str()).collect();

    let mut by_simple: HashMap<String, String> = HashMap::new();
    for s in symbols {
        let simple = s.qname.rsplit('.').next().unwrap_or(&s.qname).to_string();
        by_simple.entry(simple).or_insert_with(|| s.qname.clone());
    }

    let mut parser = Parser::new();
    if parser.set_language(&KOTLIN.into()).is_err() {
        return Vec::new();
    }

    let imports = parse_imports(source);
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

    fn adapter() -> KotlinAdapter {
        KotlinAdapter::new()
    }

    #[test]
    fn parses_package_class_method_object() {
        let src = r#"
package com.example.payments

class PaymentService {
    fun charge(customerId: String, amount: Long): Receipt {
        return Receipt()
    }

    companion object {
        fun create(): PaymentService = PaymentService()
    }
}

interface Gateway {
    fun process(payment: Payment)
}

object Config {
    val timeout = 30
}
"#;
        let syms = adapter().parse_symbols("PaymentService.kt", src).unwrap();
        let qnames: Vec<&str> = syms.iter().map(|s| s.qname.as_str()).collect();
        assert!(qnames.contains(&"com.example.payments.PaymentService"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.payments.PaymentService.charge"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.payments.Gateway"), "{qnames:?}");
        assert!(qnames.contains(&"com.example.payments.Config"), "{qnames:?}");
    }

    #[test]
    fn package_prefix_parses_correctly() {
        assert_eq!(package_prefix("package com.example.payments"), "com.example.payments");
        assert_eq!(package_prefix("// no package"), "");
        assert_eq!(package_prefix("package  org.acme"), "org.acme");
    }

    #[test]
    fn infers_fs_read_and_net_out() {
        let src = r#"
class Fetcher {
    fun fetch(): String {
        val data = File("/tmp/config.yml").readText()
        val client = OkHttpClient()
        val request = Request.Builder()
            .url("https://api.example.com/v1/data")
            .build()
        return data
    }
}
"#;
        let syms = adapter().parse_symbols("Fetcher.kt", src).unwrap();
        let fetch = syms.iter().find(|s| s.qname.ends_with(".fetch")).unwrap();
        let effs = adapter().infer_effects("", fetch);
        let cats: Vec<_> = effs.iter().map(|e| &e.effect).collect();
        assert!(cats.contains(&&EffectCategory::IoFsRead), "{cats:?}");
        assert!(cats.contains(&&EffectCategory::IoNetOut), "{cats:?}");
    }

    #[test]
    fn infers_db_and_env() {
        let src = r#"
class UserRepo {
    fun findUser(id: Long): User? {
        val dbUrl = System.getenv("DATABASE_URL")
        return dao.findById(id)
    }
    fun saveUser(user: User) {
        dao.insertAndGetId { it[Users.name] = user.name }
    }
}
"#;
        let syms = adapter().parse_symbols("UserRepo.kt", src).unwrap();
        let find = syms.iter().find(|s| s.qname.ends_with(".findUser")).unwrap();
        let save = syms.iter().find(|s| s.qname.ends_with(".saveUser")).unwrap();
        let find_effs = adapter().infer_effects("", find);
        let save_effs = adapter().infer_effects("", save);
        assert!(find_effs.iter().any(|e| e.effect == EffectCategory::EnvRead));
        assert!(save_effs.iter().any(|e| e.effect == EffectCategory::IoDbWrite));

        let env_eff = find_effs.iter().find(|e| e.effect == EffectCategory::EnvRead).unwrap();
        if let Some(vars) = env_eff.qualifiers.get("vars") {
            let vars: Vec<&str> = vars.as_array().unwrap().iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(vars.contains(&"DATABASE_URL"), "{vars:?}");
        }
    }

    #[test]
    fn empty_when_no_patterns() {
        let src = r#"
class Math {
    fun add(a: Int, b: Int): Int = a + b
}
"#;
        let syms = adapter().parse_symbols("Math.kt", src).unwrap();
        let add = syms.iter().find(|s| s.qname.ends_with(".add")).unwrap();
        let effs = adapter().infer_effects("", add);
        assert!(effs.is_empty(), "{effs:?}");
    }

    #[test]
    fn extracts_intra_class_call_edges() {
        let src = r#"
package com.example

class OrderService {
    fun placeOrder(order: Order) {
        charge(order.customerId, order.total)
    }

    fun charge(customerId: String, amount: Long) {
        // process
    }
}
"#;
        let ws = WorkspaceSymbols {
            qnames: HashSet::new(),
            kinds: HashMap::new(),
        };
        let syms = adapter().parse_symbols("OrderService.kt", src).unwrap();
        let edges = adapter().extract_call_edges("OrderService.kt", src, &syms, &ws);
        let found = edges.iter().any(|e| {
            e.caller_qname.ends_with(".placeOrder") && e.callee_qname.ends_with(".charge")
        });
        assert!(found, "expected intra-class edge; got: {edges:?}");
    }
}
