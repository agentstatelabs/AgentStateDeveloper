//! Plan J t-010: Lock M24's ledger-anchor behavior on
//! `find_candidates`.
//!
//! The M24 work added an anchoring pass that injects symbols
//! whose **ledger entries** (Invariant or Hazard) mention a query
//! token, EVEN IF the symbol's name/doc/file don't. This catches
//! cases like:
//!   - query "idempotent" → returns `charge()` because its
//!     invariant says "must be idempotent across retries", even
//!     though "idempotent" appears nowhere in the symbol itself.
//!
//! Two paths are exercised so regressions in either are caught:
//!   1. **FTS fast path** — anchor_candidates SQL over the
//!      denormalized ledger_text/ledger_flags columns.
//!   2. **Git fallback path** — walks /asd/v1/ledger directly when
//!      FTS is empty or has no data.
//!
//! The fixture is constructed so the anchored symbol cannot
//! surface via the FTS BM25 search itself — the qname, file path,
//! and signature/doc share no tokens with the query. The only
//! signal that can pull it in is the ledger anchor.

use std::collections::HashMap;
use std::path::Path;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, FtsFilters, IndexStore, LedgerEntry,
    LedgerKind, LedgerStore, Position, SearchFtsDb, Symbol, SymbolKind, find_candidates,
    query_tokens,
};

fn mk_sym(sym_id: &str, qname: &str, file: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: format!("fp-{sym_id}"),
        qname: qname.into(),
        language: "python".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some("def charge()".into()),
        doc: Some("Charges the customer for an order.".into()),
    }
}

fn seed_anchor_fixture(db: &Path) -> Vec<Symbol> {
    // One target symbol — `billing.payment.charge`. Its qname,
    // file, signature, and doc contain NONE of the query tokens
    // we'll use ("idempotent"). The ONLY way it can surface is
    // via the ledger-anchor pass over its Invariant entry.
    let engine = Engine::open_sqlite(db).expect("open");
    let idx = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);

    let target = mk_sym(
        "sym_charge",
        "billing.payment.charge",
        "src/billing/payment.py",
    );
    idx.put_symbol(&engine.ref_name, &target, "t").unwrap();

    // A second symbol that DOES contain the query token — so we
    // can also assert the anchor's relative score (it should
    // surface as ANCHOR_SCORE = 0.5, below an obvious BM25 hit).
    let bystander = Symbol {
        signature: Some("def idempotent_decorator()".into()),
        doc: Some("Decorator for idempotent operations.".into()),
        ..mk_sym(
            "sym_decorator",
            "utils.decorators.idempotent_decorator",
            "src/utils/decorators.py",
        )
    };
    idx.put_symbol(&engine.ref_name, &bystander, "t").unwrap();

    // The Invariant ledger entry whose summary contains
    // "idempotent" — the anchor signal.
    let alice = Author {
        kind: AuthorKind::Human,
        id: "alice".into(),
    };
    let mut inv = LedgerEntry::new(
        "sym_charge",
        LedgerKind::Invariant,
        "must be idempotent across retries",
        alice.clone(),
    );
    inv.entry_id = "led_inv_charge".into();
    ledger
        .append_entry(&engine.ref_name, &inv, "alice")
        .unwrap();

    // Also add a Concept entry on the same symbol — its summary
    // also contains the token, but the anchor pass should IGNORE
    // it (only Invariant/Hazard kinds are anchored). If we
    // accidentally start anchoring Concept entries, this test
    // wouldn't fail on its own — see negative_kind_filter test
    // below for the explicit guard.
    let mut concept = LedgerEntry::new(
        "sym_charge",
        LedgerKind::Concept,
        "this concept also says idempotent but should not anchor",
        alice,
    );
    concept.entry_id = "led_con_charge".into();
    ledger
        .append_entry(&engine.ref_name, &concept, "alice")
        .unwrap();

    vec![target, bystander]
}

fn run_find(db: &Path, query: &str) -> Vec<(f64, String)> {
    let engine = Engine::open_sqlite(db).expect("open for find");
    let ledger = AsgLedgerStore::from_engine(&engine);
    let idx = AsgIndexStore::from_engine(&engine);
    let tokens = query_tokens(query);
    let filters = FtsFilters {
        kind: None,
        language: None,
        include_tests: false,
        tests_only: false,
        exclude_terms: vec![],
        paths_filter: vec![],
        exclude_paths: vec![],
        exclude_languages: vec![],
    };
    find_candidates(&engine, query, &tokens, &filters, &ledger, &idx, 20)
}

#[test]
fn anchor_pass_surfaces_invariant_match_via_git_fallback() {
    // FTS NOT rebuilt → has_data() == false → ledger_anchor_pass
    // takes the git-fallback branch that walks /asd/v1/ledger.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_anchor_fixture(&db);
    // Deliberately skip FTS rebuild here.

    let candidates = run_find(&db, "idempotent");
    let qnames: Vec<&str> = candidates.iter().map(|(_, q)| q.as_str()).collect();
    assert!(
        qnames.contains(&"billing.payment.charge"),
        "anchor pass must surface billing.payment.charge via its invariant; got: {qnames:?}"
    );
}

