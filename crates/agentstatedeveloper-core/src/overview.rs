//! Read-only "orient me" reports shared by the CLI and the `asd-mcp` server.
//!
//! These build the same JSON the `asd architecture`, `asd dead-code`, and
//! `asd endpoints` commands emit in `--agent` mode. Hoisting them into core
//! means the CLI and the MCP tools return byte-identical payloads and can
//! never drift. The logic was moved verbatim from the CLI command modules.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};

use crate::{
    Direction, Engine, Symbol, SymbolKind, classify_layer_sym, detect_communities,
    endpoints_from_tree, load_layer_overrides, match_edges, resolve_repo_id, symbol_tier,
};

/// `id → Symbol` map for the current ref. Fast path via the SQLite symbol
/// cache, authoritative git fallback. Shared by the many commands that need to
/// resolve call-graph node ids back to symbols.
pub fn build_id_map(engine: &Engine) -> HashMap<String, Symbol> {
    // Fast path: SQLite symbol cache — reuse engine's already-open connection.
    if let Some(fts) = engine.fts.as_ref() {
        if fts.symbols_cached_for(&engine.ref_name) {
            let map = fts.build_id_map_cached(&engine.ref_name);
            if !map.is_empty() {
                return map;
            }
        }
    }
    // Authoritative git fallback.
    let tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap_or(Value::Object(Default::default()));
    tree.as_object()
        .map(|m| {
            m.values()
                .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                .map(|s| (s.symbol_id.clone(), s))
                .collect()
        })
        .unwrap_or_default()
}

// ── architecture ───────────────────────────────────────────────────────────

/// One-call "orient me" overview: languages, packages, layers, call-graph
/// communities (clusters), inbound/outbound routes, and hotspots. This is the
/// JSON body `asd architecture --agent` prints.
pub fn architecture_overview(engine: &Engine, top: usize) -> Value {
    let id_map = build_id_map(engine);
    let db_path = engine.db_path.clone().unwrap_or_default();
    let overrides = load_layer_overrides(&db_path);

    // Languages, packages, layers — one pass over all symbols.
    let mut languages: HashMap<String, usize> = HashMap::new();
    let mut packages: HashMap<String, usize> = HashMap::new();
    let mut layers: BTreeMap<&'static str, usize> = BTreeMap::new();
    for s in id_map.values() {
        *languages.entry(s.language.clone()).or_default() += 1;
        *packages.entry(package_of(&s.file)).or_default() += 1;
        let layer = classify_layer_sym(&s.file, &s.qname, symbol_tier(&s.file), &overrides);
        *layers.entry(layer).or_default() += 1;
    }

    // Hotspots: call-graph degree = |callers| + |callees|.
    let degree = symbol_degree(engine);
    let mut hot: Vec<(&String, usize)> = degree.iter().map(|(k, v)| (k, *v)).collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let hotspots: Vec<Value> = hot
        .iter()
        .take(top)
        .filter_map(|(id, deg)| {
            id_map
                .get(*id)
                .map(|s| json!({ "qname": s.qname, "file": s.file, "degree": deg }))
        })
        .collect();

    // Routes: inbound endpoints this repo serves (+ outbound consumer count).
    let ep_tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
        .unwrap_or(Value::Null);
    let endpoints = endpoints_from_tree(&ep_tree);
    let mut inbound: Vec<Value> = endpoints
        .iter()
        .filter(|e| e.direction == Direction::Inbound)
        .map(|e| json!({ "contract": e.contract, "qname": e.qname }))
        .collect();
    inbound.sort_by(|a, b| a["contract"].as_str().cmp(&b["contract"].as_str()));
    let outbound_count = endpoints
        .iter()
        .filter(|e| e.direction == Direction::Outbound)
        .count();

    // Functional clusters: call-graph communities (Louvain local-move), labeled
    // by their dominant package + highest-degree representative.
    let clusters = compute_clusters(engine, &id_map, &degree, top);

    let repo_id = resolve_repo_id(std::env::var("ASD_REPO_ID").ok().as_deref(), &cwd());

    json!({
        "repo_id": repo_id,
        "symbols": id_map.len(),
        "languages": sorted_counts(languages, usize::MAX),
        "packages": sorted_counts(packages, top),
        "layers": layers.iter().map(|(k, v)| json!({ "layer": k, "symbols": v })).collect::<Vec<_>>(),
        "clusters": clusters,
        "routes": { "inbound": inbound, "outbound_consumers": outbound_count },
        "hotspots": hotspots,
        "note": "clusters are call-graph communities (functional groups that call each other); layers are the orthogonal path-based view",
    })
}

