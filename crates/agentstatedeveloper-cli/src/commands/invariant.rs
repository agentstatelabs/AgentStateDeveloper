//! `asd invariant` — first-class shortcut for ledger invariant operations.
//!
//! Three subcommands:
//!   add <qname> "<summary>"   — record an invariant that must hold at qname
//!   list [<qname>]            — list all recorded invariants (or for a single symbol)
//!   rm <entry-id>             — remove (withdraw) an invariant entry

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore,
};

use crate::commands::{graph::build_id_map, ledger::open_engine_public};
use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum InvariantCmd {
    /// Record an invariant that must hold at the given symbol.
    Add(AddArgs),

    /// List invariants — for one symbol or all symbols.
    List(ListArgs),

    /// Remove (withdraw) an invariant entry by its entry-id.
    Rm(RmArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Fully-qualified symbol name.
    pub qname: String,

    /// One-line invariant summary.
    pub summary: String,

    /// Author identifier recorded in the ledger entry.
    #[arg(long, default_value = "asd-cli-user")]
    pub author_id: String,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter to a single symbol's invariants (omit to show all).
    pub qname: Option<String>,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Entry id of the invariant to remove (e.g. `led_abc…`).
    pub entry_id: String,

    /// Author id — must match the original entry's author.
    #[arg(long, default_value = "asd-cli-user")]
    pub author_id: String,
}

pub fn run(cfg: &Config, cmd: InvariantCmd) -> Result<()> {
    match cmd {
        InvariantCmd::Add(args) => add(cfg, args),
        InvariantCmd::List(args) => list(cfg, args),
        InvariantCmd::Rm(args) => rm(cfg, args),
    }
}

fn add(cfg: &Config, args: AddArgs) -> Result<()> {
    let engine = open_engine_public(cfg)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let author = Author {
        kind: AuthorKind::Agent,
        id: args.author_id.clone(),
    };
    let entry = LedgerEntry::new(
        &symbol.symbol_id,
        LedgerKind::Invariant,
        args.summary.clone(),
        author,
    );

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    ledger_store.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "added",
            "entry_id": entry.entry_id,
            "symbol_id": entry.symbol_id,
            "qname": args.qname,
            "summary": args.summary,
        }))?
    );
    Ok(())
}

fn list(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let id_map = build_id_map(&engine);

    // Build a reverse map: symbol_id → qname.
    let sym_to_qname: std::collections::HashMap<&str, &str> = id_map
        .iter()
        .map(|(id, sym)| (id.as_str(), sym.qname.as_str()))
        .collect();

    let rows: Vec<serde_json::Value> = if let Some(ref qname) = args.qname {
        let index_store = AsgIndexStore { repo: &engine.repo };
        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let symbol = index_store
            .get_symbol_by_qname(&engine.ref_name, qname)?
            .ok_or_else(|| anyhow::anyhow!("symbol not found: {qname}"))?;
        ledger_store
            .list_entries(&engine.ref_name, &symbol.symbol_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.kind == LedgerKind::Invariant)
            .map(|e| entry_to_json(e, qname))
            .collect()
    } else {
        // Walk all ledger entries via raw tree (same approach as `asd list ledger`).
        let tree = match engine.repo.get_tree(&engine.ref_name, "/asd/v1/ledger") {
            Ok(v) => v,
            _ => serde_json::Value::Null,
        };
        let mut rows = Vec::new();
        if let Some(sym_map) = tree.as_object() {
            for per_symbol in sym_map.values() {
                if let Some(entry_map) = per_symbol.as_object() {
                    for entry_val in entry_map.values() {
                        if let Ok(e) = serde_json::from_value::<LedgerEntry>(entry_val.clone()) {
                            if e.kind == LedgerKind::Invariant {
                                let qname = sym_to_qname
                                    .get(e.symbol_id.as_str())
                                    .copied()
                                    .unwrap_or("");
                                rows.push(entry_to_json(e, qname));
                            }
                        }
                    }
                }
            }
        }
        rows.sort_by(|a, b| {
            a.get("qname").and_then(serde_json::Value::as_str)
                .cmp(&b.get("qname").and_then(serde_json::Value::as_str))
        });
        rows
    };

    println!("{}", serde_json::to_string_pretty(&json!({ "invariants": rows }))?);
    Ok(())
}

fn rm(cfg: &Config, args: RmArgs) -> Result<()> {
    let engine = open_engine_public(cfg)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store.withdraw_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.author_id,
        &cfg.agent_id,
    )?;
    let status = if outcome.already_resolved {
        "already-withdrawn"
    } else {
        "withdrawn"
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": status,
            "entry_id": outcome.entry.entry_id,
            "symbol_id": outcome.entry.symbol_id,
        }))?
    );
    Ok(())
}

fn entry_to_json(entry: agentstatedeveloper_core::LedgerEntry, qname: &str) -> serde_json::Value {
    json!({
        "entry_id": entry.entry_id,
        "qname": qname,
        "symbol_id": entry.symbol_id,
        "summary": entry.summary,
        "author": { "kind": format!("{:?}", entry.author.kind), "id": entry.author.id },
        "created_at": entry.created_at,
        "tags": entry.tags,
    })
}
