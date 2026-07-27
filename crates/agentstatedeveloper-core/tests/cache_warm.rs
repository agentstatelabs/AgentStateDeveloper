//! Plan T: cache warm + open-time self-heal tests.
//!
//! Covers:
//! - `Engine::warm_caches` populates asd_symbols_cache / asd_call_edges /
//!   an empty FTS table from the authoritative git trees
//! - `Engine::open_sqlite` self-heals a DB whose git trees have symbols but
//!   whose SQLite caches are empty (born-cold hydrate DBs, past sync failures)
//! - in-memory engines skip cleanly
//! - warm reads git, never the (possibly stale) cache

use std::path::PathBuf;

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Position, Symbol, SymbolKind, paths,
};
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;

fn unique_tempdir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-cachewarm-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn make_symbol(n: usize) -> Symbol {
    Symbol {
        symbol_id: format!("sym_fn_mod_func{n}"),
        symbol_fp: format!("fp_{n:04}"),
        qname: format!("mod.func{n}"),
        language: "python".to_string(),
        kind: SymbolKind::Function,
        file: "mod.py".to_string(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!("def func{n}()")),
        doc: None,
    }
}

/// Seed two symbols and one call edge (func0 → func1) directly into the git
/// trees, bypassing every cache path — the state a hydrated or sync-failed
/// DB is in.
fn seed_git_only(engine: &Engine) -> (Symbol, Symbol) {
    let a = make_symbol(0);
    let b = make_symbol(1);
    let index = AsgIndexStore::new(&engine.repo);
    index
        .put_symbol(&engine.ref_name, &a, "test-agent")
        .expect("put symbol a");
    index
        .put_symbol(&engine.ref_name, &b, "test-agent")
        .expect("put symbol b");

    let opts = CommitOptions::new("test-agent", IntentCategory::Refine, "edges");
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &paths::callees_path(&a.symbol_id),
            &serde_json::json!({ "callees": [b.symbol_id.clone()] }),
            opts,
        )
        .expect("write callees");
    let opts = CommitOptions::new("test-agent", IntentCategory::Refine, "edges");
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &paths::callers_path(&b.symbol_id),
            &serde_json::json!({ "callers": [a.symbol_id.clone()] }),
            opts,
        )
        .expect("write callers");
    (a, b)
}

#[test]
fn warm_caches_populates_symbol_edge_and_fts_caches() {
    let dir = unique_tempdir("warm");
    let db = dir.join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).expect("open");
    let (a, b) = seed_git_only(&engine);

    let fts = engine.fts.as_ref().expect("sqlite engine has fts");
    assert!(
        !fts.symbols_cached_for(&engine.ref_name),
        "cache must be cold before warm"
    );
    assert_eq!(fts.fts_symbol_row_count(), 0, "FTS empty before warm");

    let w = engine.warm_caches().expect("warm");
    assert!(!w.skipped);
    assert_eq!(w.symbols_cached, 2);
    assert_eq!(w.edges_cached, 2, "one edge in each direction");
    assert!(w.fts_rebuilt, "empty FTS table must be rebuilt");

    assert!(fts.symbols_cached_for(&engine.ref_name));
    let id_map = fts.build_id_map_cached(&engine.ref_name);
    assert_eq!(id_map.len(), 2);
    assert_eq!(
        id_map.get(&a.symbol_id).map(|s| s.qname.as_str()),
        Some("mod.func0")
    );
    assert_eq!(fts.fts_symbol_row_count(), 2, "FTS rebuilt from git");

    // Cached edge reads agree with what was seeded.
    let store = AsgIndexStore::from_engine(&engine);
    assert_eq!(
        store
            .get_callees(&engine.ref_name, &a.symbol_id)
            .expect("callees"),
        vec![b.symbol_id.clone()]
    );
    assert_eq!(
        store
            .get_callers(&engine.ref_name, &b.symbol_id)
            .expect("callers"),
        vec![a.symbol_id.clone()]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn warm_caches_is_idempotent_and_reads_git_not_cache() {
    let dir = unique_tempdir("idem");
    let db = dir.join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).expect("open");
    seed_git_only(&engine);
    engine.warm_caches().expect("first warm");

    // Add a third symbol to git only — the cache is now stale.
    let c = make_symbol(2);
    AsgIndexStore::new(&engine.repo)
        .put_symbol(&engine.ref_name, &c, "test-agent")
        .expect("put symbol c");

    // A second warm must pick it up: warm reads git, never the cache.
    let w = engine.warm_caches().expect("second warm");
    assert_eq!(w.symbols_cached, 3);
    let fts = engine.fts.as_ref().unwrap();
    assert_eq!(fts.build_id_map_cached(&engine.ref_name).len(), 3);
    // FTS was already populated — never clobbered by warm.
    assert!(!w.fts_rebuilt);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_sqlite_self_heals_cold_cache() {
    let dir = unique_tempdir("heal");
    let db = dir.join(".asd-state.db");
    {
        let engine = Engine::open_sqlite(&db).expect("open fresh");
        seed_git_only(&engine);
        // Simulate the born-cold state: symbols exist in git but no sync ever
        // ran (hydrate-created DB, or a SQLITE_BUSY sync failure).
        assert!(
            !engine
                .fts
                .as_ref()
                .unwrap()
                .symbols_cached_for(&engine.ref_name)
        );
    }

    // Reopen: the self-heal in open_sqlite must warm the caches.
    let engine = Engine::open_sqlite(&db).expect("reopen");
    let fts = engine.fts.as_ref().unwrap();
    assert!(
        fts.symbols_cached_for(&engine.ref_name),
        "open_sqlite must self-heal a cold cache when git has symbols"
    );
    assert_eq!(fts.build_id_map_cached(&engine.ref_name).len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_sqlite_on_truly_empty_db_does_not_create_cache_rows() {
    let dir = unique_tempdir("empty");
    let db = dir.join(".asd-state.db");
    {
        Engine::open_sqlite(&db).expect("open fresh");
    }
    let engine = Engine::open_sqlite(&db).expect("reopen");
    assert!(
        !engine
            .fts
            .as_ref()
            .unwrap()
            .symbols_cached_for(&engine.ref_name),
        "no symbols in git → nothing to heal"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn warm_caches_skips_in_memory_engine() {
    let engine = Engine::open_in_memory().expect("in-memory");
    let w = engine.warm_caches().expect("warm on in-memory");
    assert!(w.skipped);
    assert_eq!(w.symbols_cached, 0);
}
