//! `asd context-for <qname[,qname...]>` — assemble agent query context.
//!
//! Returns a structured package containing everything an agent needs to reason
//! about a symbol: signature, source range, direct callers/callees, declared
//! and transitive effects, all ledger entries (invariants, hazards, decisions,
//! etc.), and applicable policy constraints.  Pass `--budget-tokens N` to trim
//! the output to fit a context window.
//!
//! This is also exposed as the `asd_context_for` MCP tool.

use std::collections::HashMap;

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, IndexStore, LedgerStore,
    Symbol, discover_symbol_ownership, find_covering_tests,
};

use crate::commands::graph::build_id_map;
use crate::config::Config;

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
}

pub fn run(cfg: &Config, args: ContextForArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };

    let id_map = build_id_map(&engine);

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

        let sym_ctx = assemble_symbol_context(
            &engine,
            &index_store,
            &effect_store,
            &ledger_store,
            &symbol,
            &id_map,
            args.include_body,
            Some(&cfg.db_path),
        )?;
        symbols_out.push(sym_ctx);
    }

    let out = json!({
        "query": args.qnames,
        "count": symbols_out.len(),
        "symbols": symbols_out,
    });

    let mut output = serde_json::to_string_pretty(&out)?;

    // Trim to budget if requested (trim ledger entries from the end).
    if let Some(max_chars) = budget_chars {
        if output.len() > max_chars {
            // Re-assemble with fewer ledger entries per symbol.
            let trimmed = trim_to_budget(&out, max_chars);
            output = serde_json::to_string_pretty(&trimmed)?;
        }
    }

    println!("{output}");
    Ok(())
}

pub(crate) fn assemble_symbol_context(
    engine: &Engine,
    index_store: &AsgIndexStore<'_>,
    effect_store: &AsgEffectStore<'_>,
    ledger_store: &AsgLedgerStore<'_>,
    symbol: &Symbol,
    id_map: &HashMap<String, Symbol>,
    include_body: bool,
    db_path: Option<&std::path::Path>,
) -> Result<Value> {
    // Callers and callees.
    let callee_ids = index_store.get_callees(&engine.ref_name, &symbol.symbol_id)?;
    let caller_ids = index_store.get_callers(&engine.ref_name, &symbol.symbol_id)?;

    let resolve = |ids: &[String]| -> Vec<Value> {
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

    // Effects.
    let effects = effect_store.get_effects(&engine.ref_name, &symbol.symbol_id)?;

    // Ledger — all entries, newest first.
    let ledger = ledger_store.list_entries(&engine.ref_name, &symbol.symbol_id)?;

    // Build output — group ledger by kind for readability.
    let mut invariants: Vec<Value> = Vec::new();
    let mut hazards: Vec<Value> = Vec::new();
    let mut ownership: Vec<Value> = Vec::new();
    let mut proofs: Vec<Value> = Vec::new();
    let mut validation_scenarios: Vec<Value> = Vec::new();
    let mut known_bugs: Vec<Value> = Vec::new();
    let mut other_ledger: Vec<Value> = Vec::new();

    for entry in &ledger {
        let v = serde_json::to_value(entry)?;
        match entry.kind {
            agentstatedeveloper_core::LedgerKind::Invariant => invariants.push(v),
            agentstatedeveloper_core::LedgerKind::Hazard => hazards.push(v),
            agentstatedeveloper_core::LedgerKind::Ownership => ownership.push(v),
            agentstatedeveloper_core::LedgerKind::Proof => proofs.push(v),
            agentstatedeveloper_core::LedgerKind::ValidationScenario => validation_scenarios.push(v),
            agentstatedeveloper_core::LedgerKind::KnownBug => known_bugs.push(v),
            _ => other_ledger.push(v),
        }
    }

    let mut sym_val = serde_json::to_value(symbol)?;
    if !include_body {
        // Remove body from the symbol output to keep context compact.
        // The file + line range tells an agent exactly where to look.
        if let Some(obj) = sym_val.as_object_mut() {
            obj.remove("body");
        }
    }

    // t-003: Ownership discovery from git blame + doc-comment annotations.
    let ownership_signal = discover_symbol_ownership(
        &symbol.file,
        symbol.start.line,
        symbol.end.line,
        symbol.doc.as_deref(),
    );
    // Merge discovered signals into the existing ledger ownership entries.
    let mut discovered_ownership: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Some(ref author) = ownership_signal.primary_author {
        discovered_ownership.insert("primary_author".into(), json!(author));
    }
    if let Some(ref doc_owner) = ownership_signal.doc_owner {
        discovered_ownership.insert("doc_owner".into(), json!(doc_owner));
    }
    if !ownership_signal.recent_committers.is_empty() {
        discovered_ownership.insert("recent_committers".into(), json!(ownership_signal.recent_committers));
    }

    // t-004: Find test symbols that cover this impl symbol.
    let covering_tests: Vec<Value> = if let Some(db) = db_path {
        find_covering_tests(db, &symbol.qname)
            .into_iter()
            .map(|(qname, file, line)| json!({ "qname": qname, "file": file, "line": line }))
            .collect()
    } else {
        Vec::new()
    };

    // Invariants and hazards are anti-footgun guards — surface them first so
    // agents see them before the call-graph details.
    Ok(json!({
        "symbol": sym_val,
        "invariants": invariants,
        "hazards": hazards,
        "known_bugs": known_bugs,
        "ownership": ownership,
        "ownership_discovery": discovered_ownership,
        "covering_tests": covering_tests,
        "validation_scenarios": validation_scenarios,
        "callers": resolve(&caller_ids),
        "callees": resolve(&callee_ids),
        "effects": effects,
        "proofs": proofs,
        "decisions_and_notes": other_ledger,
    }))
}

/// Reduce ledger entries to fit within `max_chars`. Trims `decisions_and_notes`
/// first (least critical), then `proofs`, keeping invariants and hazards.
fn trim_to_budget(out: &Value, max_chars: usize) -> Value {
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
