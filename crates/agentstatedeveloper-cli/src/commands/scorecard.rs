//! `asd scorecard` — benchmark scorecard across the five dimensions.
//!
//! Dimensions (each 0-100):
//!   truth       — % symbols with verified effects + ownership ledger entries
//!   feedback    — feedback entries recorded (50+ = 100%)
//!   change      — % symbols with invariant or validation scenario ledger entries
//!   uncertainty — index health proxy: symbol count + effect verification rate
//!   workflow    — ledger entry density + CTX-tagged entries presence
//!
//! Per-dimension drill-down: `--drill-down <dim>`
//! Trend vs previous run:    `--trend`

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgFeedbackStore, Engine,
    FeedbackStore, EffectStore,
    glob_match, resolve_scope,
    schema::{LedgerEntry, LedgerKind, Symbol, VerificationStatus},
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

    /// Show per-symbol breakdown for a specific dimension.
    /// Values: truth, feedback, change, uncertainty, workflow
    #[arg(long)]
    pub drill_down: Option<String>,

    /// Compare current scores against the previous stored snapshot and show trends.
    #[arg(long)]
    pub trend: bool,

    /// Override the history file path (default: <db_path>.scorecard-history.json).
    #[arg(long)]
    pub history_path: Option<PathBuf>,

    /// Max symbols shown in --drill-down output (default: 10).
    #[arg(long, default_value_t = 10)]
    pub limit: usize,

    /// Named scope alias from .asd/scopes.toml — evaluate only that subsystem.
    #[arg(long)]
    pub scope: Option<String>,

    /// Comma-separated glob patterns — restrict scoring to matching file paths.
    #[arg(long)]
    pub paths: Option<String>,
}

struct Metrics {
    total_symbols: usize,
    verified_count: usize,
    owned_count: usize,
    has_invariant: usize,
    has_validation: usize,
    total_ledger_entries: usize,
    ctx_tagged_entries: usize,
    feedback_count: usize,
}

struct Scores {
    truth: u64,
    feedback: u64,
    change: u64,
    uncertainty: u64,
    workflow: u64,
    overall: u64,
}

fn compute_scores(m: &Metrics) -> Scores {
    let total = m.total_symbols;
    let truth = if total == 0 { 0.0 } else {
        let verified_pct = m.verified_count as f64 / total as f64;
        let owned_pct = m.owned_count as f64 / total as f64;
        ((verified_pct + owned_pct) / 2.0 * 100.0).min(100.0)
    };
    let feedback = (m.feedback_count as f64 / 50.0 * 100.0).min(100.0);
    let change = if total == 0 { 0.0 } else {
        let inv_pct = m.has_invariant as f64 / total as f64;
        let vs_pct = m.has_validation as f64 / total as f64;
        ((inv_pct + vs_pct) / 2.0 * 100.0).min(100.0)
    };
    let uncertainty = {
        let effect_rate = if total == 0 { 0.0 } else { m.verified_count as f64 / total as f64 };
        let volume_score = (total as f64 / 500.0).min(1.0);
        ((effect_rate + volume_score) / 2.0 * 100.0).min(100.0)
    };
    let workflow = {
        let density = (m.total_ledger_entries as f64 / total.max(1) as f64 / 2.0).min(1.0);
        let ctx_adoption = if m.total_ledger_entries == 0 { 0.0 } else {
            (m.ctx_tagged_entries as f64 / m.total_ledger_entries as f64).min(1.0)
        };
        ((density * 0.6 + ctx_adoption * 0.4) * 100.0).min(100.0)
    };
    let overall = (truth + feedback + change + uncertainty + workflow) / 5.0;
    Scores {
        truth: truth.round() as u64,
        feedback: feedback.round() as u64,
        change: change.round() as u64,
        uncertainty: uncertainty.round() as u64,
        workflow: workflow.round() as u64,
        overall: overall.round() as u64,
    }
}

fn history_path(cfg: &Config, override_path: Option<&PathBuf>) -> PathBuf {
    if let Some(p) = override_path {
        return p.clone();
    }
    let mut p = cfg.db_path.clone();
    let stem = p.file_stem()
        .map(|s| format!("{}.scorecard-history.json", s.to_string_lossy()))
        .unwrap_or_else(|| "asd-scorecard-history.json".to_string());
    p.set_file_name(stem);
    p
}

