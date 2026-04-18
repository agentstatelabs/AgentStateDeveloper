//! `asd verify-effects <qname>` — M1 placeholder. Loads the declared
//! effect set for a symbol and emits it with `status: "unverified"`.
//! A real static checker lands later.

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{AsgEffectStore, AsgIndexStore, EffectStore, Engine, IndexStore};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct VerifyEffectsArgs {
    /// Fully-qualified symbol name.
    pub qname: String,
}

pub fn run(cfg: &Config, args: VerifyEffectsArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let declared = effect_store
        .get_effects(&engine.ref_name, &symbol.symbol_id)?
        .map(|d| d.declared)
        .unwrap_or_default();

    let out = json!({
        "qname": symbol.qname,
        "symbol_id": symbol.symbol_id,
        "status": "unverified",
        "declared": declared,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
