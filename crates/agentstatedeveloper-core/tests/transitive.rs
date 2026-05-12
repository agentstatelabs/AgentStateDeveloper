//! Tests for transitive effect propagation.
//!
//! Builds a tiny call graph in an in-memory ASG repo, runs
//! `propagate_transitive`, and asserts the resulting `EffectDecl.transitive`
//! sets and `via` chains.

use std::collections::HashSet;

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use serde_json::json;

use agentstatedeveloper_core::{
    paths, propagate_transitive, AsgEffectStore, AsgIndexStore, Effect, EffectCategory, EffectDecl,
    EffectStore, Engine,
};

/// Write a callees array under `paths::callees_path(symbol_id)`. Mirrors
/// what the call-graph indexer does for real, but bypasses any helper API
/// since we don't need full graph extraction here.
fn put_callees(engine: &Engine, symbol_id: &str, callees: &[&str]) {
    let path = paths::callees_path(symbol_id);
    let value = json!({ "callees": callees });
    let opts = CommitOptions::new(
        "test-agent",
        IntentCategory::Refine,
        format!("test callees for {}", symbol_id),
    );
    engine
        .repo
        .set_json(&engine.ref_name, &path, &value, opts)
        .expect("set callees");
}

fn put_simple_effect_decl(
    engine: &Engine,
    symbol_id: &str,
    declared: Vec<EffectCategory>,
) {
    let store = AsgEffectStore { repo: &engine.repo };
    let decl = EffectDecl {
        symbol_id: symbol_id.to_string(),
        declared: declared
            .into_iter()
            .map(|c| Effect {
                effect: c,
                qualifiers: serde_json::Value::Null,
                note: None,
                ..Default::default()
            })
            .collect(),
        transitive: Vec::new(),
        verification: None,
        confidence: None,
        matched_policy: None,
    };
    store
        .put_effects(&engine.ref_name, symbol_id, &decl, "test-agent")
        .expect("put effects");
}

#[test]
fn propagates_transitive_effects_with_via_chains() {
    let engine = Engine::open_in_memory().expect("open engine");
    let index_store = AsgIndexStore::new(&engine.repo);
    let effect_store = AsgEffectStore { repo: &engine.repo };

    // A declares io.fs.read; B declares io.net.out; C declares nothing.
    put_simple_effect_decl(&engine, "A", vec![EffectCategory::IoFsRead]);
    put_simple_effect_decl(&engine, "B", vec![EffectCategory::IoNetOut]);
    put_simple_effect_decl(&engine, "C", vec![]);

    // A -> B, C -> A, C -> B
    put_callees(&engine, "A", &["B"]);
    put_callees(&engine, "B", &[]);
    put_callees(&engine, "C", &["A", "B"]);

    let symbol_ids = vec!["A".to_string(), "B".to_string(), "C".to_string()];
    let updated = propagate_transitive(
        &index_store,
        &effect_store,
        &engine.ref_name,
        &symbol_ids,
    )
    .expect("propagate");

    // A and C gain transitive effects; B gains none.
    assert_eq!(updated, 2, "expected A and C to be updated, got {}", updated);

    // A: transitive should be [io.net.out via [B]].
    let a_decl = effect_store
        .get_effects(&engine.ref_name, "A")
        .expect("get A")
        .expect("A exists");
    assert_eq!(a_decl.transitive.len(), 1);
    assert_eq!(a_decl.transitive[0].effect, EffectCategory::IoNetOut);
    assert_eq!(a_decl.transitive[0].via, vec!["B".to_string()]);

    // B: no callees, no transitive.
    let b_decl = effect_store
        .get_effects(&engine.ref_name, "B")
        .expect("get B")
        .expect("B exists");
    assert!(
        b_decl.transitive.is_empty(),
        "B has no callees, transitive must be empty"
    );

    // C: should surface both io.fs.read (via A) and io.net.out (via A and/or B).
    let c_decl = effect_store
        .get_effects(&engine.ref_name, "C")
        .expect("get C")
        .expect("C exists");
    let cats: HashSet<EffectCategory> =
        c_decl.transitive.iter().map(|t| t.effect.clone()).collect();
    assert!(
        cats.contains(&EffectCategory::IoFsRead),
        "C should surface io.fs.read via A, got {:?}",
        c_decl.transitive
    );
    assert!(
        cats.contains(&EffectCategory::IoNetOut),
        "C should surface io.net.out, got {:?}",
        c_decl.transitive
    );

    // Verify the via chain for io.fs.read points at A.
    let fs_entry = c_decl
        .transitive
        .iter()
        .find(|t| t.effect == EffectCategory::IoFsRead)
        .unwrap();
    assert!(
        fs_entry.via.contains(&"A".to_string()),
        "io.fs.read should be reached via A, got via={:?}",
        fs_entry.via
    );

    // Verify io.net.out reaches via at least B (direct callee). It may also
    // surface via A (since B is a transitive callee of A), so we accept the
    // superset that includes B.
    let net_entry = c_decl
        .transitive
        .iter()
        .find(|t| t.effect == EffectCategory::IoNetOut)
        .unwrap();
    assert!(
        net_entry.via.contains(&"B".to_string()),
        "io.net.out should be reached via B, got via={:?}",
        net_entry.via
    );

    // Re-running should be idempotent: zero further updates.
    let again = propagate_transitive(
        &index_store,
        &effect_store,
        &engine.ref_name,
        &symbol_ids,
    )
    .expect("second propagate");
    assert_eq!(again, 0, "second run must not re-write unchanged decls");
}

#[test]
fn handles_cycles_without_infinite_loop() {
    let engine = Engine::open_in_memory().expect("open engine");
    let index_store = AsgIndexStore::new(&engine.repo);
    let effect_store = AsgEffectStore { repo: &engine.repo };

    put_simple_effect_decl(&engine, "A", vec![EffectCategory::IoFsRead]);
    put_simple_effect_decl(&engine, "B", vec![EffectCategory::IoNetOut]);

    // Mutual recursion: A -> B -> A.
    put_callees(&engine, "A", &["B"]);
    put_callees(&engine, "B", &["A"]);

    let symbol_ids = vec!["A".to_string(), "B".to_string()];
    let updated = propagate_transitive(
        &index_store,
        &effect_store,
        &engine.ref_name,
        &symbol_ids,
    )
    .expect("propagate must terminate on a cycle");

    // Both should have been updated with at least one transitive entry.
    assert_eq!(updated, 2);

    let a_decl = effect_store
        .get_effects(&engine.ref_name, "A")
        .unwrap()
        .unwrap();
    let b_decl = effect_store
        .get_effects(&engine.ref_name, "B")
        .unwrap()
        .unwrap();

    // A should pick up B's io.net.out through the direct callee edge.
    assert!(
        a_decl
            .transitive
            .iter()
            .any(|t| t.effect == EffectCategory::IoNetOut),
        "A should pick up io.net.out from B even with cycle, got {:?}",
        a_decl.transitive
    );
    // B should pick up A's io.fs.read symmetrically.
    assert!(
        b_decl
            .transitive
            .iter()
            .any(|t| t.effect == EffectCategory::IoFsRead),
        "B should pick up io.fs.read from A even with cycle, got {:?}",
        b_decl.transitive
    );
}
