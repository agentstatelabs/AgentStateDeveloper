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
use serde_json::json;

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

    /// Output ceiling of 4 serialized UTF-8 bytes per approximate token (min 128).
    /// Not a tokenizer guarantee. JSON and its newline fit; omissions are flagged.
    #[arg(long)]
    pub budget_tokens: Option<usize>,

    /// Include bounded current source; known-changed indexed files suppress excerpts.
    #[arg(long, default_value = "false")]
    pub include_body: bool,

    /// Include source excerpts for direct callers as well as the target.
    #[arg(long)]
    pub include_caller_bodies: bool,

    /// Include parser-recorded caller expressions/locations; not a full binding proof.
    #[arg(long)]
    pub include_call_sites: bool,

    /// Maximum recorded sites per caller (1..32); omissions remain explicit.
    #[arg(long, default_value = "4", value_parser = clap::value_parser!(u32).range(1..=32))]
    pub call_site_limit: u32,

    /// Maximum source lines per excerpt (1..400).
    #[arg(long, default_value = "80", value_parser = clap::value_parser!(u32).range(1..=400))]
    pub body_lines: u32,

    /// Maximum direct callers supplied with source (0..100); remaining metadata stays.
    #[arg(long, default_value = "8", value_parser = clap::value_parser!(u32).range(0..=100))]
    pub caller_limit: u32,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Print per-phase timing to stderr.
    #[arg(long)]
    pub timing: bool,
}

pub fn run(cfg: &Config, args: ContextForArgs) -> Result<()> {
    let budget_bytes = args
        .budget_tokens
        .map(|tokens| {
            anyhow::ensure!(tokens >= 128, "--budget-tokens must be at least 128");
            tokens
                .checked_mul(4)
                .ok_or_else(|| anyhow::anyhow!("--budget-tokens is too large"))
        })
        .transpose()?;
    let root = std::env::current_dir()?;
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
    // Read only requested provenance records; metadata-only queries do not
    // load the source-fingerprint tree at all.
    let fingerprint_for = |id: &str| {
        engine
            .repo
            .get_tree(
                &engine.ref_name,
                &format!(
                    "{}/{}",
                    agentstatedeveloper_core::paths::source_fingerprints_root(),
                    id
                ),
            )
            .ok()
    };
    t.phase("build_id_map");

    let qnames: Vec<&str> = args.qnames.split(',').map(|s| s.trim()).collect();

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

        let mut sym_ctx = assemble_symbol_context(
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
        if args.include_body || args.include_caller_bodies {
            sym_ctx["source"] = super::context_evidence::source_excerpt(
                &root,
                &symbol.file,
                symbol.start.line,
                symbol.end.line,
                args.body_lines as usize,
            );
            let indexed = fingerprint_for(&symbol.symbol_id);
            sym_ctx["source"] = super::context_evidence::verify_indexed_source(
                sym_ctx["source"].take(),
                indexed.as_ref(),
                &symbol,
            );
        }
        if args.include_caller_bodies {
            let caller_ids = index_store.get_callers(&engine.ref_name, &symbol.symbol_id)?;
            let callers = sym_ctx["callers"].as_array_mut().unwrap();
            for (i, (row, id)) in callers.iter_mut().zip(caller_ids).enumerate() {
                if i >= args.caller_limit as usize {
                    row["source"] = json!({"status":"omitted", "reason":"caller-limit"});
                } else if let Some(caller) = id_map.get(&id) {
                    row["source"] = super::context_evidence::source_excerpt(
                        &root,
                        &caller.file,
                        caller.start.line,
                        caller.end.line,
                        args.body_lines as usize,
                    );
                    let indexed = fingerprint_for(&caller.symbol_id);
                    row["source"] = super::context_evidence::verify_indexed_source(
                        row["source"].take(),
                        indexed.as_ref(),
                        caller,
                    );
                } else {
                    row["source"] = json!({"status":"unavailable", "reason":"unresolved symbol"});
                }
            }
        }
        if args.include_call_sites {
            let records = engine
                .repo
                .get_tree(
                    &engine.ref_name,
                    &format!(
                        "{}/{}",
                        agentstatedeveloper_core::paths::call_sites_root(),
                        symbol.symbol_id
                    ),
                )
                .unwrap_or(serde_json::Value::Null);
            let ids = index_store.get_callers(&engine.ref_name, &symbol.symbol_id)?;
            for (i, (row, id)) in sym_ctx["callers"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .zip(ids)
                .enumerate()
            {
                row["call_sites"] = if i >= args.caller_limit as usize {
                    json!({"status":"omitted","reason":"caller-limit"})
                } else if let Some(caller) = id_map.get(&id) {
                    super::context_evidence::call_site_evidence(
                        &root,
                        caller,
                        records.get(&id),
                        args.call_site_limit as usize,
                        args.body_lines as usize,
                    )
                } else {
                    json!({"status":"unavailable","reason":"unresolved indexed caller"})
                };
            }
        }
        t.phase("assemble_context");
        symbols_out.push(sym_ctx);
    }

    let mut out = json!({
        "query": args.qnames,
        "count": symbols_out.len(),
        "symbols": symbols_out,
    });
    if args.include_body || args.include_caller_bodies || args.include_call_sites {
        out["source_contract"] = json!({"origin":"current_worktree", "locations":"indexed_ranges",
            "index_verification":"per_excerpt", "graph_freshness":"not_verified",
            "graph_scope":"indexed direct edges; absence does not prove no callers",
            "body_lines":args.body_lines, "caller_body_limit":args.caller_limit,
            "call_site_limit":args.call_site_limit, "caller_evidence_limit":args.caller_limit,
            "call_site_scope":"parser-recorded resolved calls; syntactic import candidate is not a binding proof; full chain not recorded"});
    }

    // Token economy (1.0.78): context_for output is always JSON
    // consumed by agents — compact, no whitespace bloat.
    if let Some(bytes) = budget_bytes {
        out = super::context_evidence::bounded(out, bytes);
    }
    println!("{}", serde_json::to_string(&out)?);
    t.total("context-for");
    Ok(())
}
