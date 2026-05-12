//! `asd status` — show index health, age, modified files, and sidecar lifecycle.
//!
//! With `--json`, emits a machine-readable object that includes a `trust` block
//! (State Trust Score) and appends a compact snapshot to `.asd/trust-history.jsonl`
//! for drift tracking.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Engine, IndexStore, LedgerStore,
    SearchFtsDb, SidecarState, format_age, sidecar_lifecycle_state,
    schema::{LedgerKind, Symbol},
    compute_trust_score,
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Show source files modified since the last index run (requires git).
    #[arg(long)]
    pub show_dirty: bool,

    /// Emit machine-readable JSON instead of the default human text.
    #[arg(long)]
    pub json: bool,

    /// Subcommand: history (show trust-history snapshots).
    #[command(subcommand)]
    pub command: Option<StatusSubcommand>,
}

#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    /// Show recent trust-score snapshots from .asd/trust-history.jsonl.
    History(StatusHistoryArgs),
}

#[derive(Debug, Args)]
pub struct StatusHistoryArgs {
    /// Number of recent entries to show (default: 20).
    #[arg(long, default_value = "20")]
    pub last: usize,
    /// Emit raw JSONL instead of a formatted table.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: StatusArgs) -> Result<()> {
    // Dispatch subcommands first.
    if let Some(sub) = args.command {
        match sub {
            StatusSubcommand::History(h) => return show_history(cfg, &h),
        }
    }

    let fts = SearchFtsDb::open(&cfg.db_path)?;

    let project_root = cfg.db_path.parent().unwrap_or(std::path::Path::new("."));
    let sidecar_state = sidecar_lifecycle_state(project_root);

    if !fts.has_data() {
        if args.json {
            println!("{}", json!({
                "state": "empty",
                "note": "run 'asd index <dir>' to build",
                "sidecar": sidecar_state_key(&sidecar_state),
            }));
        } else {
            println!("ASD index status");
            println!("  db:       {}", cfg.db_path.display());
            println!("  state:    empty — run 'asd index <dir>' to build");
        }
        return Ok(());
    }

    let count = fts.symbol_count();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (indexed_at, age_hours, fresh) = match fts.last_indexed_at() {
        Some(ts) => {
            let age_h = (now - ts).max(0) / 3600;
            (Some(ts), Some(age_h), age_h == 0)
        }
        None => (None, None, false),
    };

    let dirty_files = if args.show_dirty || args.json {
        collect_dirty_files(cfg)
    } else {
        vec![]
    };