/// Parent directory of a file, used as a coarse package/module grouping.
fn package_of(file: &str) -> String {
    match file.rfind('/') {
        Some(i) => file[..i].to_string(),
        None => ".".to_string(),
    }
}

/// Call-graph degree per symbol_id from the callers/callees registry trees.
fn symbol_degree(engine: &Engine) -> HashMap<String, usize> {
    let mut degree: HashMap<String, usize> = HashMap::new();
    for (path, key) in [
        ("/asd/v1/index/callees", "callees"),
        ("/asd/v1/index/callers", "callers"),
    ] {
        let tree = engine
            .repo
            .get_tree(&engine.ref_name, path)
            .unwrap_or(Value::Null);
        if let Some(obj) = tree.as_object() {
            for (sym_id, v) in obj {
                let n = v.get(key).and_then(|x| x.as_array()).map_or(0, |a| a.len());
                *degree.entry(sym_id.clone()).or_default() += n;
            }
        }
    }
    degree
}

/// Functional clusters = call-graph communities, each labeled by its dominant
/// package and highest-degree representative symbol. Singletons are dropped.
fn compute_clusters(
    engine: &Engine,
    id_map: &HashMap<String, Symbol>,
    degree: &HashMap<String, usize>,
    top: usize,
) -> Vec<Value> {
    let tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/callees")
        .unwrap_or(Value::Null);
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut node_set: BTreeSet<String> = BTreeSet::new();
    if let Some(obj) = tree.as_object() {
        for (sym, v) in obj {
            if let Some(arr) = v.get("callees").and_then(|a| a.as_array()) {
                for c in arr.iter().filter_map(|c| c.as_str()) {
                    node_set.insert(sym.clone());
                    node_set.insert(c.to_string());
                    edges.push((sym.clone(), c.to_string()));
                }
            }
        }
    }
    if edges.is_empty() {
        return Vec::new();
    }
    let nodes: Vec<String> = node_set.into_iter().collect();
    let comm = detect_communities(&nodes, &edges);

    let mut groups: HashMap<usize, Vec<&String>> = HashMap::new();
    for (sym, c) in &comm {
        groups.entry(*c).or_default().push(sym);
    }

    let mut clusters: Vec<(usize, Value)> = groups
        .into_values()
        .filter(|members| members.len() >= 2)
        .map(|members| {
            // Dominant package among members (ties → lexicographically smaller).
            let mut pkg: HashMap<String, usize> = HashMap::new();
            for m in &members {
                if let Some(s) = id_map.get(*m) {
                    *pkg.entry(package_of(&s.file)).or_default() += 1;
                }
            }
            let package = pkg
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                .map(|(p, _)| p)
                .unwrap_or_default();
            // Representative = highest-degree member.
            let representative = members
                .iter()
                .filter_map(|m| {
                    id_map.get(*m).map(|s| (degree.get(*m).copied().unwrap_or(0), s.qname.clone()))
                })
                .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
                .map(|(_, q)| q)
                .unwrap_or_default();
            (
                members.len(),
                json!({ "size": members.len(), "package": package, "representative": representative }),
            )
        })
        .collect();
    clusters.sort_by(|a, b| b.0.cmp(&a.0));
    clusters.into_iter().take(top).map(|(_, v)| v).collect()
}

/// `[{name, symbols}]` sorted by count desc then name, truncated to `limit`.
fn sorted_counts(map: HashMap<String, usize>, limit: usize) -> Vec<Value> {
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter()
        .take(limit)
        .map(|(name, n)| json!({ "name": name, "symbols": n }))
        .collect()
}

fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

// ── dead code ──────────────────────────────────────────────────────────────

