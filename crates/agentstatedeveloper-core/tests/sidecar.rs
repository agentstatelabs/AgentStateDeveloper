//! Round-trip tests for the `.asd/v1/` on-disk sidecar.
//!
//! Covers:
//! - sync writes the expected file layout
//! - hydrate into a separate engine reproduces the same symbols,
//!   effects, and ledger entries visible via the stores
//! - re-syncing is idempotent (same file set, same contents)

use std::collections::HashSet;
use std::path::PathBuf;

use agentstatedeveloper_core::{
    hydrate_from_dir, paths, sync_to_dir, AsgEffectStore, AsgIndexStore, AsgLedgerStore, Author,
    AuthorKind, Effect, EffectCategory, EffectDecl, EffectStore, Engine, IndexStore, LedgerEntry,
    LedgerKind, LedgerStore, Position, Rebind, Symbol, SymbolKind, Verification, VerificationSource,
    VerificationStatus, ASD_SCHEMA_VERSION,
};
use chrono::Utc;

fn unique_tempdir(tag: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("asd-sidecar-{tag}-{id}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn seed_engine(e: &Engine) -> (Symbol, EffectDecl, LedgerEntry) {
    let symbol = Symbol {
        symbol_id: "sym_fn_payments_charge_card".to_string(),
        symbol_fp: "fp_0001".to_string(),
        qname: "payments.charge_card".to_string(),
        language: "python".to_string(),
        kind: SymbolKind::Function,
        file: "payments.py".to_string(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 10, col: 0 },
        signature: Some("def charge_card(amount: int) -> bool".to_string()),
        doc: None,
    };
    let index = AsgIndexStore::new(&e.repo);
    index
        .put_symbol(&e.ref_name, &symbol, "test-agent")
        .expect("put symbol");

    let decl = EffectDecl {
        symbol_id: symbol.symbol_id.clone(),
        declared: vec![Effect {
            effect: EffectCategory::IoNetOut,
            qualifiers: serde_json::Value::Null,
            note: Some("calls Stripe API".to_string()),
            ..Default::default()
        }],
        transitive: Vec::new(),
        verification: Some(Verification {
            by: VerificationSource::StaticChecker,
            at: Utc::now(),
            status: VerificationStatus::Unverified,
            mismatches: Vec::new(),
        }),
        confidence: Some(0.9),
        matched_policy: None,
    };
    let effects = AsgEffectStore { repo: &e.repo };
    effects
        .put_effects(&e.ref_name, &symbol.symbol_id, &decl, "test-agent")
        .expect("put effects");

    let mut entry = LedgerEntry::new(
        &symbol.symbol_id,
        LedgerKind::Hazard,
        "needs review",
        Author {
            kind: AuthorKind::Human,
            id: "alice".to_string(),
        },
    );
    entry.tags.push("awaiting-approval".to_string());
    let ledger = AsgLedgerStore::new(&e.repo);
    ledger
        .append_entry(&e.ref_name, &entry, "test-agent")
        .expect("append entry");

    (symbol, decl, entry)
}

#[test]
fn sync_writes_expected_files() {
    let engine = Engine::open_in_memory().expect("open engine");
    let (symbol, _decl, entry) = seed_engine(&engine);
    let tmp = unique_tempdir("sync-files");

    let summary = sync_to_dir(&engine.repo, &engine.ref_name, &tmp).expect("sync");
    assert_eq!(summary.effects_written, 1);
    assert_eq!(summary.ledger_entries_written, 1);
    assert_eq!(summary.symbols_written, 1);
    assert_eq!(summary.schema_version, ASD_SCHEMA_VERSION);

    let root = tmp.join(".asd/v1");
    assert!(root.join("meta/schema-version").is_file(), "schema-version");
    assert!(
        root.join(format!("effects/{}.json", symbol.symbol_id)).is_file(),
        "effects file"
    );
    assert!(
        root.join(format!("ledger/{}/{}.json", symbol.symbol_id, entry.entry_id))
            .is_file(),
        "ledger entry file"
    );
    assert!(
        root.join(format!("symbols/{}.json", symbol.qname)).is_file(),
        "symbols file"
    );

    // Schema version file is plain text, not JSON.
    let sv = std::fs::read_to_string(root.join("meta/schema-version")).unwrap();
    assert!(sv.trim() == ASD_SCHEMA_VERSION, "schema version = {sv:?}");
}

#[test]
fn hydrate_into_fresh_engine_reproduces_state() {
    let src = Engine::open_in_memory().expect("open src");
    let (symbol, decl, entry) = seed_engine(&src);
    let tmp = unique_tempdir("hydrate");
    sync_to_dir(&src.repo, &src.ref_name, &tmp).expect("sync");

    // Second, independent engine.
    let dst = Engine::open_in_memory().expect("open dst");
    let summary =
        hydrate_from_dir(&dst.repo, &dst.ref_name, &tmp, "hydrate-agent").expect("hydrate");
    assert_eq!(summary.symbols_loaded, 1);
    assert_eq!(summary.effects_loaded, 1);
    assert_eq!(summary.ledger_entries_loaded, 1);
    assert!(!summary.missing_schema_version);

    let index = AsgIndexStore::new(&dst.repo);
    let got_sym = index
        .get_symbol_by_qname(&dst.ref_name, &symbol.qname)
        .expect("lookup")
        .expect("symbol present after hydrate");
    assert_eq!(got_sym.symbol_id, symbol.symbol_id);
    assert_eq!(got_sym.signature, symbol.signature);

    let effects = AsgEffectStore { repo: &dst.repo };
    let got_decl = effects
        .get_effects(&dst.ref_name, &symbol.symbol_id)
        .expect("get_effects")
        .expect("effects present after hydrate");
    assert_eq!(got_decl.declared.len(), decl.declared.len());
    assert_eq!(got_decl.declared[0].effect, EffectCategory::IoNetOut);
    assert_eq!(got_decl.confidence, decl.confidence);

    let ledger = AsgLedgerStore::new(&dst.repo);
    let got_entries = ledger
        .list_entries(&dst.ref_name, &symbol.symbol_id)
        .expect("list");
    assert_eq!(got_entries.len(), 1);
    assert_eq!(got_entries[0].entry_id, entry.entry_id);
    assert_eq!(got_entries[0].summary, "needs review");
    assert!(
        got_entries[0].tags.iter().any(|t| t == "awaiting-approval"),
        "approval tag survived roundtrip"
    );
}

#[test]
fn resync_is_idempotent() {
    let engine = Engine::open_in_memory().expect("open engine");
    seed_engine(&engine);
    let tmp = unique_tempdir("idempotent");

    sync_to_dir(&engine.repo, &engine.ref_name, &tmp).expect("sync 1");
    let files_1 = collect_json_files(&tmp);
    let bytes_1 = hash_dir(&tmp);

    sync_to_dir(&engine.repo, &engine.ref_name, &tmp).expect("sync 2");
    let files_2 = collect_json_files(&tmp);
    let bytes_2 = hash_dir(&tmp);

    assert_eq!(files_1, files_2, "file set identical across syncs");
    assert_eq!(bytes_1, bytes_2, "file contents identical across syncs");
}

#[test]
fn invariant_survives_hydrate_roundtrip() {
    // Regression guard: `asd invariant add` stores LedgerKind::Invariant entries.
    // Verify they survive a sync → wipe → hydrate cycle so agents see them on a
    // fresh clone without rerunning `asd index`.
    let src = Engine::open_in_memory().expect("open src");
    let index = AsgIndexStore::new(&src.repo);
    let ledger = AsgLedgerStore::new(&src.repo);

    let sym = Symbol {
        symbol_id: "sym_refreshDriftPlayhead".to_string(),
        symbol_fp: "fp_inv_test".to_string(),
        qname: "ExampleFlowViewModel.refreshDriftPlayhead".to_string(),
        language: "swift".to_string(),
        kind: SymbolKind::Function,
        file: "ExampleFlow.swift".to_string(),
        start: Position { line: 42, col: 0 },
        end: Position { line: 55, col: 0 },
        signature: Some("func refreshDriftPlayhead()".to_string()),
        doc: None,
    };
    index.put_symbol(&src.ref_name, &sym, "test").expect("put symbol");

    let invariant = LedgerEntry::new(
        &sym.symbol_id,
        LedgerKind::Invariant,
        "playhead must always reflect the current model state",
        Author { kind: AuthorKind::Human, id: "craig".to_string() },
    );
    ledger.append_entry(&src.ref_name, &invariant, "test").expect("append invariant");

    // Sync to sidecar.
    let tmp = unique_tempdir("invariant-hydrate");
    let sync_summary = sync_to_dir(&src.repo, &src.ref_name, &tmp).expect("sync");
    assert_eq!(sync_summary.ledger_entries_written, 1, "invariant written to sidecar");

    // Hydrate into a fresh engine (simulates fresh clone).
    let dst = Engine::open_in_memory().expect("open dst");
    let hydrate_summary = hydrate_from_dir(&dst.repo, &dst.ref_name, &tmp, "test").expect("hydrate");
    assert_eq!(hydrate_summary.ledger_entries_loaded, 1, "invariant loaded from sidecar");

    // Verify the invariant is present and correct.
    let dst_ledger = AsgLedgerStore::new(&dst.repo);
    let entries = dst_ledger
        .list_entries(&dst.ref_name, &sym.symbol_id)
        .expect("list entries");
    assert_eq!(entries.len(), 1, "exactly one invariant");
    assert_eq!(entries[0].kind, LedgerKind::Invariant, "kind preserved");
    assert_eq!(entries[0].summary, "playhead must always reflect the current model state", "summary preserved");
    assert_eq!(entries[0].entry_id, invariant.entry_id, "entry_id stable");
    assert_eq!(entries[0].author.id, "craig", "author preserved");
}

#[test]
fn hydrate_errors_when_sidecar_missing() {
    let engine = Engine::open_in_memory().expect("open engine");
    let tmp = unique_tempdir("missing");
    // No sync was performed.
    let err = hydrate_from_dir(&engine.repo, &engine.ref_name, &tmp, "a")
        .expect_err("should error when sidecar absent");
    let msg = err.to_string();
    assert!(
        msg.contains("no sidecar found") && msg.contains("asd sync"),
        "error message should suggest running `asd sync`: got {msg}"
    );
}

/// Helper: write a rebind record directly into the repo (same as CLI/MCP).
fn write_rebind(engine: &Engine, from_id: &str, to_id: &str, to_qname: &str) {
    use agentstategraph::CommitOptions;
    use agentstategraph_core::IntentCategory;
    use chrono::Utc;

    let rebind = Rebind {
        from_symbol_id: from_id.to_string(),
        to_symbol_id: to_id.to_string(),
        to_qname: to_qname.to_string(),
        at: Utc::now(),
        by: "test-agent".to_string(),
    };
    engine.repo.set_json(
        &engine.ref_name,
        &paths::rebind_path(from_id),
        &serde_json::to_value(&rebind).unwrap(),
        CommitOptions::new("test", IntentCategory::Refine, "rebind"),
    ).expect("write rebind");
}

#[test]
fn sidecar_roundtrip_with_chained_rebinds() {
    // Set up: symbol A → B (rebind 1), then B → C (rebind 2).
    // After both rebinds, entries are under C. Verify sync+hydrate preserves
    // this: entries still under C, both rebind records restored.
    use agentstatedeveloper_core::{AsgIndexStore, IndexStore, Position, SymbolKind};

    let src = Engine::open_in_memory().expect("open src");
    let index = AsgIndexStore::new(&src.repo);
    let ledger = AsgLedgerStore::new(&src.repo);

    // Create three symbols.
    for (id, qname) in [("sym_a", "mod.fn_a"), ("sym_b", "mod.fn_b"), ("sym_c", "mod.fn_c")] {
        let sym = Symbol {
            symbol_id: id.to_string(),
            symbol_fp: format!("fp_{id}"),
            qname: qname.to_string(),
            language: "python".to_string(),
            kind: SymbolKind::Function,
            file: "mod.py".to_string(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&src.ref_name, &sym, "test").expect("put symbol");
    }

    // Append entry under A.
    let mut entry = LedgerEntry::new(
        "sym_a",
        LedgerKind::Decision,
        "original decision",
        Author { kind: AuthorKind::Human, id: "alice".to_string() },
    );
    ledger.append_entry(&src.ref_name, &entry, "test").expect("append entry");

    // Rebind A→B: move entry to B.
    {
        use agentstategraph::CommitOptions;
        use agentstategraph_core::IntentCategory;
        let entries = ledger.list_entries_with_superseded(&src.ref_name, "sym_a").unwrap();
        for mut e in entries {
            e.symbol_id = "sym_b".to_string();
            src.repo.set_json(&src.ref_name, &paths::ledger_entry_path("sym_b", &e.entry_id),
                &serde_json::to_value(&e).unwrap(),
                CommitOptions::new("test", IntentCategory::Refine, "reparent")).unwrap();
            let _ = src.repo.delete(&src.ref_name, &paths::ledger_entry_path("sym_a", &e.entry_id),
                CommitOptions::new("test", IntentCategory::Refine, "delete old")).unwrap();
        }
    }
    write_rebind(&src, "sym_a", "sym_b", "mod.fn_b");

    // Rebind B→C: move entry to C.
    {
        use agentstategraph::CommitOptions;
        use agentstategraph_core::IntentCategory;
        let entries = ledger.list_entries_with_superseded(&src.ref_name, "sym_b").unwrap();
        entry = entries.into_iter().next().expect("entry under B");
        let mut e2 = entry.clone();
        e2.symbol_id = "sym_c".to_string();
        src.repo.set_json(&src.ref_name, &paths::ledger_entry_path("sym_c", &e2.entry_id),
            &serde_json::to_value(&e2).unwrap(),
            CommitOptions::new("test", IntentCategory::Refine, "reparent")).unwrap();
        let _ = src.repo.delete(&src.ref_name, &paths::ledger_entry_path("sym_b", &e2.entry_id),
            CommitOptions::new("test", IntentCategory::Refine, "delete old")).unwrap();
    }
    write_rebind(&src, "sym_b", "sym_c", "mod.fn_c");

    // Sync.
    let tmp = unique_tempdir("chained-rebind");
    let sync_summary = sync_to_dir(&src.repo, &src.ref_name, &tmp).expect("sync");
    assert_eq!(sync_summary.rebinds_synced, 2, "two rebind records synced");
    assert!(tmp.join(".asd/v1/rebinds/sym_a.json").is_file(), "rebind A file");
    assert!(tmp.join(".asd/v1/rebinds/sym_b.json").is_file(), "rebind B file");

    // Hydrate into fresh engine.
    let dst = Engine::open_in_memory().expect("open dst");
    let hydrate_summary = hydrate_from_dir(&dst.repo, &dst.ref_name, &tmp, "test").expect("hydrate");
    assert_eq!(hydrate_summary.rebinds_replayed, 2, "two rebind records replayed");

    // Entry should be under C in the hydrated engine.
    let dst_ledger = AsgLedgerStore::new(&dst.repo);
    let entries_c = dst_ledger.list_entries(&dst.ref_name, "sym_c").expect("list C");
    assert_eq!(entries_c.len(), 1, "entry under C after hydrate");
    assert_eq!(entries_c[0].summary, "original decision");

    // Should be nothing under A or B.
    let entries_a = dst_ledger.list_entries(&dst.ref_name, "sym_a").expect("list A");
    let entries_b = dst_ledger.list_entries(&dst.ref_name, "sym_b").expect("list B");
    assert_eq!(entries_a.len(), 0, "no entries under A");
    assert_eq!(entries_b.len(), 0, "no entries under B");
}

fn collect_json_files(root: &std::path::Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let sidecar = root.join(".asd/v1");
    walk(&sidecar, &mut out);
    out
}

fn walk(dir: &std::path::Path, out: &mut HashSet<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.insert(p);
        }
    }
}

fn hash_dir(root: &std::path::Path) -> Vec<(PathBuf, u64)> {
    let mut files: Vec<PathBuf> = collect_json_files(root).into_iter().collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            // Cheap stable digest: length + byte sum. Good enough for
            // idempotence check — content-equality in a test.
            let sum: u64 = bytes.iter().map(|b| *b as u64).sum();
            let len = bytes.len() as u64;
            (p, sum.wrapping_mul(31).wrapping_add(len))
        })
        .collect()
}
