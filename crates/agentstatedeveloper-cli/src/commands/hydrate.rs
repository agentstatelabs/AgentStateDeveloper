//! `asd hydrate [--dir <path>] [--verify]` — read the `.asd/v1/` sidecar and write
//! its contents back into the ASG repo. Inverse of `asd sync`.
//!
//! Intended for fresh `git clone` flows: after `asd init`, `asd hydrate`
//! populates the ASG repo from the committed sidecar so the local
//! machine has full current-state without a registry call.
//!
//! `--verify` performs a read-back pass after hydration and checks that
//! the symbol, effect, and ledger counts (including invariant count) match
//! what was loaded.  Exits with a non-zero status and prints a `verify`
//! object on mismatch so callers and CI can detect silent failures.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{LedgerEntry, LedgerKind, hydrate_from_dir, Engine};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct HydrateArgs {
    /// Project root to hydrate from. `.asd/v1/` is appended internally.
    /// Defaults to the current working directory.
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// After hydrating, read the engine back and verify symbol, ledger,
    /// and effect counts match what was loaded. Exits non-zero on mismatch.
    #[arg(long)]
    pub verify: bool,
}

pub fn run(cfg: &Config, args: HydrateArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let dir = resolve_dir(args.dir)?;

    // sidecar::hydrate_from_dir returns a clear error if `.asd/v1/`
    // doesn't exist. Surface it as-is; the message already says "did
    // you mean to run `asd sync` first?".
    let summary = hydrate_from_dir(&engine.repo, &engine.ref_name, &dir, &cfg.agent_id)?;

    let mut out = json!({
        "dir": dir.join(".asd/v1").display().to_string(),
        "symbols_loaded": summary.symbols_loaded,
        "symbols_skipped": summary.symbols_skipped,
        "effects_loaded": summary.effects_loaded,
        "ledger_entries_loaded": summary.ledger_entries_loaded,
        "rebinds_replayed": summary.rebinds_replayed,
        "blobs_rejected": summary.blobs_rejected,
        "refs_dropped": summary.refs_dropped,
        "missing_schema_version": summary.missing_schema_version,
        "note": "commit history not restored; run `asd index` to rebuild the semantic index and call graph",
    });

    if args.verify {
        let result = verify_hydration(&engine, &summary);
        let ok = result.ok;
        if let Some(obj) = out.as_object_mut() {
            obj.insert("verify".to_string(), serde_json::to_value(&result)?);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        if !ok {
            std::process::exit(1);
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }

    Ok(())
}

#[derive(serde::Serialize)]
struct VerifyResult {
    ok: bool,
    symbols_expected: usize,
    symbols_actual: usize,
    effects_expected: usize,
    effects_actual: usize,
    ledger_entries_expected: usize,
    ledger_entries_actual: usize,
    invariants_actual: usize,
    discrepancies: Vec<String>,
}

fn verify_hydration(
    engine: &Engine,
    summary: &agentstatedeveloper_core::HydrateSummary,
) -> VerifyResult {
    let ref_name = &engine.ref_name;
    let mut discrepancies: Vec<String> = Vec::new();

    // --- Symbol count -------------------------------------------------------
    let symbols_actual = match engine.repo.get_tree(ref_name, "/asd/v1/index/by-qname") {
        Ok(serde_json::Value::Object(map)) => map.len(),
        _ => 0,
    };
    // symbols_loaded + symbols_skipped = total symbols in DB (skipped means
    // already-present and up-to-date, not a failure).
    let symbols_expected = summary.symbols_loaded + summary.symbols_skipped;
    if symbols_actual < summary.symbols_loaded {
        discrepancies.push(format!(
            "symbols: loaded {} but only {} readable from index",
            summary.symbols_loaded, symbols_actual
        ));
    }

    // --- Effect count -------------------------------------------------------
    let effects_actual = match engine.repo.get_tree(ref_name, "/asd/v1/effects") {
        Ok(serde_json::Value::Object(map)) => map.len(),
        _ => 0,
    };
    let effects_expected = summary.effects_loaded;
    if effects_actual < effects_expected {
        discrepancies.push(format!(
            "effects: loaded {} but only {} readable",
            effects_expected, effects_actual
        ));
    }

    // --- Ledger count + invariant count -------------------------------------
    let (ledger_entries_actual, invariants_actual) = count_ledger_entries(engine);
    let ledger_entries_expected = summary.ledger_entries_loaded;
    if ledger_entries_actual < ledger_entries_expected {
        discrepancies.push(format!(
            "ledger: loaded {} but only {} readable",
            ledger_entries_expected, ledger_entries_actual
        ));
    }

    // Flag if sidecar had ledger entries but no invariants surfaced.
    if ledger_entries_expected > 0 && invariants_actual == 0 {
        discrepancies.push(
            "invariants: ledger entries were loaded but no invariant-kind entries found — possible kind mismatch".to_string(),
        );
    }

    // Any rejected blobs are a hard warning.
    if summary.blobs_rejected > 0 {
        discrepancies.push(format!(
            "{} sidecar file(s) were rejected due to parse errors",
            summary.blobs_rejected
        ));
    }

    VerifyResult {
        ok: discrepancies.is_empty(),
        symbols_expected,
        symbols_actual,
        effects_expected,
        effects_actual,
        ledger_entries_expected,
        ledger_entries_actual,
        invariants_actual,
        discrepancies,
    }
}

fn count_ledger_entries(engine: &Engine) -> (usize, usize) {
    let tree = match engine.repo.get_tree(&engine.ref_name, "/asd/v1/ledger") {
        Ok(v) => v,
        _ => return (0, 0),
    };
    let mut total = 0usize;
    let mut invariants = 0usize;
    if let Some(sym_map) = tree.as_object() {
        for per_symbol in sym_map.values() {
            if let Some(entry_map) = per_symbol.as_object() {
                for entry_val in entry_map.values() {
                    if let Ok(e) = serde_json::from_value::<LedgerEntry>(entry_val.clone()) {
                        total += 1;
                        if e.kind == LedgerKind::Invariant {
                            invariants += 1;
                        }
                    }
                }
            }
        }
    }
    (total, invariants)
}

fn resolve_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p),
        None => Ok(std::env::current_dir()?),
    }
}
