//! `asd read <qname>` — look up a symbol by qname, pull its effect
//! declaration, ledger entries, and direct callers/callees from the call graph.

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, IndexStore, LedgerStore,
};

use crate::commands::graph::build_id_map;
use crate::config::Config;

#[derive(Debug, Args)]
pub struct ReadArgs {
    /// Fully-qualified symbol name, e.g. `pkg.module.func`.
    pub qname: String,
}

pub fn run(cfg: &Config, args: ReadArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let index_store = AsgIndexStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let effects = effect_store.get_effects(&engine.ref_name, &symbol.symbol_id)?;
    let mut ledger = ledger_store.list_entries(&engine.ref_name, &symbol.symbol_id)?;
    ledger.truncate(5);

    // Call graph — resolve symbol IDs to qname + location.
    let callee_ids = index_store.get_callees(&engine.ref_name, &symbol.symbol_id)?;
    let caller_ids = index_store.get_callers(&engine.ref_name, &symbol.symbol_id)?;

    let id_map = build_id_map(&engine);

    let resolve = |ids: Vec<String>| -> Vec<serde_json::Value> {
        ids.iter()
            .map(|id| {
                if let Some(s) = id_map.get(id) {
                    json!({ "qname": s.qname, "file": s.file, "line": s.start.line })
                } else {
                    json!({ "symbol_id": id })
                }
            })
            .collect()
    };

    let out = json!({
        "symbol": symbol,
        "callers": resolve(caller_ids),
        "callees": resolve(callee_ids),
        "effects": effects,
        "ledger": ledger,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
