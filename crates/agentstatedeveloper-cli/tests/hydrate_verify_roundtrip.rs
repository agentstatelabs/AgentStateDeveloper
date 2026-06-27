//! Plan J t-008: Live `asd hydrate --verify` regression test.
//!
//! Why it exists: between M22 and M24 the sync ↔ hydrate ↔ verify
//! triangle quietly drifted twice — once when a new ledger kind
//! was added but skipped in sync's whitelist, once when hydrate
//! started writing to a different ASG path than sync read from.
//! Both bugs left `verify` reporting "ok" because the counts
//! happened to match (lossy in both directions cancels out at
//! the count level for small fixtures).
//!
//! This test creates a non-trivial fixture (symbols + invariants +
//! hazards + ownership + effects), syncs it, opens a FRESH empty
//! engine in a new tempdir, hydrates from the synced sidecar, and
//! finally runs `asd hydrate --verify` to assert verify.ok=true
//! with no discrepancies AND that the counts the verifier saw
//! match what the fixture put in. Belt + suspenders: if either
//! sync or hydrate silently drops something, the count assertion
//! catches what verify might not.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Effect, EffectCategory,
    EffectDecl, EffectStore, Engine, IndexStore, LedgerEntry, LedgerKind, LedgerStore, Position,
    Symbol, SymbolKind, sync_to_dir,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn mk_sym(sym_id: &str, qname: &str, file: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: format!("fp-{sym_id}"),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 10, col: 0 },
        signature: Some(format!("def {}()", qname.rsplit('.').next().unwrap_or(qname))),
        doc: Some(format!("Function {qname}")),
    }
}

fn seed_source_engine(db: &Path) -> (usize, usize, usize, usize) {
    // Returns (symbols, ledger_entries, invariants, effects) so the
    // test can assert the post-hydrate counts match the source.
    let engine = Engine::open_sqlite(db).expect("open source");
    let idx = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let effects = AsgEffectStore::from_engine(&engine);

    // 4 symbols, varied paths/qnames so the index_pipeline tree
    // shape is non-trivial.
    let syms = [
        mk_sym("sym_a", "billing.payment.charge", "src/billing/payment.py"),
        mk_sym("sym_b", "billing.workflow.process", "src/billing/workflow.py"),
        mk_sym("sym_c", "catalog.pricing.discount", "src/catalog/pricing.py"),
        mk_sym("sym_d", "ui.session.ExampleFlowView", "app/components/ExampleFlowView.swift"),
    ];
    for s in &syms {
        idx.put_symbol(&engine.ref_name, s, "t").unwrap();
    }

    // 4 ledger entries across 3 symbols — 1 invariant, 1 hazard,
    // 1 ownership, 1 concept. Exercises the multi-kind path that
    // historically had a kind-whitelist drift bug.
    let alice = Author { kind: AuthorKind::Human, id: "alice".into() };
    let entries = [
        ("sym_a", LedgerKind::Invariant,
         "charge() must be idempotent across retries", "led_inv_a"),
        ("sym_b", LedgerKind::Hazard,
         "process() may double-charge on partial retry", "led_haz_b"),
        ("sym_c", LedgerKind::Ownership,
         "owned by pricing team", "led_own_c"),
        ("sym_a", LedgerKind::Concept,
         "single point of money movement", "led_con_a"),
    ];
    for (sid, kind, summary, eid) in entries {
        let mut e = LedgerEntry::new(sid, kind, summary, alice.clone());
        e.entry_id = eid.into();
        ledger.append_entry(&engine.ref_name, &e, "alice").unwrap();
    }

    // 2 effects on 2 symbols — exercise the EffectCategory
    // round-trip (multiple categories per declaration on sym_a).
    let eff_a = EffectDecl {
        symbol_id: "sym_a".into(),
        declared: vec![
            Effect::new(EffectCategory::IoNetOut),
            Effect::new(EffectCategory::IoDbWrite),
        ],
        transitive: vec![],
        verification: None,
        confidence: None,
        runtime: None,
        matched_policy: None,
    };
    let eff_b = EffectDecl {
        symbol_id: "sym_b".into(),
        declared: vec![Effect::new(EffectCategory::StateGlobalWrite)],
        transitive: vec![],
        verification: None,
        confidence: None,
        runtime: None,
        matched_policy: None,
    };
    effects.put_effects(&engine.ref_name, "sym_a", &eff_a, "alice").unwrap();
    effects.put_effects(&engine.ref_name, "sym_b", &eff_b, "alice").unwrap();

    (syms.len(), entries.len(), 1, 2) // 1 invariant of the 4 entries; 2 effect symbols
}

