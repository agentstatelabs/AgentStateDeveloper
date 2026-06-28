//! `asd dead-code` — functions/methods with no inbound call edges in the index.
//!
//! This is the "NOT EXISTS inbound CALLS" signal. It is a *candidate* list, not
//! a verdict: the static call graph does not capture public API used by other
//! repos, dynamic dispatch (reflection / callbacks / trait objects), or
//! framework-invoked methods. To cut the obvious false positives we exclude
//! reachability roots we *do* know about — HTTP route handlers (from the t-002
//! endpoint registry), test functions, and `main`/dunder methods.

use std::collections::HashSet;

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    Direction, Engine, Symbol, SymbolKind, endpoints_from_tree, symbol_tier,
};

use crate::commands::graph::build_id_map;
use crate::config::Config;

#[derive(Debug, Args)]
pub struct DeadCodeArgs {
    /// Machine-readable JSON.
    #[arg(long)]
    pub agent: bool,

    /// Max candidates to list (the total count is always reported).
    #[arg(long, default_value = "50")]
    pub limit: usize,

    /// Include test functions (excluded by default).
    #[arg(long)]
    pub include_tests: bool,
}

pub fn run(cfg: &Config, args: DeadCodeArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let id_map = build_id_map(&engine);

    // Symbols that have at least one inbound caller.
    let callers_tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/callers")
        .unwrap_or(Value::Null);
    let mut has_callers: HashSet<String> = HashSet::new();
    if let Some(obj) = callers_tree.as_object() {
        for (sym_id, v) in obj {
            let n = v.get("callers").and_then(|a| a.as_array()).map_or(0, |a| a.len());
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
        // Only functions/methods have a meaningful "is it called" question.
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
        if !args.include_tests && is_test_symbol(&sym.file, &sym.qname) {
            excluded_tests += 1;
            continue;
        }
        dead.push(sym);
    }
    dead.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.start.line.cmp(&b.start.line)));

    let total = dead.len();
    let candidates: Vec<Value> = dead
        .iter()
        .take(args.limit)
        .map(|s| {
            json!({
                "qname": s.qname,
                "file": s.file,
                "line": s.start.line,
                "kind": kind_str(s.kind),
            })
        })
        .collect();

    let note = "Functions/methods with no inbound call edges in the index. NOT definitive — \
                the static call graph misses public API used by other repos, dynamic dispatch \
                (reflection, callbacks, trait/interface impls), framework-invoked methods, and \
                calls the resolver couldn't bind (see `dropped_call_edges` in `asd index`). \
                Route handlers, test functions, and main/dunder methods are excluded.";

    let out = json!({
        "count": total,
        "shown": candidates.len(),
        "excluded": { "route_handlers": excluded_handlers, "test_symbols": excluded_tests },
        "note": note,
        "candidates": candidates,
    });

    if args.agent {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "{} unreferenced function/method{} (no inbound call edges).",
        total,
        if total == 1 { "" } else { "s" }
    );
    if excluded_handlers > 0 || excluded_tests > 0 {
        println!(
            "  (excluded {} route handler(s), {} test symbol(s))",
            excluded_handlers, excluded_tests
        );
    }
    for c in candidates.iter() {
        println!(
            "  {}:{}  {}",
            c["file"].as_str().unwrap_or("?"),
            c["line"].as_u64().unwrap_or(0),
            c["qname"].as_str().unwrap_or("?")
        );
    }
    if total > candidates.len() {
        println!("  … {} more (use --limit)", total - candidates.len());
    }
    println!("\nNote: candidates only — static call graph misses public API, dynamic dispatch, and framework callbacks.");
    Ok(())
}

/// Runtime-reachable by name even without a static caller: program entry points
/// and dunder methods (`__init__`, `__repr__`, `__enter__`, …) the language
/// runtime invokes implicitly.
fn is_runtime_entry(qname: &str) -> bool {
    let last = qname.rsplit(['.', ':']).next().unwrap_or(qname);
    last == "main" || (last.starts_with("__") && last.ends_with("__"))
}

/// A test symbol by file path (tier 2: `tests/`, `*_test.*`) OR by an inline
/// test module in its qname (`...::tests::foo` / `....tests.foo`) — the latter
/// catches Rust's `#[cfg(test)] mod tests` inside a `src/` file, which the
/// path-based tier check misses.
fn is_test_symbol(file: &str, qname: &str) -> bool {
    symbol_tier(file) == 2 || qname.split(['.', ':']).any(|c| c == "tests" || c == "test")
}

fn kind_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Module => "module",
        SymbolKind::Variable => "variable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_entries_excluded() {
        assert!(is_runtime_entry("app.main"));
        assert!(is_runtime_entry("Foo.__init__"));
        assert!(is_runtime_entry("mod::__repr__"));
        assert!(!is_runtime_entry("app.helper"));
        assert!(!is_runtime_entry("Foo.compute"));
        // A normal name that merely contains underscores is not a dunder.
        assert!(!is_runtime_entry("app.do_thing"));
    }

    #[test]
    fn test_symbols_detected_by_path_and_qname() {
        // Inline #[cfg(test)] mod tests inside a src file (path isn't tier-2).
        assert!(is_test_symbol("crates/x/src/lib.rs", "x.lib.tests.it_works"));
        assert!(is_test_symbol("crates/x/src/lib.rs", "x::lib::tests::it_works"));
        // Path-based test file.
        assert!(is_test_symbol("crates/x/tests/foo.rs", "x.foo.bar"));
        // Ordinary source symbol is not a test.
        assert!(!is_test_symbol("crates/x/src/lib.rs", "x.lib.compute"));
        // A name merely containing "test" as a substring isn't a tests module.
        assert!(!is_test_symbol("src/lib.rs", "app.latest_value"));
    }
}
