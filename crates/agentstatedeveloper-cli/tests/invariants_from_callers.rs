//! Plan J t-001: invariants attached to direct callers of a query
//! candidate must surface in `prepare_change.design_invariants`.
//!
//! Scenario:
//!   - `billing.payment.charge` is the candidate the agent is editing.
//!   - `billing.workflow.process` calls charge() and has an invariant
//!     attached: "must call charge exactly once per order".
//!   - The agent's prepare_change query for the charge work should
//!     surface the workflow's invariant, tagged `from_caller: true`,
//!     so the agent doesn't break it by accident.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn put_sym(engine: &Engine, sym_id: &str, qname: &str, file: &str) -> String {
    let sym = Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: "fp".into(),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!(
            "def {}()",
            qname.rsplit('.').next().unwrap_or(qname)
        )),
        doc: Some(format!("Function {qname}")),
    };
    AsgIndexStore::from_engine(engine)
        .put_symbol(&engine.ref_name, &sym, "t")
        .unwrap();
    sym_id.into()
}

fn seed_engine_with_caller_invariant(db_path: &Path) {
    use agentstategraph::CommitOptions;
    use agentstategraph_core::IntentCategory;

    let engine = Engine::open_sqlite(db_path).expect("open");
    // Two symbols + one call edge: process → charge
    let charge_id = put_sym(
        &engine,
        "sym_charge",
        "billing.payment.charge",
        "src/billing/payment.py",
    );
    // Caller is named to share NO tokens with the query "charge
    // payment" or the candidate's qname, so it can't itself become
    // a candidate via FTS. The propagation path is the only way its
    // invariant should surface.
    let process_id = put_sym(
        &engine,
        "sym_process",
        "unrelated.orchestrator.runOnce",
        "src/orchestrator/runner.py",
    );
    // Wire the call edge directly at the ASG paths (no public
    // put_call_edges API — index pipeline materializes these via
    // run_index, but tests can write them directly).
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &format!("/asd/v1/index/callers/{}", charge_id),
            &serde_json::json!({ "callers": [process_id.clone()] }),
            CommitOptions::new("t", IntentCategory::Refine, "seed callers".to_string()),
        )
        .unwrap();
    engine
        .repo
        .set_json(
            &engine.ref_name,
            &format!("/asd/v1/index/callees/{}", process_id),
            &serde_json::json!({ "callees": [charge_id.clone()] }),
            CommitOptions::new("t", IntentCategory::Refine, "seed callees".to_string()),
        )
        .unwrap();

    // Invariant attached to the CALLER (process), not the candidate.
    let ledger = AsgLedgerStore::from_engine(&engine);
    // Invariant text is intentionally lexically disjoint from the
    // test query ("charge payment") and from the candidate qname
    // ("billing.payment.charge"). That way `ledger_anchor_pass`
    // doesn't pull the caller in as a candidate via token match —
    // the only way this invariant can surface is the Plan J t-001
    // caller-walk path under test.
    let mut inv = LedgerEntry::new(
        &process_id,
        LedgerKind::Invariant,
        "wrapper guarantees idempotency across retries",
        Author {
            kind: AuthorKind::Human,
            id: "alice".into(),
        },
    );
    inv.entry_id = "led_inv_process".into();
    ledger
        .append_entry(&engine.ref_name, &inv, "alice")
        .unwrap();
}

fn run_prepare_change(db: &Path, description: &str) -> serde_json::Value {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db)
        .arg("prepare-change")
        .arg(description)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "prepare-change failed:\nstderr={}",
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
fn invariant_on_direct_caller_surfaces_with_from_caller_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_engine_with_caller_invariant(&db);

    // Query the CANDIDATE (charge) — caller's invariant must surface.
    let v = run_prepare_change(&db, "charge payment");

    let invs = v["design_invariants"]
        .as_array()
        .expect("design_invariants array");
    let from_caller_invs: Vec<&serde_json::Value> = invs
        .iter()
        .filter(|i| i["from_caller"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        from_caller_invs.len(),
        1,
        "expected exactly one caller-sourced invariant; got: {invs:#?}"
    );
    let inv = from_caller_invs[0];
    assert_eq!(
        inv["source"].as_str(),
        Some("unrelated.orchestrator.runOnce"),
        "source must name the caller, not the candidate; got: {inv:#?}"
    );
    assert!(
        inv["summary"]
            .as_str()
            .map(|s| s.contains("idempotency"))
            .unwrap_or(false),
        "summary preserved: {inv:#?}"
    );
}
