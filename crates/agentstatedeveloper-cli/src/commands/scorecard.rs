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

use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    Engine,
    scorecard::{ScorecardOptions, compute},
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

fn history_path(cfg: &Config, override_path: Option<&PathBuf>) -> PathBuf {
    if let Some(p) = override_path {
        return p.clone();
    }
    let mut p = cfg.db_path.clone();
    let stem = p
        .file_stem()
        .map(|s| format!("{}.scorecard-history.json", s.to_string_lossy()))
        .unwrap_or_else(|| "asd-scorecard-history.json".to_string());
    p.set_file_name(stem);
    p
}

fn load_history(path: &PathBuf) -> Vec<Value> {
    if !path.exists() {
        return vec![];
    }
    let s = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str::<Vec<Value>>(&s).unwrap_or_default()
}

fn save_snapshot(path: &PathBuf, snapshot: &Value) {
    let mut history = load_history(path);
    history.push(snapshot.clone());
    if history.len() > 100 {
        history.drain(..history.len() - 100);
    }
    // Best-effort: scorecard history is observability metadata. A
    // write failure (read-only fs, disk full) shouldn't fail the
    // user-facing scorecard command — the score is already printed.
    let _ = std::fs::write(
        path,
        serde_json::to_string_pretty(&history).unwrap_or_default(),
    );
}

pub fn run(cfg: &Config, args: ScorecardArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = agentstatedeveloper_core::stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }

    // All gathering and arithmetic lives in core::scorecard — this command
    // owns presentation (the table, the trend snapshot) and nothing else.
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let card = compute(
        &engine,
        &cfg.db_path,
        &ScorecardOptions {
            scope: args.scope.as_deref(),
            paths: args.paths.as_deref(),
            drill_down: args.drill_down.as_deref(),
            drill_limit: args.limit,
        },
    );

    if card.matched_nothing {
        // Terminal-flavoured advice, which is why the phrasing stays here
        // rather than in core: over HTTP "--scope/--paths" means nothing.
        let note = if card.scoped {
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

    let scores = card.scores;
    let scoped = card.scoped;
    let paths_filter = card.data_quality.scope.clone().unwrap_or_default();
    let total_symbols = card.details.total_symbols;
    let feedback_count = card.details.feedback_entries;
    let total_ledger_entries = card.details.total_ledger_entries;
    let ctx_tagged_entries = card.details.ctx_tagged_ledger_entries;
    let structured_tokens = card.token_economy.structured_tokens;
    let source_read_tokens = card.token_economy.source_read_tokens_est;
    let ratio_x = card.token_economy.ratio_x;
    let sparse_note: Option<&str> = card
        .data_quality
        .sparse_db
        .then_some(card.data_quality.note.as_str());

    let now = Utc::now().to_rfc3339();
    let hist_path = history_path(cfg, args.history_path.as_ref());

    // Trend computation.
    let trend_obj: Option<Value> = if args.trend {
        let history = load_history(&hist_path);
        history.last().map(|prev| {
            let prev_scores = prev.get("scores").cloned().unwrap_or(json!({}));
            let dims = [
                "truth",
                "feedback",
                "change",
                "uncertainty",
                "workflow",
                "overall",
            ];
            let mut deltas = serde_json::Map::new();
            for dim in dims {
                // `Scores::get` keeps this list and the struct in step — a
                // new dimension added to one shows up in the other.
                let current = scores.get(dim).unwrap_or(scores.overall) as i64;
                let prev_val = prev_scores.get(dim).and_then(Value::as_i64).unwrap_or(0);
                let delta = current - prev_val;
                deltas.insert(
                    dim.to_string(),
                    json!({
                        "current": current,
                        "previous": prev_val,
                        "delta": delta,
                        "trend": if delta > 0 { "▲" } else if delta < 0 { "▼" } else { "─" },
                    }),
                );
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

    // data_quality / details / token_economy all come from the shared
    // envelope now — see core::scorecard::Scorecard::to_json.

    if args.json {
        // Shared envelope, then the two things only this command adds: when
        // it ran, and how it compares to the last run.
        let mut out = json!({ "timestamp": now });
        let obj = out.as_object_mut().expect("json! built an object");
        for (k, v) in card
            .to_json()
            .as_object()
            .expect("to_json built an object")
            .clone()
        {
            obj.insert(k, v);
        }
        if let Some(ref t) = trend_obj {
            out.as_object_mut()
                .unwrap()
                .insert("trend".into(), t.clone());
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
        ("Truth Model      ", "truth", scores.truth),
        ("Feedback Loop    ", "feedback", scores.feedback),
        ("Change Model     ", "change", scores.change),
        ("Uncertainty Model", "uncertainty", scores.uncertainty),
        ("Workflow         ", "workflow", scores.workflow),
    ];
    for (label, key, val) in &dim_names {
        let bar: String = (0..val / 5)
            .map(|_| '█')
            .chain((val / 5..20).map(|_| '░'))
            .collect();
        let trend_suffix = if let Some(ref t) = trend_obj {
            let sym = t
                .get("dimensions")
                .and_then(|d| d.get(*key))
                .and_then(|d| d.get("trend"))
                .and_then(Value::as_str)
                .unwrap_or("─");
            let delta = t
                .get("dimensions")
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
        let sym = t
            .get("dimensions")
            .and_then(|d| d.get("overall"))
            .and_then(|d| d.get("trend"))
            .and_then(Value::as_str)
            .unwrap_or("─");
        let delta = t
            .get("dimensions")
            .and_then(|d| d.get("overall"))
            .and_then(|d| d.get("delta"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        format!("  {sym}{:+}", delta)
    } else {
        String::new()
    };
    println!(
        "Overall                               {:3}/100{trend_overall}",
        scores.overall
    );
    println!();
    println!("Symbols indexed:    {}", total_symbols);
    println!("Feedback entries:   {}", feedback_count);
    println!("Ledger entries:     {}", total_ledger_entries);
    println!("CTX-tagged:         {}", ctx_tagged_entries);
    println!(
        "Token economy:      ~{:.1}x vs source ({} index tok vs ~{} source tok, est.)",
        ratio_x, structured_tokens, source_read_tokens
    );

    if let Some(ref note) = sparse_note {
        println!("\nNote: {note}");
    }

    if let Some(drill) = card.drill_down.as_ref().filter(|d| d.total_gaps > 0) {
        let (dimension, total_gaps) = (&drill.dimension, drill.total_gaps);
        println!("\n## Drill-down: {dimension} gaps ({total_gaps} symbols)");
        for row in &drill.gap_symbols {
            println!("  {}  ({})", row.qname, row.file);
        }
        // `omitted` is what core actually withheld, which is not always
        // `total - limit`: a run with fewer gaps than the limit omits none.
        if drill.omitted > 0 {
            println!("  … and {} more (use --limit to show more)", drill.omitted);
        }
    }

    if args.trend {
        if let Some(ref t) = trend_obj {
            let compared = t
                .get("compared_to")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            println!("\nTrend vs: {compared}");
        } else {
            println!("\nNo previous snapshot found — this run has been saved as the baseline.");
        }
    }

    Ok(())
}