#[test]
fn anchor_pass_surfaces_invariant_match_via_fts_fast_path() {
    // FTS rebuilt with ledger_text populated → anchor SQL runs
    // and short-circuits before the git fallback.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let symbols = seed_anchor_fixture(&db);

    // Populate FTS with ledger_text/ledger_flags so the fast-path
    // SQL can match. Match the shape produced by index_pipeline:
    //   ledger_text = lowercased summaries concatenated by space
    //   ledger_flags = comma-separated kinds
    let mut ledger_data: HashMap<String, (String, String)> = HashMap::new();
    ledger_data.insert(
        "sym_charge".into(),
        (
            "must be idempotent across retries this concept also says idempotent but should not anchor".into(),
            "invariant,concept".into(),
        ),
    );
    let fts = SearchFtsDb::open(&db).unwrap();
    fts.rebuild_refs(&symbols.iter().collect::<Vec<_>>(), &ledger_data)
        .expect("rebuild fts");

    let candidates = run_find(&db, "idempotent");
    let qnames: Vec<&str> = candidates.iter().map(|(_, q)| q.as_str()).collect();
    assert!(
        qnames.contains(&"billing.payment.charge"),
        "anchor pass (FTS fast path) must surface billing.payment.charge; got: {qnames:?}"
    );
}

#[test]
fn anchor_pass_only_fires_for_invariant_and_hazard_kinds() {
    // Negative guard for the FTS fast path: seed a symbol whose
    // ONLY ledger entry is a Concept matching the query. The
    // anchor SQL filter (`ledger_flags LIKE '%invariant%' OR
    // %hazard%`) must skip it. Plus a bystander symbol so FTS
    // has_data() is true and the test exercises the FTS path,
    // NOT the in-memory fallback (which uses substring matching
    // across ALL ledger kinds and would surface the Concept
    // entry by design — different code path, not what this test
    // is locking).
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);

    let concept_sym = mk_sym(
        "sym_unrelated",
        "billing.payment.charge",
        "src/billing/payment.py",
    );
    idx.put_symbol(&engine.ref_name, &concept_sym, "t").unwrap();

    let mut concept = LedgerEntry::new(
        "sym_unrelated",
        LedgerKind::Concept,
        "concept says idempotent but kind is wrong for anchor",
        Author {
            kind: AuthorKind::Human,
            id: "alice".into(),
        },
    );
    concept.entry_id = "led_con_only".into();
    ledger
        .append_entry(&engine.ref_name, &concept, "alice")
        .unwrap();

    // Bystander: must NOT contain the query token anywhere — its
    // job is solely to make FTS has_data() true so find_candidates
    // takes the FTS path. If it contained the token, the FTS BM25
    // search would surface it and the assertion about
    // billing.payment.charge would be the only signal under test
    // (good — that's what we want).
    let bystander = mk_sym(
        "sym_bystander",
        "unrelated.module.helper",
        "src/unrelated/helper.py",
    );
    idx.put_symbol(&engine.ref_name, &bystander, "t").unwrap();

    // Populate FTS with both symbols; ledger_flags="concept" for
    // the target so the anchor SQL's invariant/hazard filter
    // skips it. ledger_text DOES contain "idempotent" so any
    // other text-only match would still trigger — only the kind
    // filter prevents anchoring.
    let mut ledger_data: HashMap<String, (String, String)> = HashMap::new();
    ledger_data.insert(
        "sym_unrelated".into(),
        (
            "concept says idempotent but kind is wrong for anchor".into(),
            "concept".into(),
        ),
    );
    let fts = SearchFtsDb::open(&db).unwrap();
    fts.rebuild_refs(&[&concept_sym, &bystander], &ledger_data)
        .expect("rebuild");

    let candidates = run_find(&db, "idempotent");
    let qnames: Vec<&str> = candidates.iter().map(|(_, q)| q.as_str()).collect();
    assert!(
        !qnames.contains(&"billing.payment.charge"),
        "Concept-only kind must NOT anchor (FTS path); got: {qnames:?}"
    );
}

#[test]
fn anchor_pass_does_not_duplicate_symbols_already_in_results() {
    // The anchor pass uses existing_qnames to dedupe — if the
    // BM25 search already returned a symbol, the anchor must NOT
    // push a second copy. Lock that or downstream callers see
    // two entries for the same qname with two scores.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let symbols = seed_anchor_fixture(&db);
    // Populate FTS so BM25 surfaces sym_decorator (whose
    // signature contains "idempotent_decorator") AND the anchor
    // pass would normally try to add sym_charge.
    let mut ledger_data: HashMap<String, (String, String)> = HashMap::new();
    ledger_data.insert(
        "sym_charge".into(),
        (
            "must be idempotent across retries".into(),
            "invariant".into(),
        ),
    );
    let fts = SearchFtsDb::open(&db).unwrap();
    fts.rebuild_refs(&symbols.iter().collect::<Vec<_>>(), &ledger_data)
        .expect("rebuild");

    let candidates = run_find(&db, "idempotent");
    let charge_count = candidates
        .iter()
        .filter(|(_, q)| q == "billing.payment.charge")
        .count();
    assert!(
        charge_count <= 1,
        "anchor must not duplicate existing qname; saw billing.payment.charge {charge_count}× in {candidates:?}"
    );
}
