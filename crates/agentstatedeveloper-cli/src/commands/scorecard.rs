//! `asd scorecard` — lightweight benchmark scorecard across the five dimensions.
//!
//! Dimensions (each 0-100):
//!   truth       — % symbols with verified effects + ownership ledger entries
//!   feedback    — feedback entries recorded (50+ = 100%)
//!   change      — % symbols with invariant or validation scenario ledger entries
//!   uncertainty — index health proxy: symbol count + effect verification rate
//!   workflow    — ledger entry density + CTX-tagged entries presence

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine,
    FeedbackStore, IndexStore, LedgerStore,
    schema::{LedgerKind, VerificationStatus},
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct ScorecardArgs {
    /// Emit machine-readable JSON instead of the default table.
    #[arg(long)]
    pub json: bool,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,
}

pub fn run(cfg: &Config, args: ScorecardArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = agentstatedeveloper_core::stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }

    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };

    // --- Gather raw data ------------------------------------------------

    // All indexed symbols.
    let all_qnames: Vec<String> = {
        let tree = engine.repo
            .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(serde_json::Value::Object(Default::default()));
        tree.as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    let total_symbols = all_qnames.len();

    if total_symbols == 0 {
        let zero = json!({
            "note": "no symbols indexed — run `asd index` first",
            "scores": { "truth": 0, "feedback": 0, "change": 0, "uncertainty": 0, "workflow": 0, "overall": 0 }
        });
        println!("{}", serde_json::to_string_pretty(&zero)?);
        return Ok(());
    }

    let mut verified_count = 0usize;
    let mut owned_count = 0usize;
    let mut has_invariant = 0usize;
    let mut has_validation = 0usize;
    let mut total_ledger_entries = 0usize;
    let mut ctx_tagged_entries = 0usize;

    for qname in &all_qnames {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };

        // Effects verification (truth dimension).
        if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, &sym.symbol_id) {
            if decl.verification.as_ref()
                .map(|v| matches!(v.status, VerificationStatus::Ok))
                .unwrap_or(false)
            {
                verified_count += 1;
            }
        }

        // Ledger analysis.
        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();
        total_ledger_entries += entries.len();

        let mut has_inv = false;
        let mut has_vs = false;
        for entry in &entries {
            match entry.kind {
                LedgerKind::Invariant => has_inv = true,
                LedgerKind::ValidationScenario => has_vs = true,
                LedgerKind::Ownership => { owned_count += 1; }
                _ => {}
            }
            // Count CTX-tagged entries (workflow dimension).
            if entry.tags.iter().any(|t| t.starts_with("ctx:")) {
                ctx_tagged_entries += 1;
            }
        }
        if has_inv { has_invariant += 1; }
        if has_vs { has_validation += 1; }
    }

    let feedback_count = feedback_store.list_all(&engine.ref_name)
        .map(|v| v.len()).unwrap_or(0);

    // --- Compute scores -------------------------------------------------

    let truth_score = if total_symbols == 0 { 0.0 } else {
        let verified_pct = verified_count as f64 / total_symbols as f64;
        let owned_pct = owned_count as f64 / total_symbols as f64;
        ((verified_pct + owned_pct) / 2.0 * 100.0).min(100.0)
    };

    // 50 feedback entries = 100%; scales linearly below.
    let feedback_score = (feedback_count as f64 / 50.0 * 100.0).min(100.0);

    let change_score = if total_symbols == 0 { 0.0 } else {
        let inv_pct = has_invariant as f64 / total_symbols as f64;
        let vs_pct = has_validation as f64 / total_symbols as f64;
        ((inv_pct + vs_pct) / 2.0 * 100.0).min(100.0)
    };

    // Uncertainty: proxy on index health — verified effects rate + symbols > threshold.
    let uncertainty_score = {
        let effect_rate = if total_symbols == 0 { 0.0 } else { verified_count as f64 / total_symbols as f64 };
        let volume_score = (total_symbols as f64 / 500.0).min(1.0); // 500 symbols = fully indexed
        ((effect_rate + volume_score) / 2.0 * 100.0).min(100.0)
    };

    // Workflow: ledger density + CTX adoption.
    let workflow_score = {
        let density = (total_ledger_entries as f64 / total_symbols as f64 / 2.0).min(1.0); // 2 entries/sym = good
        let ctx_adoption = if total_ledger_entries == 0 { 0.0 } else {
            (ctx_tagged_entries as f64 / total_ledger_entries as f64).min(1.0)
        };
        ((density * 0.6 + ctx_adoption * 0.4) * 100.0).min(100.0)
    };

    let overall = (truth_score + feedback_score + change_score + uncertainty_score + workflow_score) / 5.0;

    let scores = json!({
        "truth":       truth_score.round() as u64,
        "feedback":    feedback_score.round() as u64,
        "change":      change_score.round() as u64,
        "uncertainty": uncertainty_score.round() as u64,
        "workflow":    workflow_score.round() as u64,
        "overall":     overall.round() as u64,
    });

    let details = json!({
        "total_symbols": total_symbols,
        "verified_effects": verified_count,
        "owned_symbols": owned_count,
        "invariant_symbols": has_invariant,
        "validation_symbols": has_validation,
        "feedback_entries": feedback_count,
        "total_ledger_entries": total_ledger_entries,
        "ctx_tagged_ledger_entries": ctx_tagged_entries,
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json!({ "scores": scores, "details": details }))?);
        return Ok(());
    }

    // Human-readable table.
    println!("ASD Benchmark Scorecard");
    println!("{:-<40}", "");
    for (dim, key) in &[
        ("Truth Model      ", "truth"),
        ("Feedback Loop    ", "feedback"),
        ("Change Model     ", "change"),
        ("Uncertainty Model", "uncertainty"),
        ("Workflow         ", "workflow"),
    ] {
        let val = scores[key].as_u64().unwrap_or(0);
        let bar: String = (0..val / 5).map(|_| '█').chain((val / 5..20).map(|_| '░')).collect();
        println!("{dim}  {bar}  {val:3}/100");
    }
    println!("{:-<40}", "");
    println!("Overall                               {:3}/100", scores["overall"].as_u64().unwrap_or(0));
    println!();
    println!("Symbols indexed:    {}", total_symbols);
    println!("Feedback entries:   {}", feedback_count);
    println!("Ledger entries:     {}", total_ledger_entries);
    println!("CTX-tagged:         {}", ctx_tagged_entries);

    Ok(())
}
