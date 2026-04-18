//! `asd read <qname>` — look up a symbol by qname, pull its effect
//! declaration, and the 5 most recent ledger entries.

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, IndexStore, LedgerStore,
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct ReadArgs {
    /// Fully-qualified symbol name, e.g. `pkg.module.func`.
    pub qname: String,
}

pub fn run(cfg: &Config, args: ReadArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let effects = effect_store.get_effects(&engine.ref_name, &symbol.symbol_id)?;
    let mut ledger = ledger_store.list_entries(&engine.ref_name, &symbol.symbol_id)?;
    ledger.truncate(5);

    let out = json!({
        "symbol": symbol,
        "effects": effects,
        "ledger": ledger,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
