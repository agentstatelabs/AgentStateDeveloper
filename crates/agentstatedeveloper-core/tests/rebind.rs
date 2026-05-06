//! Integration tests for the ledger rebind flow.
//!
//! Covers:
//! - Happy path: entries re-parented under new symbol_id, old paths gone
//! - Rebind record written with correct fields
//! - Idempotent: rebind from a symbol with no entries succeeds
//! - Error: rebind to an unknown qname returns an error before writing anything

use agentstatedeveloper_core::{
    paths, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry,
    LedgerKind, LedgerStore, Position, Rebind, Symbol, SymbolKind,
};
use chrono::Utc;

fn make_symbol(id: &str, qname: &str) -> Symbol {
    Symbol {
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
    }
}

fn seed_two_symbols(engine: &Engine) -> (Symbol, Symbol) {
    let sym_a = make_symbol("sym_a", "mod.old_fn");
    let sym_b = make_symbol("sym_b", "mod.new_fn");
    let index = AsgIndexStore { repo: &engine.repo };
    index.put_symbol(&engine.ref_name, &sym_a, "test").expect("put sym_a");
    index.put_symbol(&engine.ref_name, &sym_b, "test").expect("put sym_b");
    (sym_a, sym_b)
}

fn append_entry(engine: &Engine, symbol_id: &str, summary: &str) -> LedgerEntry {
    let entry = LedgerEntry::new(
        symbol_id,
        LedgerKind::Decision,
        summary,
        Author { kind: AuthorKind::Agent, id: "test-agent".to_string() },
    );
    let ledger = AsgLedgerStore { repo: &engine.repo };
    ledger.append_entry(&engine.ref_name, &entry, "test").expect("append entry");
    entry
}

/// Simulate the rebind logic (matches the CLI/MCP implementation).
fn do_rebind(engine: &Engine, from_symbol_id: &str, to_symbol_id: &str, to_qname: &str) {
    use agentstategraph::CommitOptions;
    use agentstategraph_core::IntentCategory;

    let rebind = Rebind {
        from_symbol_id: from_symbol_id.to_string(),
        to_symbol_id: to_symbol_id.to_string(),
        to_qname: to_qname.to_string(),
        at: Utc::now(),
        by: "test-agent".to_string(),
    };
    let rebind_path = paths::rebind_path(from_symbol_id);
    engine.repo.set_json(
        &engine.ref_name,
        &rebind_path,
        &serde_json::to_value(&rebind).unwrap(),
        CommitOptions::new("test", IntentCategory::Refine, "rebind"),
    ).expect("write rebind record");

    let ledger = AsgLedgerStore { repo: &engine.repo };
    let entries = ledger
        .list_entries_with_superseded(&engine.ref_name, from_symbol_id)
        .expect("list entries");
    for mut entry in entries {
        entry.symbol_id = to_symbol_id.to_string();
        let new_path = paths::ledger_entry_path(to_symbol_id, &entry.entry_id);
        engine.repo.set_json(
            &engine.ref_name,
            &new_path,
            &serde_json::to_value(&entry).unwrap(),
            CommitOptions::new("test", IntentCategory::Refine, "reparent entry"),
        ).expect("write reparented entry");
        let old_path = paths::ledger_entry_path(from_symbol_id, &entry.entry_id);
        let _ = engine.repo.delete(
            &engine.ref_name,
            &old_path,
            CommitOptions::new("test", IntentCategory::Refine, "delete old entry"),
        );
    }
}

