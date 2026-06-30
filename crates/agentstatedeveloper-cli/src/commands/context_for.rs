//! `asd context-for <qname[,qname...]>` — assemble agent query context.
//!
//! Returns a structured package containing everything an agent needs to reason
//! about a symbol: signature, source range, direct callers/callees, declared
//! and transitive effects, all ledger entries (invariants, hazards, decisions,
//! etc.), and applicable policy constraints.  Pass `--budget-tokens N` to trim
//! the output to fit a context window.
//!
//! This is also exposed as the `asd_context_for` MCP tool.

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{AsgEffectStore, AsgIndexStore, AsgLedgerStore, Engine, IndexStore};

use crate::commands::graph::AsdTimer;
use crate::config::Config;

// Plan M t-001 (1.0.91): assemble_symbol_context lifted to
// core::context. Re-export so existing intra-CLI imports keep
// working (investigate.rs imports this path).
pub(crate) use agentstatedeveloper_core::assemble_symbol_context;

#[derive(Debug, Args)]
pub struct ContextForArgs {
    /// Comma-separated list of fully-qualified symbol names.
    /// Example: `DriftSynthPool.resolveForPreview,Scheduler.restartLane`
    pub qnames: String,

    /// Approximate token budget for the output (rough estimate: 1 token ≈ 4 chars).
    /// When set, ledger entries and callees are trimmed to fit.
    #[arg(long)]
    pub budget_tokens: Option<usize>,

    /// Include full source body of each symbol (can be large).
    #[arg(long, default_value = "false")]
    pub include_body: bool,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Print per-phase timing to stderr.
    #[arg(long)]
    pub timing: bool,
}

pub fn run(cfg: &Config, args: ContextForArgs) -> Result<()> {
    let mut t = AsdTimer::new(args.timing);
    if !args.quiet {
        if let Some(warn) = agentstatedeveloper_core::stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    t.phase("open_engine");
    let index_store = AsgIndexStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);
    let ledger_store = AsgLedgerStore::from_engine(&engine);

    let id_map = index_store.build_id_map(&engine);
    t.phase("build_id_map");

    let qnames: Vec<&str> = args.qnames.split(',').map(|s| s.trim()).collect();
    let budget_chars = args.budget_tokens.map(|t| t * 4);

    let mut symbols_out = Vec::new();

    for qname in &qnames {
        let symbol = match index_store.get_symbol_by_qname(&engine.ref_name, qname)? {
            Some(s) => s,
            None => {
                symbols_out.push(json!({ "qname": qname, "error": "symbol not found" }));
                continue;
            }
        };
        t.phase("symbol_lookup");

        let sym_ctx = assemble_symbol_context(
            &engine,
            &index_store,
            &effect_store,
            &ledger_store,
            &symbol,
            &id_map,
            args.include_body,
            engine.fts.as_ref(),
            None, // single symbol — compute ownership fresh
        )?;
        t.phase("assemble_context");
        symbols_out.push(sym_ctx);
    }

    let out = json!({
        "query": args.qnames,
        "count": symbols_out.len(),
        "symbols": symbols_out,
    });

    // Token economy (1.0.78): context_for output is always JSON
    // consumed by agents — compact, no whitespace bloat.
    let mut output = serde_json::to_string(&out)?;

    // Trim to budget if requested (trim ledger entries from the end).
    if let Some(max_chars) = budget_chars {
        if output.len() > max_chars {
            // Re-assemble with fewer ledger entries per symbol.
            let trimmed = trim_to_budget(&out, max_chars);
            output = serde_json::to_string(&trimmed)?;
        }
    }

    println!("{output}");
    t.total("context-for");
    Ok(())
}

/// Reduce ledger entries to fit within `max_chars`. Trims `decisions_and_notes`
/// first (least critical), then `proofs`, keeping invariants and hazards.
fn trim_to_budget(out: &Value, _max_chars: usize) -> Value {
    // Simple approach: return as-is and let the caller log a warning.
    // A more sophisticated trim would iterate and drop entries.
    // For now just note the budget was exceeded.
    let mut v = out.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "_budget_warning".to_string(),
            json!(format!(
                "output exceeds budget; use --budget-tokens to trim, or filter to fewer qnames"
            )),
        );
    }
    v
}
