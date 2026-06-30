//! Plan J t-013: `asd impact` must surface ValidationScenario and
//! KnownBug entries alongside Invariants and Hazards. Seeds an
//! engine with one entry of each kind on a target symbol, runs
//! `asd impact`, and parses the JSON to assert all four arrays
//! are populated.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn seed_engine(db_path: &Path) {
    let engine = Engine::open_sqlite(db_path).expect("open sqlite");
    let sym = Symbol {
        symbol_id: "sym_payment".into(),
        symbol_fp: "fp".into(),
        qname: "billing.payment.charge".into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: "src/billing/payment.py".into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some("def charge(amount)".into()),
        doc: None,
    };
    AsgIndexStore::from_engine(&engine)
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();
    let ledger = AsgLedgerStore::from_engine(&engine);
    let append = |kind: LedgerKind, id: &str, summary: &str| {
        let mut e = LedgerEntry::new(
            "sym_payment",
            kind,
            summary,
            Author {
                kind: AuthorKind::Human,
                id: "alice".into(),
            },
        );
        e.entry_id = id.into();
        ledger.append_entry(&engine.ref_name, &e, "alice").unwrap();
    };
    append(
        LedgerKind::Invariant,
        "led_inv_1",
        "amount must be positive",
    );
    append(
        LedgerKind::Hazard,
        "led_haz_1",
        "rounding errors on subcents",
    );
    append(
        LedgerKind::ValidationScenario,
        "led_vs_1",
        "charge() rejects negative amount with InvalidAmount",
    );
    append(
        LedgerKind::KnownBug,
        "led_kb_1",
        "double-charge under retry race",
    );
}

fn run_impact(db_path: &Path, qname: &str) -> serde_json::Value {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db_path)
        .arg("impact")
        .arg(qname)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "asd impact failed:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "non-JSON stdout: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn impact_surfaces_validation_scenarios_alongside_invariants() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine(&db);

    let v = run_impact(&db, "billing.payment.charge");

    // Original blast-radius fields stay populated.
    let invs = v["invariants"].as_array().expect("invariants array");
    assert_eq!(invs.len(), 1, "expected 1 invariant; got: {invs:?}");
    let hazs = v["hazards"].as_array().expect("hazards array");
    assert_eq!(hazs.len(), 1, "expected 1 hazard; got: {hazs:?}");

    // Plan J t-013 additions.
    let vs = v["validation_scenarios"]
        .as_array()
        .expect("validation_scenarios array present");
    assert_eq!(vs.len(), 1, "expected 1 validation scenario; got: {vs:?}");
    assert!(
        vs[0]["summary"]
            .as_str()
            .map(|s| s.contains("InvalidAmount"))
            .unwrap_or(false),
        "scenario summary must be preserved; got: {vs:?}"
    );
    let kbs = v["known_bugs"]
        .as_array()
        .expect("known_bugs array present");
    assert_eq!(kbs.len(), 1, "expected 1 known bug; got: {kbs:?}");
    assert!(
        kbs[0]["summary"]
            .as_str()
            .map(|s| s.contains("double-charge"))
            .unwrap_or(false),
        "known bug summary must be preserved; got: {kbs:?}"
    );
}

#[test]
fn impact_returns_empty_arrays_when_no_entries_present() {
    // Regression guard: a symbol with no ledger entries must still
    // return the arrays as empty `[]`, not absent or `null`. Agents
    // shouldn't have to handle three different "no scenarios"
    // sentinels.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).unwrap();
    let sym = Symbol {
        symbol_id: "sym_bare".into(),
        symbol_fp: "fp".into(),
        qname: "pkg.bare".into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: "src/pkg/bare.py".into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 2, col: 0 },
        signature: None,
        doc: None,
    };
    AsgIndexStore::from_engine(&engine)
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();

    let v = run_impact(&db, "pkg.bare");
    for k in [
        "invariants",
        "hazards",
        "validation_scenarios",
        "known_bugs",
    ] {
        let arr = v[k]
            .as_array()
            .unwrap_or_else(|| panic!("`{k}` must be present as an array, even when empty"));
        assert!(
            arr.is_empty(),
            "`{k}` must be empty array for bare symbol; got {arr:?}"
        );
    }
}