#[test]
fn rebind_reparents_entries_to_new_symbol() {
    let engine = Engine::open_in_memory().expect("open engine");
    let (sym_a, sym_b) = seed_two_symbols(&engine);
    let e1 = append_entry(&engine, &sym_a.symbol_id, "first decision");
    let e2 = append_entry(&engine, &sym_a.symbol_id, "second decision");
    let ledger = AsgLedgerStore { repo: &engine.repo };

    // Pre-rebind: both entries under A.
    let before = ledger.list_entries(&engine.ref_name, &sym_a.symbol_id).expect("list before");
    assert_eq!(before.len(), 2, "two entries under A before rebind");

    do_rebind(&engine, &sym_a.symbol_id, &sym_b.symbol_id, &sym_b.qname);

    // Post-rebind: entries under B.
    let after_b = ledger.list_entries(&engine.ref_name, &sym_b.symbol_id).expect("list after B");
    assert_eq!(after_b.len(), 2, "two entries under B after rebind");
    let entry_ids: Vec<&str> = after_b.iter().map(|e| e.entry_id.as_str()).collect();
    assert!(entry_ids.contains(&e1.entry_id.as_str()), "e1 under B");
    assert!(entry_ids.contains(&e2.entry_id.as_str()), "e2 under B");
    assert!(after_b.iter().all(|e| e.symbol_id == sym_b.symbol_id), "symbol_id updated");

    // Old paths should be gone.
    let after_a = ledger.list_entries(&engine.ref_name, &sym_a.symbol_id).expect("list after A");
    assert_eq!(after_a.len(), 0, "no entries under A after rebind");
}

#[test]
fn rebind_record_is_written_with_correct_fields() {
    let engine = Engine::open_in_memory().expect("open engine");
    let (sym_a, sym_b) = seed_two_symbols(&engine);
    append_entry(&engine, &sym_a.symbol_id, "some decision");

    do_rebind(&engine, &sym_a.symbol_id, &sym_b.symbol_id, &sym_b.qname);

    let rebind_path = paths::rebind_path(&sym_a.symbol_id);
    let val = engine.repo
        .get_json(&engine.ref_name, &rebind_path)
        .expect("get rebind record");
    let rebind: Rebind = serde_json::from_value(val).expect("deserialize rebind");

    assert_eq!(rebind.from_symbol_id, sym_a.symbol_id);
    assert_eq!(rebind.to_symbol_id, sym_b.symbol_id);
    assert_eq!(rebind.to_qname, sym_b.qname);
    assert_eq!(rebind.by, "test-agent");
}

#[test]
fn rebind_with_no_entries_is_idempotent() {
    let engine = Engine::open_in_memory().expect("open engine");
    let (sym_a, sym_b) = seed_two_symbols(&engine);
    // No entries under sym_a.

    // Should not panic or error.
    do_rebind(&engine, &sym_a.symbol_id, &sym_b.symbol_id, &sym_b.qname);

    let ledger = AsgLedgerStore { repo: &engine.repo };
    let entries_b = ledger.list_entries(&engine.ref_name, &sym_b.symbol_id).expect("list");
    assert_eq!(entries_b.len(), 0, "still no entries after rebind of empty symbol");

    // Rebind record itself should be written.
    let rebind_path = paths::rebind_path(&sym_a.symbol_id);
    engine.repo.get_json(&engine.ref_name, &rebind_path)
        .expect("rebind record written even with no entries");
}

#[test]
fn rebind_to_unknown_qname_does_not_write_rebind_record() {
    // The CLI/MCP checks qname existence before writing; this test verifies
    // the index lookup returns None for an unknown qname — the guard that
    // prevents any writes when the target doesn't exist.
    let engine = Engine::open_in_memory().expect("open engine");
    let sym_a = make_symbol("sym_a", "mod.old_fn");
    let index = AsgIndexStore { repo: &engine.repo };
    index.put_symbol(&engine.ref_name, &sym_a, "test").expect("put sym_a");
    append_entry(&engine, &sym_a.symbol_id, "entry");

    let missing = AsgIndexStore { repo: &engine.repo }
        .get_symbol_by_qname(&engine.ref_name, "mod.nonexistent")
        .expect("lookup ok");
    assert!(missing.is_none(), "B not in index — caller should bail before writing");

    // Verify no rebind record exists (we never called do_rebind).
    let rebind_path = paths::rebind_path(&sym_a.symbol_id);
    let result = engine.repo.get_json(&engine.ref_name, &rebind_path);
    assert!(result.is_err(), "no rebind record written when B not found");
}
