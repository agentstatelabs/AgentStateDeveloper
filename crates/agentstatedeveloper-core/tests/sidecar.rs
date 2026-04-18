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
    hydrate_from_dir, sync_to_dir, AsgEffectStore, AsgIndexStore, AsgLedgerStore, Author,
    AuthorKind, Effect, EffectCategory, EffectDecl, EffectStore, Engine, IndexStore, LedgerEntry,
    LedgerKind, LedgerStore, Position, Symbol, SymbolKind, Verification, VerificationSource,
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
    };
    let index = AsgIndexStore { repo: &e.repo };
    index
        .put_symbol(&e.ref_name, &symbol, "test-agent")
        .expect("put symbol");

    let decl = EffectDecl {
        symbol_id: symbol.symbol_id.clone(),
        declared: vec![Effect {
            effect: EffectCategory::IoNetOut,
            qualifiers: serde_json::Value::Null,
            note: Some("calls Stripe API".to_string()),
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
    let ledger = AsgLedgerStore { repo: &e.repo };
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

    let index = AsgIndexStore { repo: &dst.repo };
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

    let ledger = AsgLedgerStore { repo: &dst.repo };
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