fn load_history(path: &PathBuf) -> Vec<Value> {
    if !path.exists() { return vec![]; }
    let s = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str::<Vec<Value>>(&s).unwrap_or_default()
}

fn save_snapshot(path: &PathBuf, snapshot: &Value) {
    let mut history = load_history(path);
    history.push(snapshot.clone());
    if history.len() > 100 { history.drain(..history.len() - 100); }
    let _ = std::fs::write(path, serde_json::to_string_pretty(&history).unwrap_or_default());
}

pub fn run(cfg: &Config, args: ScorecardArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = agentstatedeveloper_core::stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }

    // Build path filter from --scope / --paths.
    let mut paths_filter: Vec<String> = Vec::new();
    if let Some(ref s) = args.scope {
        paths_filter.extend(resolve_scope(s, &cfg.db_path));
    }
    if let Some(ref p) = args.paths {
        paths_filter.extend(p.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()));
    }
    let scoped = !paths_filter.is_empty();

    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };

    // Bulk load index — one git read instead of N.
    let all_syms: Vec<Symbol> = {
        let tree = engine.repo
            .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(serde_json::Value::Object(Default::default()));
        tree.as_object()
            .map(|m| m.values()
                .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                .collect())
            .unwrap_or_default()
    };

    // Apply path filter upfront.
    let scored_syms: Vec<&Symbol> = if scoped {
        all_syms.iter()
            .filter(|s| paths_filter.iter().any(|p| glob_match(p, &s.file)))
            .collect()
    } else {
        all_syms.iter().collect()
    };
    let total_symbols = scored_syms.len();

    if total_symbols == 0 {
        let note = if scoped {
            "no symbols matched the path filter — try broadening --scope/--paths"
        } else {
            "no symbols indexed — run `asd index` first"
        };
        let zero = json!({
            "note": note,
            "scores": { "truth": 0, "feedback": 0, "change": 0, "uncertainty": 0, "workflow": 0, "overall": 0 }
        });
        println!("{}", serde_json::to_string_pretty(&zero)?);
        return Ok(());
    }

    // Bulk load ledger — one git read instead of N per-symbol reads.
    let ledger_by_sym: HashMap<String, Vec<LedgerEntry>> = {
        let tree = engine.repo
            .get_tree(&engine.ref_name, "/asd/v1/ledger")
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let mut map: HashMap<String, Vec<LedgerEntry>> = HashMap::new();
        if let serde_json::Value::Object(by_symbol) = tree {
            for (sym_id, per_symbol) in by_symbol {
                if let serde_json::Value::Object(entries_map) = per_symbol {
                    let mut entries: Vec<LedgerEntry> = entries_map.values()
                        .filter_map(|v| serde_json::from_value::<LedgerEntry>(v.clone()).ok())
                        .collect();
                    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                    let superseded: std::collections::HashSet<String> = entries.iter()
                        .flat_map(|e| e.supersedes.iter().cloned())
                        .collect();
                    entries.retain(|e| !superseded.contains(&e.entry_id));
                    map.insert(sym_id, entries);
                }
            }
        }
        map
    };

    // Per-symbol tracking for drill-down.
    let drill = args.drill_down.as_deref().unwrap_or("").to_lowercase();
    let need_drill = !drill.is_empty();

    let mut drill_rows: Vec<Value> = Vec::new();

    let mut verified_count = 0usize;
    let mut owned_count = 0usize;
    let mut has_invariant = 0usize;
    let mut has_validation = 0usize;
    let mut total_ledger_entries = 0usize;
    let mut ctx_tagged_entries = 0usize;

    for sym in &scored_syms {
        let has_verified = if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, &sym.symbol_id) {
            decl.verification.as_ref()
                .map(|v| matches!(v.status, VerificationStatus::Ok))
                .unwrap_or(false)
        } else { false };
        if has_verified { verified_count += 1; }

        let entries = ledger_by_sym.get(&sym.symbol_id).cloned().unwrap_or_default();
        total_ledger_entries += entries.len();

        let qname = &sym.qname;

        let mut sym_owned = false;
        let mut sym_inv = false;
        let mut sym_vs = false;
        let mut sym_ctx = false;
        for entry in &entries {
            match entry.kind {
                LedgerKind::Invariant => sym_inv = true,
                LedgerKind::ValidationScenario => sym_vs = true,
                LedgerKind::Ownership => sym_owned = true,
                _ => {}
            }
            if entry.tags.iter().any(|t| t.starts_with("ctx:")) {
                sym_ctx = true;
                ctx_tagged_entries += 1;
            }
        }
        if sym_owned { owned_count += 1; }
        if sym_inv { has_invariant += 1; }
        if sym_vs { has_validation += 1; }

        if need_drill {
            let include = match drill.as_str() {
                "truth" => !has_verified || !sym_owned,
                "change" => !sym_inv || !sym_vs,
                "workflow" => entries.is_empty() || !sym_ctx,
                "uncertainty" => !has_verified,
                _ => false,
            };
            if include {
                drill_rows.push(json!({
                    "qname": qname,
                    "file": sym.file,
                    "has_verified_effects": has_verified,
                    "has_ownership": sym_owned,
                    "has_invariant": sym_inv,
                    "has_validation_scenario": sym_vs,
                    "ledger_entries": entries.len(),
                    "ctx_tagged": sym_ctx,
                }));
            }
        }
    }

    let feedback_count = feedback_store.list_all(&engine.ref_name)
        .map(|v| v.len()).unwrap_or(0);

    let metrics = Metrics {
        total_symbols,
        verified_count,
        owned_count,
        has_invariant,
        has_validation,
        total_ledger_entries,
        ctx_tagged_entries,
        feedback_count,
    };
    let scores = compute_scores(&metrics);

    // Sparse-DB detection: warn when ledger density is too low to be meaningful.
    let ledger_density = total_ledger_entries as f64 / total_symbols.max(1) as f64;
    let sparse_db = ledger_density < 0.5 && total_symbols > 0;
    let sparse_note = if sparse_db {
        Some(format!(
            "sparse ledger ({total_ledger_entries} entries across {total_symbols} symbols, \
             {:.2} avg) — run 'asd sync' + 'asd hydrate' to populate; \
             scores reflect data density, not workflow quality",
            ledger_density
        ))
    } else {
        None
    };

    let now = Utc::now().to_rfc3339();
    let hist_path = history_path(cfg, args.history_path.as_ref());

    // Trend computation.
    let trend_obj: Option<Value> = if args.trend {
        let history = load_history(&hist_path);
        history.last().map(|prev| {
            let prev_scores = prev.get("scores").cloned().unwrap_or(json!({}));
            let dims = ["truth", "feedback", "change", "uncertainty", "workflow", "overall"];
            let mut deltas = serde_json::Map::new();
            for dim in dims {
                let current = match dim {
                    "truth" => scores.truth as i64,
                    "feedback" => scores.feedback as i64,
                    "change" => scores.change as i64,
                    "uncertainty" => scores.uncertainty as i64,
                    "workflow" => scores.workflow as i64,
                    _ => scores.overall as i64,
                };
                let prev_val = prev_scores.get(dim).and_then(Value::as_i64).unwrap_or(0);
                let delta = current - prev_val;
                deltas.insert(dim.to_string(), json!({
                    "current": current,
                    "previous": prev_val,
                    "delta": delta,
                    "trend": if delta > 0 { "▲" } else if delta < 0 { "▼" } else { "─" },
                }));
            }
            json!({
                "compared_to": prev.get("timestamp").cloned().unwrap_or(json!("unknown")),
                "dimensions": deltas,
            })
        })
    } else {
        None
    };

    // Save snapshot after computing trend (so we don't compare to ourselves).
    let snapshot = json!({
        "timestamp": now,
        "scores": {
            "truth": scores.truth,
            "feedback": scores.feedback,
            "change": scores.change,
            "uncertainty": scores.uncertainty,
            "workflow": scores.workflow,
            "overall": scores.overall,
        },
    });
    save_snapshot(&hist_path, &snapshot);

    let data_quality = json!({
        "ledger_density": ledger_density,
        "symbols_scored": total_symbols,
        "symbols_with_any_ledger": scored_syms.iter()
            .filter(|s| ledger_by_sym.contains_key(&s.symbol_id))
            .count(),
        "coverage_pct": if total_symbols > 0 {
            (scored_syms.iter().filter(|s| ledger_by_sym.contains_key(&s.symbol_id)).count() as f64
             / total_symbols as f64 * 100.0).round()
        } else { 0.0 },
        "sparse_db": sparse_db,
        "note": sparse_note.as_deref().unwrap_or("ledger density is adequate"),
        "scope": if scoped { json!(paths_filter) } else { json!(null) },
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
        let mut out = json!({
            "timestamp": now,
            "capability_scores": snapshot["scores"].clone(),
            "scores": snapshot["scores"].clone(),  // kept for history compat
            "data_quality": data_quality,
            "details": details,
        });
        if need_drill {
            let total_gaps = drill_rows.len();
            let shown: Vec<_> = drill_rows.into_iter().take(args.limit).collect();
            let omitted = total_gaps.saturating_sub(shown.len());
            out.as_object_mut().unwrap().insert("drill_down".into(), json!({
                "dimension": drill,
                "total_gaps": total_gaps,
                "shown": shown.len(),
                "omitted": omitted,
                "gap_symbols": shown,
            }));
        }
        if let Some(ref t) = trend_obj {
            out.as_object_mut().unwrap().insert("trend".into(), t.clone());
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Human-readable table.
    println!("ASD Benchmark Scorecard");
    if scoped {
        println!("  scope:  {}", paths_filter.join(", "));
    }
    println!("{:-<40}", "");
    let dim_names = [
        ("Truth Model      ", "truth",       scores.truth),
        ("Feedback Loop    ", "feedback",    scores.feedback),
        ("Change Model     ", "change",      scores.change),
        ("Uncertainty Model", "uncertainty", scores.uncertainty),
        ("Workflow         ", "workflow",    scores.workflow),
    ];
    for (label, key, val) in &dim_names {
        let bar: String = (0..val / 5).map(|_| '█').chain((val / 5..20).map(|_| '░')).collect();
        let trend_suffix = if let Some(ref t) = trend_obj {
            let sym = t.get("dimensions")
                .and_then(|d| d.get(*key))
                .and_then(|d| d.get("trend"))
                .and_then(Value::as_str)
                .unwrap_or("─");
            let delta = t.get("dimensions")
                .and_then(|d| d.get(*key))
                .and_then(|d| d.get("delta"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            format!("  {sym}{:+}", delta)
        } else {
            String::new()
        };
        println!("{label}  {bar}  {:3}/100{trend_suffix}", val);
    }
    println!("{:-<40}", "");
    let trend_overall = if let Some(ref t) = trend_obj {
        let sym = t.get("dimensions").and_then(|d| d.get("overall")).and_then(|d| d.get("trend")).and_then(Value::as_str).unwrap_or("─");
        let delta = t.get("dimensions").and_then(|d| d.get("overall")).and_then(|d| d.get("delta")).and_then(Value::as_i64).unwrap_or(0);
        format!("  {sym}{:+}", delta)
    } else { String::new() };
    println!("Overall                               {:3}/100{trend_overall}", scores.overall);
    println!();
    println!("Symbols indexed:    {}", total_symbols);
    println!("Feedback entries:   {}", feedback_count);
    println!("Ledger entries:     {}", total_ledger_entries);
    println!("CTX-tagged:         {}", ctx_tagged_entries);

    if let Some(ref note) = sparse_note {
        println!("\nNote: {note}");
    }

    if need_drill && !drill_rows.is_empty() {
        let total_gaps = drill_rows.len();
        let limit = args.limit;
        println!("\n## Drill-down: {drill} gaps ({total_gaps} symbols)");
        for row in drill_rows.iter().take(limit) {
            let qname = row.get("qname").and_then(Value::as_str).unwrap_or("");
            let file = row.get("file").and_then(Value::as_str).unwrap_or("");
            println!("  {qname}  ({file})");
        }
        if total_gaps > limit {
            println!("  … and {} more (use --limit to show more)", total_gaps - limit);
        }
    }

    if args.trend {
        if let Some(ref t) = trend_obj {
            let compared = t.get("compared_to").and_then(Value::as_str).unwrap_or("unknown");
            println!("\nTrend vs: {compared}");
        } else {
            println!("\nNo previous snapshot found — this run has been saved as the baseline.");
        }
    }

    Ok(())
}