    // Concept-gap detection: symbols with Ownership but no Concept entry.
    let concept_gaps: Vec<serde_json::Value> = if args.json {
        if let Ok(engine) = Engine::open_sqlite(&cfg.db_path) {
            let index_store = AsgIndexStore { repo: &engine.repo };
            let ledger_store = AsgLedgerStore::with_cache(&engine.repo, &cfg.db_path);
            let tree = engine.repo
                .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            tree.as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                        .filter_map(|sym| {
                            let entries = ledger_store
                                .list_entries(&engine.ref_name, &sym.symbol_id)
                                .unwrap_or_default();
                            let has_ownership = entries.iter().any(|e| e.kind == LedgerKind::Ownership);
                            let has_concept = entries.iter().any(|e| e.kind == LedgerKind::Concept);
                            if has_ownership && !has_concept {
                                Some(json!({"qname": sym.qname, "file": sym.file}))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let sidecar_key = sidecar_state_key(&sidecar_state);
    let sidecar_action = sidecar_action_hint(&sidecar_state);

    if args.json {
        let index_state = if fresh { "fresh" } else if age_hours.unwrap_or(0) >= 1 { "stale" } else { "ok" };

        // State Trust Score rollup.
        let trust = compute_trust_score(&cfg.db_path);

        let out = json!({
            "db": cfg.db_path.display().to_string(),
            "symbols": count,
            "indexed_at_unix": indexed_at,
            "age_hours": age_hours,
            "state": index_state,
            "sidecar": sidecar_key,
            "sidecar_action": sidecar_action,
            "dirty_files": dirty_files,
            "concept_gaps": concept_gaps,
            "trust": trust.to_json(),
        });

        // Append compact snapshot to trust-history.jsonl.
        append_trust_history(cfg, &trust, indexed_at, age_hours, count as u64,
            sidecar_key, dirty_files.len(), concept_gaps.len());

        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Human-readable output.
    println!("ASD index status");
    println!("  db:       {}", cfg.db_path.display());
    println!("  symbols:  {count}");

    match indexed_at {
        Some(ts) => {
            println!("  indexed:  {} (unix {})", format_age(ts), ts);
            if age_hours.unwrap_or(0) >= 1 {
                println!("  warning:  index is {}h old — consider re-running 'asd index'", age_hours.unwrap_or(0));
            } else {
                println!("  state:    fresh");
            }
        }
        None => println!("  indexed:  unknown"),
    }

    let sidecar_label = match sidecar_state {
        SidecarState::Missing   => "missing — run 'asd sync' to create",
        SidecarState::Present   => "present — run 'asd hydrate' to load into ASG",
        SidecarState::Hydrated  => "hydrated",
        SidecarState::FreshReset => "fresh-reset (deliberate reset — re-run 'asd index' + 'asd sync')",
    };
    println!("  sidecar:  {sidecar_label}");

    if args.show_dirty {
        let files = dirty_files;
        if files.is_empty() {
            println!("  dirty:    none (all tracked source files match index)");
        } else {
            println!("  dirty:    {} modified source file(s) since last commit:", files.len());
            for f in &files {
                println!("            {}", f);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Trust-history JSONL
// ---------------------------------------------------------------------------

fn trust_history_path(cfg: &Config) -> std::path::PathBuf {
    cfg.db_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(".asd")
        .join("trust-history.jsonl")
}

fn append_trust_history(
    cfg: &Config,
    trust: &agentstatedeveloper_core::TrustScore,
    indexed_at: Option<i64>,
    age_hours: Option<i64>,
    symbol_count: u64,
    sidecar_state: &str,
    dirty_file_count: usize,
    concept_gap_count: usize,
) {
    use std::io::Write;

    let now = chrono::Utc::now().to_rfc3339();
    let record = json!({
        "timestamp": now,
        "indexed_at_unix": indexed_at,
        "age_hours": age_hours,
        "db_state": if age_hours.map(|h| h >= 1).unwrap_or(false) { "stale" } else { "fresh" },
        "symbol_count": symbol_count,
        "sidecar_state": sidecar_state,
        "dirty_file_count": dirty_file_count,
        "concept_gap_count": concept_gap_count,
        "ledger_density": trust.signals.ledger_density,
        "schema_version": trust.signals.schema_version,
        "asd_version": env!("CARGO_PKG_VERSION"),
        "trust_score": trust.score,
        "trust_level": trust.level,
    });

    let path = trust_history_path(cfg);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Read existing lines, cap at 500 before appending.
    const MAX_LINES: usize = 500;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = existing.lines().collect();
    let new_line = serde_json::to_string(&record).unwrap_or_default();

    // Trim old entries so total stays at MAX_LINES after append.
    if lines.len() >= MAX_LINES {
        let keep = MAX_LINES - 1;
        lines = lines[lines.len() - keep..].to_vec();
    }

    let mut content = lines.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&new_line);
    content.push('\n');

    let _ = std::fs::write(&path, content);
}

// ---------------------------------------------------------------------------
// Trust-history reader (`asd status history`)
// ---------------------------------------------------------------------------

fn show_history(cfg: &Config, args: &StatusHistoryArgs) -> Result<()> {
    let path = trust_history_path(cfg);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut records: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Newest first, then take last N.
    records.reverse();
    records.truncate(args.last);
    records.reverse(); // restore chronological for display

    if records.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("No trust-history snapshots yet. Run `asd status --json` to record one.");
        }
        return Ok(());
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }

    // Human-readable table.
    println!("{:<25} {:>6} {:>7} {:>8} {:>10} {:>8} {:>5}",
        "timestamp", "score", "level", "symbols", "sidecar", "age_hrs", "dirty");
    println!("{}", "-".repeat(75));
    for r in &records {
        let ts  = r.get("timestamp").and_then(|v| v.as_str()).unwrap_or("?");
        // Trim to 23 chars (ISO without offset) for table width.
        let ts_short = &ts[..ts.len().min(23)];
        let score   = r.get("trust_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let level   = r.get("trust_level").and_then(|v| v.as_str()).unwrap_or("?");
        let syms    = r.get("symbol_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let sidecar = r.get("sidecar_state").and_then(|v| v.as_str()).unwrap_or("?");
        let age     = r.get("age_hours").and_then(|v| v.as_i64()).map(|h| h.to_string()).unwrap_or_else(|| "?".to_string());
        let dirty   = r.get("dirty_file_count").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("{:<25} {:>6.2} {:>7} {:>8} {:>10} {:>8} {:>5}",
            ts_short, score, level, syms, sidecar, age, dirty);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sidecar_state_key(s: &SidecarState) -> &'static str {
    match s {
        SidecarState::Missing    => "missing",
        SidecarState::Present    => "present",
        SidecarState::Hydrated   => "hydrated",
        SidecarState::FreshReset => "fresh-reset",
    }
}

fn sidecar_action_hint(s: &SidecarState) -> &'static str {
    match s {
        SidecarState::Missing    => "run 'asd sync' to create sidecar",
        SidecarState::Present    => "run 'asd hydrate' to load sidecar into ASG",
        SidecarState::Hydrated   => "sidecar is current",
        SidecarState::FreshReset => "re-run 'asd index' then 'asd sync'",
    }
}

fn collect_dirty_files(cfg: &Config) -> Vec<String> {
    let workspace = cfg.db_path.parent().unwrap_or(std::path::Path::new("."));
    let output = std::process::Command::new("git")
        .args(["status", "--short", "--untracked-files=no"])
        .current_dir(workspace)
        .output();

    let Ok(out) = output else { return vec![]; };
    if !out.status.success() { return vec![]; }

    let text = String::from_utf8_lossy(&out.stdout);
    let source_exts = [".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go", ".kt", ".java", ".rb", ".cs"];
    text.lines()
        .filter(|l| source_exts.iter().any(|ext| l.ends_with(ext)))
        .map(|l| l.trim().to_string())
        .collect()
}
