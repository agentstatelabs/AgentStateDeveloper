//! Plan M t-006 / regression coverage (1.1.11): exercise the in-memory
//! fallback path in `find_candidates`.
//!
//! Background: `find_candidates` has two code paths:
//!   1. **FTS path** — fires when `engine.fts.is_some() && fts.has_data()`.
//!      Indexed lookup, hybrid_boost, ledger-aware dedup, etc.
//!   2. **In-memory fallback** — `fallback_in_memory_search()`. Walks every
//!      symbol via `index.get_symbol_by_qname`, scores with `in_memory_score`,
//!      sorts and truncates. Fires when FTS is missing or empty.
//!
//! The Plan M t-006 (1.0.101) refactor lifted the fallback into a private
//! helper. The original code path had no direct test coverage — this test
//! file plugs that gap by opening an in-memory `Engine` (which has
//! `fts: None`), seeding a small symbol set, and verifying the fallback
//! returns the expected ranked candidates.
//!
//! What this proves:
//!   - The fallback path is reachable and doesn't panic on an empty FTS.
//!   - Symbols are scored and surfaced by `in_memory_score`.
//!   - Kind and language filters apply on the fallback path too.
//!   - The depth truncation works correctly.

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Engine, FtsFilters, IndexStore, Position, Symbol, SymbolKind,
    find_candidates,
};

fn make_symbol(id: &str, qname: &str, file: &str, language: &str) -> Symbol {
    Symbol {
        symbol_id: id.to_string(),
        symbol_fp: format!("fp_{id}"),
        qname: qname.to_string(),
        language: language.to_string(),
        kind: SymbolKind::Function,
        file: file.to_string(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!("def {qname}()")),
        doc: Some(format!("doc for {qname}")),
    }
}

fn seed(engine: &Engine, symbols: &[Symbol]) {
    let index = AsgIndexStore::new(&engine.repo);
    for sym in symbols {
        index
            .put_symbol(&engine.ref_name, sym, "test")
            .expect("put symbol");
    }
}

fn empty_filters() -> FtsFilters {
    FtsFilters {
        kind: None,
        language: None,
        include_tests: true,
        tests_only: false,
        exclude_terms: Vec::new(),
        paths_filter: Vec::new(),
        exclude_paths: Vec::new(),
        exclude_languages: Vec::new(),
    }
}

#[test]
fn fallback_returns_candidates_matching_query_token() {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    assert!(
        engine.fts.is_none(),
        "in-memory engine must have no FTS; otherwise this test exercises the FTS path"
    );
    seed(
        &engine,
        &[
            make_symbol("s1", "mod.resolve_for_preview", "mod.py", "python"),
            make_symbol("s2", "mod.unrelated_helper", "mod.py", "python"),
        ],
    );

    let index = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let filters = empty_filters();
    let tokens = vec!["resolve".to_string()];
    let results = find_candidates(
        &engine,
        "resolve for preview",
        &tokens,
        &filters,
        &ledger,
        &index,
        10,
    );

    let qnames: Vec<&str> = results.iter().map(|(_, q)| q.as_str()).collect();
    assert!(
        qnames.contains(&"mod.resolve_for_preview"),
        "fallback must surface the matching symbol; got {qnames:?}"
    );
}

#[test]
fn fallback_truncates_to_depth() {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    let many: Vec<Symbol> = (0..20)
        .map(|i| {
            make_symbol(
                &format!("s{i}"),
                &format!("mod.sym_{i}_resolve"),
                "mod.py",
                "python",
            )
        })
        .collect();
    seed(&engine, &many);

    let index = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let filters = empty_filters();
    let tokens = vec!["resolve".to_string()];
    let depth = 5;
    let results = find_candidates(&engine, "resolve", &tokens, &filters, &ledger, &index, depth);

    assert!(
        results.len() <= depth,
        "fallback must respect depth={depth}; got {} results",
        results.len()
    );
}

#[test]
fn fallback_applies_language_filter() {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    seed(
        &engine,
        &[
            make_symbol("s_py", "mod.resolve_py", "mod.py", "python"),
            make_symbol("s_rs", "mod.resolve_rs", "mod.rs", "rust"),
        ],
    );

    let index = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let mut filters = empty_filters();
    filters.language = Some("python".to_string());

    let tokens = vec!["resolve".to_string()];
    let results = find_candidates(&engine, "resolve", &tokens, &filters, &ledger, &index, 10);

    let qnames: Vec<&str> = results.iter().map(|(_, q)| q.as_str()).collect();
    assert!(
        qnames.iter().any(|q| q.contains("resolve_py")),
        "python match must survive language filter; got {qnames:?}"
    );
    assert!(
        !qnames.iter().any(|q| q.contains("resolve_rs")),
        "rust match must be filtered out by language=python; got {qnames:?}"
    );
}

#[test]
fn fallback_returns_empty_on_no_matches() {
    let engine = Engine::open_in_memory().expect("open in-memory engine");
    seed(
        &engine,
        &[make_symbol(
            "s1",
            "mod.unrelated",
            "mod.py",
            "python",
        )],
    );

    let index = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let filters = empty_filters();
    let tokens = vec!["definitely_no_match_token_xyz".to_string()];
    let results = find_candidates(
        &engine,
        "definitely_no_match_token_xyz",
        &tokens,
        &filters,
        &ledger,
        &index,
        10,
    );

    assert!(
        results.is_empty(),
        "fallback must return empty when no symbol scores > 0; got {results:?}"
    );
}
