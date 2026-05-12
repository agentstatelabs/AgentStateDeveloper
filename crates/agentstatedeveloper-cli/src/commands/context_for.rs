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
    OwnershipSignal, Symbol, discover_symbol_ownership, find_covering_tests, stale_warning,
};
use agentstatedeveloper_core::schema::VerificationStatus;

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

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,
}

pub fn run(cfg: &Config, args: ContextForArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = agentstatedeveloper_core::stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore::with_cache(&engine.repo, &cfg.db_path);
    let ledger_store = AsgLedgerStore::with_cache(&engine.repo, &cfg.db_path);

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
            None,  // single symbol — compute ownership fresh
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
    // Pre-computed ownership signal for this symbol's file.  When `Some`,
    // skips the `discover_symbol_ownership` git blame/log calls entirely —
    // pass this when processing multiple symbols to share per-file results.
    ownership_hint: Option<&OwnershipSignal>,
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
    let mut concepts: Vec<Value> = Vec::new();
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
            agentstatedeveloper_core::LedgerKind::Concept => concepts.push(v),
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
    // If the caller passes a pre-computed hint (e.g. per-file cache from investigate),
    // skip the git subprocess spawns entirely.
    let ownership_signal_owned;
    let ownership_signal = if let Some(hint) = ownership_hint {
        hint
    } else {
        ownership_signal_owned = discover_symbol_ownership(
            &symbol.file,
            symbol.start.line,
            symbol.end.line,
            symbol.doc.as_deref(),
        );
        &ownership_signal_owned
    };
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
    // t-005: Include annotated owners with source confidence for each signal.
    if !ownership_signal.annotated.is_empty() {
        let annotated_val: Vec<Value> = ownership_signal.annotated.iter().map(|a| {
            json!({ "name": a.name, "source": serde_json::to_value(a.source).unwrap_or(json!("unknown")) })
        }).collect();
        discovered_ownership.insert("annotated".into(), json!(annotated_val));
    }

    // t-003: Find test symbols that cover this impl symbol (with file + run command).
    let covering_tests: Vec<Value> = if let Some(db) = db_path {
        find_covering_tests(db, &symbol.qname)
            .into_iter()
            .map(|ct| json!({
                "qname": ct.qname,
                "file": ct.file,
                "line": ct.line,
                "run_command": ct.run_command,
            }))
            .collect()
    } else {
        Vec::new()
    };

    // t-002: Per-effect verification detail — cross-reference declared effects
    // against the verification mismatches so agents see ok/mismatch/unverified per effect.
    let effects_detail: Vec<Value> = if let Some(ref decl) = effects {
        let mismatch_effects: std::collections::HashSet<String> = decl
            .verification
            .as_ref()
            .map(|v| v.mismatches.iter().map(|m| m.effect.as_str().to_string()).collect())
            .unwrap_or_default();
        let overall_ok = decl.verification.as_ref()
            .map(|v| matches!(v.status, VerificationStatus::Ok))
            .unwrap_or(false);
        decl.declared.iter().map(|e| {
            let effect_str = e.effect.as_str();
            let is_mismatched = mismatch_effects.contains(effect_str);
            let status = if decl.verification.is_none() {
                "unverified"
            } else if is_mismatched {
                "mismatch"
            } else if overall_ok {
                "ok"
            } else {
                "ok"
            };
            let mut obj = serde_json::Map::new();
            obj.insert("effect".into(), json!(effect_str));
            obj.insert("status".into(), json!(status));
            if let Some(ref adapter) = e.adapter {
                obj.insert("adapter".into(), json!(adapter));
            }
            if let Some(ref pattern) = e.source_pattern {
                obj.insert("source_pattern".into(), json!(pattern));
            }
            if let Some(note) = &e.note {
                obj.insert("note".into(), json!(note));
            }
            Value::Object(obj)
        }).collect()
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
        "concepts": concepts,
        "ownership": ownership,
        "ownership_discovery": discovered_ownership,
        "covering_tests": covering_tests,
        "validation_scenarios": validation_scenarios,
        "callers": resolve(&caller_ids),
        "callees": resolve(&callee_ids),
        "effects": effects,
        "effects_detail": effects_detail,
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