fn run_hydrate_verify(db: &Path, project_root: &Path) -> serde_json::Value {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db)
        .arg("hydrate")
        .arg("--dir")
        .arg(project_root)
        .arg("--verify")
        .output()
        .expect("spawn hydrate");
    assert!(
        out.status.success(),
        "hydrate --verify exited non-zero — verify discrepancies likely.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("non-JSON hydrate stdout: {e}\n{}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn hydrate_verify_roundtrip_zero_discrepancies() {
    // --- Setup: source repo with non-trivial state -------------
    let src_tmp = tempfile::tempdir().unwrap();
    let src_db = src_tmp.path().join(".asd-state.db");
    let (n_syms, n_ledger, n_invariants, n_effects) = seed_source_engine(&src_db);

    // --- Sync to sidecar -----------------------------------------
    // sync_to_dir writes to `<dir>/.asd/v1/`. Point it at the
    // tempdir root so the layout is what hydrate expects.
    let src_engine = Engine::open_sqlite(&src_db).unwrap();
    let sync_summary =
        sync_to_dir(&src_engine.repo, &src_engine.ref_name, src_tmp.path())
            .expect("sync to sidecar");
    assert!(
        sync_summary.symbols_written >= n_syms,
        "sync should write all {n_syms} symbols; got summary: {sync_summary:#?}",
    );

    // --- Hydrate into a FRESH engine ------------------------------
    // New tempdir, new db file — no in-process state can leak;
    // hydrate must reconstruct everything from the sidecar alone.
    let dst_tmp = tempfile::tempdir().unwrap();
    let dst_db = dst_tmp.path().join(".asd-state.db");
    {
        // Touch the db file then drop the engine so the CLI
        // subprocess can open it without sqlite contention.
        let _e = Engine::open_sqlite(&dst_db).unwrap();
    }

    let v = run_hydrate_verify(&dst_db, src_tmp.path());

    // --- verify.ok must be true, with zero discrepancies ---------
    let verify = &v["verify"];
    assert_eq!(
        verify["ok"].as_bool(),
        Some(true),
        "verify.ok must be true after a clean round-trip; got: {v:#?}"
    );
    let discrepancies = verify["discrepancies"]
        .as_array()
        .expect("discrepancies array");
    assert!(
        discrepancies.is_empty(),
        "expected zero discrepancies; got: {discrepancies:#?}"
    );

    // --- Belt-and-suspenders: counts independently verified -----
    // verify.ok can match even if BOTH sides drop the same field.
    // Cross-check the absolute numbers against the fixture.
    assert_eq!(
        verify["symbols_actual"].as_u64(),
        Some(n_syms as u64),
        "symbols_actual must equal fixture: {v:#?}"
    );
    assert_eq!(
        verify["ledger_entries_actual"].as_u64(),
        Some(n_ledger as u64),
        "ledger_entries_actual must equal fixture: {v:#?}"
    );
    assert_eq!(
        verify["invariants_actual"].as_u64(),
        Some(n_invariants as u64),
        "invariants_actual must equal fixture (1): {v:#?}"
    );
    assert_eq!(
        verify["effects_actual"].as_u64(),
        Some(n_effects as u64),
        "effects_actual must equal fixture (2): {v:#?}"
    );
}

#[test]
fn hydrate_verify_exits_nonzero_on_missing_sidecar() {
    // Negative: hydrate with no `.asd/v1/` present should error
    // (verify can't run on nothing). CI catches the case where a
    // fresh-clone bootstrap forgets to ship the sidecar.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let _ = Engine::open_sqlite(&db).unwrap();

    let empty = tempfile::tempdir().unwrap();
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(&db)
        .arg("hydrate")
        .arg("--dir")
        .arg(empty.path())
        .arg("--verify")
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "hydrate --verify on an empty dir must fail; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
}