/// Functions/methods with no inbound call edges — a *candidate* list. Excludes
/// HTTP route handlers, test functions (unless `include_tests`), and
/// `main`/dunder methods. This is the JSON body `asd dead-code --agent` prints.
pub fn dead_code_report(engine: &Engine, limit: usize, include_tests: bool) -> Value {
    let id_map = build_id_map(engine);

    // Symbols that have at least one inbound caller.
    let callers_tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/callers")
        .unwrap_or(Value::Null);
    let mut has_callers: HashSet<String> = HashSet::new();
    if let Some(obj) = callers_tree.as_object() {
        for (sym_id, v) in obj {
            let n = v
                .get("callers")
                .and_then(|a| a.as_array())
                .map_or(0, |a| a.len());
            if n > 0 {
                has_callers.insert(sym_id.clone());
            }
        }
    }

    // Inbound endpoint owners are reachable over HTTP without a static caller.
    let ep_tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
        .unwrap_or(Value::Null);
    let handler_syms: HashSet<String> = endpoints_from_tree(&ep_tree)
        .into_iter()
        .filter(|e| e.direction == Direction::Inbound)
        .map(|e| e.symbol_id)
        .collect();

    let mut excluded_handlers = 0usize;
    let mut excluded_tests = 0usize;
    let mut dead: Vec<&Symbol> = Vec::new();
    for (sym_id, sym) in &id_map {
        if !matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        if has_callers.contains(sym_id) {
            continue;
        }
        if is_runtime_entry(&sym.qname) {
            continue;
        }
        if handler_syms.contains(sym_id) {
            excluded_handlers += 1;
            continue;
        }
        if !include_tests && is_test_symbol(&sym.file, &sym.qname) {
            excluded_tests += 1;
            continue;
        }
        dead.push(sym);
    }
    dead.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.start.line.cmp(&b.start.line))
    });

    let total = dead.len();
    let candidates: Vec<Value> = dead
        .iter()
        .take(limit)
        .map(|s| {
            json!({
                "qname": s.qname,
                "file": s.file,
                "line": s.start.line,
                "kind": dc_kind_str(s.kind),
            })
        })
        .collect();

    let note = "Functions/methods with no inbound call edges in the index. NOT definitive — \
                the static call graph misses public API used by other repos, dynamic dispatch \
                (reflection, callbacks, trait/interface impls), framework-invoked methods, and \
                calls the resolver couldn't bind (see `dropped_call_edges` in `asd index`). \
                Route handlers, test functions, and main/dunder methods are excluded.";

    json!({
        "count": total,
        "shown": candidates.len(),
        "excluded": { "route_handlers": excluded_handlers, "test_symbols": excluded_tests },
        "note": note,
        "candidates": candidates,
    })
}

/// Runtime-reachable by name even without a static caller: program entry points
/// and dunder methods the language runtime invokes implicitly.
fn is_runtime_entry(qname: &str) -> bool {
    let last = qname.rsplit(['.', ':']).next().unwrap_or(qname);
    last == "main" || (last.starts_with("__") && last.ends_with("__"))
}

/// A test symbol by file path (tier 2) OR by an inline test module in its qname.
fn is_test_symbol(file: &str, qname: &str) -> bool {
    symbol_tier(file) == 2 || qname.split(['.', ':']).any(|c| c == "tests" || c == "test")
}

fn dc_kind_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Module => "module",
        SymbolKind::Variable => "variable",
    }
}

// ── endpoints ──────────────────────────────────────────────────────────────

/// Cross-service endpoints detected in this repo plus the in-repo matched
/// edges. This is the JSON body `asd endpoints --agent` prints.
pub fn endpoints_report(engine: &Engine) -> Value {
    let tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
        .unwrap_or(Value::Null);
    let mut endpoints = endpoints_from_tree(&tree);
    endpoints.sort_by(|a, b| a.contract.cmp(&b.contract).then(a.qname.cmp(&b.qname)));
    let edges = match_edges(&endpoints);
    json!({ "endpoints": endpoints, "edges": edges })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_of_uses_parent_dir() {
        assert_eq!(package_of("crates/core/src/lib.rs"), "crates/core/src");
        assert_eq!(package_of("main.rs"), ".");
    }

    #[test]
    fn sorted_counts_orders_desc_then_name_and_truncates() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), 1);
        m.insert("b".to_string(), 3);
        m.insert("c".to_string(), 3);
        let v = sorted_counts(m, 2);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["name"], "b");
        assert_eq!(v[0]["symbols"], 3);
        assert_eq!(v[1]["name"], "c");
    }

    #[test]
    fn runtime_entries_excluded() {
        assert!(is_runtime_entry("app.main"));
        assert!(is_runtime_entry("Foo.__init__"));
        assert!(is_runtime_entry("mod::__repr__"));
        assert!(!is_runtime_entry("app.helper"));
        assert!(!is_runtime_entry("Foo.compute"));
        assert!(!is_runtime_entry("app.do_thing"));
    }

    #[test]
    fn test_symbols_detected_by_path_and_qname() {
        assert!(is_test_symbol(
            "crates/x/src/lib.rs",
            "x.lib.tests.it_works"
        ));
        assert!(is_test_symbol(
            "crates/x/src/lib.rs",
            "x::lib::tests::it_works"
        ));
        assert!(is_test_symbol("crates/x/tests/foo.rs", "x.foo.bar"));
        assert!(!is_test_symbol("crates/x/src/lib.rs", "x.lib.compute"));
        assert!(!is_test_symbol("src/lib.rs", "app.latest_value"));
    }
}
