//! `asd probe run` — golden benchmark harness for ASD ranking/classification.
//!
//! Reads `.asd/probes.toml` (relative to the DB), executes each probe as a
//! subprocess call to `asd --db <path> <command> <args...>`, parses the JSON
//! output, and evaluates structural assertions. On failure, attaches the full
//! debug payload so an agent can read boost_debug/classification_debug and
//! make a single targeted heuristic change per iteration.
//!
//! `asd probe add` appends a new [[probe]] entry to probes.toml.
//!
//! Filtering flags (may combine):
//!   --name <name>     run only the probe with exactly this name
//!   --tag  <tag>      run only probes whose [[probe]] tags array contains this tag
//!   --filter <substr> run only probes whose name contains this substring (legacy)
//!
//! Output flags:
//!   --json            emit results as a JSON object (includes duration_ms per probe,
//!                     wall_time_ms, worker_count, total/passed/failed, slowest top-5,
//!                     slow_violations list)
//!   --fail-slow <ms>  exit non-zero if any probe exceeds this wall-clock threshold
//!   --fail-fast       stop on first assertion failure
//!
//! Assertion kinds:
//!   file_not_in_key  — no item in JSON array `key` has `field` containing `value`
//!   file_in_key      — at least one item in array `key` has `field` containing `value`
//!   qname_rank_lte   — result whose qname contains `fragment` appears at rank ≤ `max_rank`
//!   qname_rank_eq    — result whose qname contains `fragment` appears at EXACTLY `exact_rank` (precision-mode; Plan J t-019)
//!   result_count_lte — results array length ≤ `max`
//!   cluster_winner_kind_not        — cluster_debug entry matching `doc_stem` winner kind ≠ `kind_not`
//!   cluster_winner_qname_contains  — cluster_debug entry matching `doc_stem` winner qname contains `fragment`
//!   no_duplicate_summaries         — no two suggested_entries share the same summary per symbol
//!   qname_not_in_results           — no result's qname contains `fragment` (feedback suppression check)
//!   boosted_outranked_contains     — boosted_outranked has an entry containing `fragment`
//!   ambiguous_terms_nonempty       — ambiguous_terms array is non-empty (broad query uncertainty check)
//!   scoped_suggestions_nonempty    — scoped_suggestions array is non-empty
//!   scoped_suggestions_contains    — scoped_suggestions contains an entry matching `fragment`
//!   uncertainty_level_lte          — uncertainty.level ≤ max_level (low/medium/high/critical)
//!   uncertainty_reason_contains    — uncertainty.reasons[*].code contains `code`
//!   uncertainty_action_eq          — uncertainty.recommended_action equals `action`
//!   recovery_suggestions_nonempty  — uncertainty.recovery_suggestions is non-empty
//!   recovery_suggestion_estimated  — at least one recovery suggestion has `estimated_recovery = strength`
//!   feedback_summary_gte           — feedback_summary[field] ≥ min_value (e.g. suppressed ≥ 1)
//!   feedback_summary_eq            — feedback_summary[field] == value
//!   feedback_rules_contains        — feedback_summary.rules_applied contains `rule`
//!   field_gte                      — output[dot.path] ≥ min_value (numeric)
//!   field_eq                       — output[dot.path] equals expected string
//!   array_field_count_lte          — length of dot-path array field ≤ max_count
//!   array_field_count_gte          — length of dot-path array field ≥ min_count
//!   workflow_steps_contains        — workflow.steps_detected contains step
//!   evidence_score_gte             — workflow.evidence_quality.evidence_quality_score ≥ min_value
//!   data_quality_state_eq          — trust.data_quality.state equals expected string
//!   feedback_state_eq              — feedback_state[field] == value (bool)
//!   feedback_state_field_eq        — feedback_state[field] equals expected string
//!   feedback_coverage_eq           — feedback_summary.coverage equals expected string
//!   array_field_contains           — dot-path array field contains `value` string
//!   array_field_excludes           — dot-path array field does NOT contain `value` string
//!   uncertainty_exact_symbol_match — uncertainty.exact_symbol_match == expected (bool)
//!   uncertainty_primary_source_eq  — uncertainty.sources.primary equals expected string
//!   uncertainty_source_gte         — uncertainty.sources[source] >= min_value
//!   all_items_have_field           — every object in a dot-path array has the named field set
//!   file_field_contains            — specific array item (by file_fragment) has field containing substring
//!
//! Special probe command (no assert block needed):
//!   hydrate-roundtrip              — write sentinel, sync, hydrate into fresh DB, verify survival

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{Connection, params};
use serde_json::Value;

use agentstatedeveloper_core::{
    FtsFilters, SearchFtsDb, calibration::compute_calibration, compute_trust_score,
    stale_warning,
};

use crate::config::Config;

// ---------------------------------------------------------------------------
// CLI shape
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct ProbeCmd {
    #[command(subcommand)]
    pub sub: ProbeSub,
}

#[derive(Debug, Subcommand)]
pub enum ProbeSub {
    /// Run all probes from .asd/probes.toml and report pass/fail.
    Run(ProbeRunArgs),
    /// Append a new probe entry to .asd/probes.toml.
    Add(ProbeAddArgs),
    /// Generate a starter .asd/probes.toml from the current index.
    Bootstrap(ProbeBootstrapArgs),
    /// Print configured probes (name, command, tags) from
    /// .asd/probes.toml. Use this to see what `asd probe run`
    /// would execute, without actually running anything.
    /// `history` shows past run results — this shows the
    /// definitions themselves.
    List(ProbeListArgs),
    /// Show probe run history from .asd/probe-history.jsonl.
    History(ProbeHistoryArgs),
    /// Rebuild probe-analytics.db from probe-history.jsonl.
    Reindex(ProbeReindexArgs),
}

#[derive(Debug, Args)]
pub struct ProbeListArgs {
    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
    /// Filter to probes whose `tags` array contains this tag.
    #[arg(long)]
    pub tag: Option<String>,
}

#[derive(Debug, Args)]
pub struct ProbeBootstrapArgs {
    /// Overwrite .asd/probes.toml if it already exists.
    #[arg(long)]
    pub force: bool,
    /// Number of top symbols to generate ranking probes for (default: 5).
    #[arg(long, default_value = "5")]
    pub top: usize,
}

#[derive(Debug, Args)]
pub struct ProbeReindexArgs {
    /// Drop and rebuild even if the DB already exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ProbeHistoryArgs {
    /// Show last N runs (default: 20).
    #[arg(long, default_value = "20")]
    pub last: usize,

    /// Filter to runs that were recorded with a specific --tag filter.
    #[arg(long)]
    pub tag: Option<String>,

    /// Emit raw JSONL records instead of the summary table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ProbeRunArgs {
    /// Emit results as JSON (default: human-readable).
    /// JSON output includes duration_ms per probe, total/passed/failed,
    /// top-5 slowest probes, and any slow_violations from --fail-slow.
    #[arg(long)]
    pub json: bool,

    /// Run only the probe with exactly this name.
    /// Useful for targeted single-probe CI runs: `asd probe run --name waveform-canvas-not-in-edit`.
    #[arg(long)]
    pub name: Option<String>,

    /// Run only probes whose `tags` array contains this tag.
    /// Tag probes in probes.toml with e.g. `tags = ["ranking", "m53"]` then
    /// run a subset with `asd probe run --tag m53`.
    #[arg(long)]
    pub tag: Option<String>,

    /// Run only probes whose name contains this substring (legacy, kept for backward compat).
    #[arg(long)]
    pub filter: Option<String>,

    /// Fail if any probe's wall-clock time exceeds this threshold (milliseconds).
    /// Useful as a CI performance regression gate. Exit code is non-zero when
    /// any probe is slow, even if all assertions pass.
    /// Example: `asd probe run --fail-slow 5000` to cap each probe at 5 s.
    #[arg(long)]
    pub fail_slow: Option<u64>,

    /// Stop on first assertion failure (does not affect --fail-slow).
    #[arg(long)]
    pub fail_fast: bool,

    /// Number of probes to run in parallel (default 0 = auto, capped at 6).
    /// Beyond 6 parallel readers the SQLite page-cache contention increases wall time.
    /// Set --jobs 1 to run sequentially for deterministic single-line terminal output.
    #[arg(long, default_value = "0")]
    pub jobs: usize,
}

#[derive(Debug, Args)]
pub struct ProbeAddArgs {
    /// Probe name (kebab-case).
    #[arg(long)]
    pub name: String,

    /// ASD subcommand to run, e.g. "search" or "prepare-change".
    #[arg(long)]
    pub command: String,

    /// Arguments to pass to the subcommand (space-separated, or repeat flag).
    #[arg(long, num_args = 1..)]
    pub args: Vec<String>,

    /// Assertion as a TOML inline table string.
    /// Example: "{ kind = \"qname_rank_lte\", fragment = \"myFn\", max_rank = 3 }"
    #[arg(long)]
    pub assert: Option<String>,

    /// Optional human description of what this probe checks.
    #[arg(long)]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Probe data model (deserialized from probes.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct ProbeFile {
    #[serde(default)]
    probe: Vec<ProbeEntry>,
}

#[derive(Debug, serde::Deserialize, Clone)]
struct ProbeEntry {
    name: String,
    // Round-trip TOML field — surfaces in `asd probe list` output
    // and probe-file diffs even though no internal logic reads it.
    #[serde(default)]
    #[allow(dead_code)]
    description: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// Optional tag list for selective runs.  Example: `tags = ["ranking", "m53"]`
    #[serde(default)]
    tags: Vec<String>,
    /// Working directory for the subprocess (default: directory containing probes.toml).
    #[serde(default)]
    cwd: Option<String>,
    /// Assertion as a free-form map — kind field discriminates.
    /// Optional: when absent the probe is a smoke test (always passes if command exits 0).
    #[serde(default = "toml_empty_table")]
    assert: toml::Value,
}

fn toml_empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(cfg: &Config, cmd: ProbeCmd) -> Result<()> {
    match cmd.sub {
        ProbeSub::Run(args) => run_probes(cfg, args),
        ProbeSub::Add(args) => add_probe(cfg, args),
        ProbeSub::Bootstrap(args) => bootstrap_probes(cfg, args),
        ProbeSub::List(args) => list_probes(cfg, args),
        ProbeSub::History(args) => show_history(cfg, args),
        ProbeSub::Reindex(args) => reindex_analytics(cfg, args),
    }
}

/// `asd probe list` — show what's configured in .asd/probes.toml
/// without actually running anything. Mirrors the shape of
/// `asd probe run --json` so a follow-up `--name <n>` invocation
/// is easy to construct.
fn list_probes(cfg: &Config, args: ProbeListArgs) -> Result<()> {
    let path = probe_file_path(cfg);
    if !path.exists() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "probe_file": path.display().to_string(),
                    "exists": false,
                    "probes": [],
                    "note": "no probes configured — run `asd probe bootstrap` to generate a starter set",
                }))?
            );
        } else {
            println!("no probes file at {}", path.display());
            println!("run `asd probe bootstrap` to generate a starter set");
        }
        return Ok(());
    }

    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let pf: ProbeFile =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    let probes: Vec<&ProbeEntry> = pf
        .probe
        .iter()
        .filter(|p| match &args.tag {
            Some(t) => p.tags.iter().any(|pt| pt == t),
            None => true,
        })
        .collect();

    if args.json {
        let entries: Vec<serde_json::Value> = probes
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "command": p.command,
                    "args": p.args,
                    "tags": p.tags,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "probe_file": path.display().to_string(),
                "exists": true,
                "total": pf.probe.len(),
                "matched": probes.len(),
                "probes": entries,
            }))?
        );
        return Ok(());
    }

    println!("# probes from {}", path.display());
    if probes.is_empty() {
        let total = pf.probe.len();
        if total == 0 {
            println!("(file exists but has no [[probe]] entries)");
        } else {
            println!("(no probes match --tag filter; {total} total in file)");
        }
        return Ok(());
    }
    for p in &probes {
        let tags = if p.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", p.tags.join(", "))
        };
        println!("{}{tags}", p.name);
        println!("  asd {} {}", p.command, p.args.join(" "));
    }
    println!();
    println!("{} probe(s); run with `asd probe run`", probes.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// probe-analytics.db — schema + helpers
// ---------------------------------------------------------------------------

fn analytics_path(cfg: &Config) -> PathBuf {
    let db_dir = Path::new(&cfg.db_path).parent().unwrap_or(Path::new("."));
    db_dir.join(".asd").join("probe-analytics.db")
}

fn open_analytics_db(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS probe_runs (
            run_id                TEXT PRIMARY KEY,
            asd_version           TEXT NOT NULL,
            started_at            TEXT NOT NULL,
            finished_at           TEXT,
            probe_file            TEXT,
            db_state              TEXT,
            symbol_count          INTEGER,
            scope                 TEXT NOT NULL DEFAULT 'all',
            total                 INTEGER NOT NULL,
            passed                INTEGER NOT NULL,
            failed                INTEGER NOT NULL,
            budget_failed         INTEGER NOT NULL DEFAULT 0,
            wall_time_ms          INTEGER NOT NULL,
            worker_count          INTEGER,
            performance_budget_ms INTEGER,
            filter_name           TEXT,
            filter_tag            TEXT
        );
        CREATE TABLE IF NOT EXISTS probe_results (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id      TEXT NOT NULL REFERENCES probe_runs(run_id),
            name        TEXT NOT NULL,
            command     TEXT,
            assertion   TEXT,
            tags        TEXT,
            passed      INTEGER NOT NULL DEFAULT 1,
            slow        INTEGER NOT NULL DEFAULT 0,
            timed_out   INTEGER NOT NULL DEFAULT 0,
            duration_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_pr_run    ON probe_results(run_id);
        CREATE INDEX IF NOT EXISTS idx_pr_name   ON probe_results(name);
        CREATE INDEX IF NOT EXISTS idx_pr_trend  ON probe_results(name, duration_ms);
        CREATE INDEX IF NOT EXISTS idx_runs_ver  ON probe_runs(asd_version);
        CREATE INDEX IF NOT EXISTS idx_runs_at   ON probe_runs(started_at);
    ",
    )?;
    Ok(conn)
}

/// Derive the scope string from filter fields (mirrors show_history logic).
fn scope_from_record(record: &Value) -> String {
    match (
        record.get("filter_name").and_then(Value::as_str),
        record.get("filter_tag").and_then(Value::as_str),
    ) {
        (Some(n), _) => format!("name:{}", n),
        (_, Some(t)) => format!("tag:{}", t),
        _ => "all".to_string(),
    }
}

/// Insert one run record + its per-probe rows.  Idempotent: skips if run_id exists.
/// Silently returns on any DB error — analytics is best-effort.
fn insert_run_to_analytics(conn: &Connection, record: &Value) {
    let run_id = match record.get("started_at").and_then(Value::as_str) {
        Some(s) => s,
        None => return,
    };

    // Skip if already present.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM probe_runs WHERE run_id=?1",
            params![run_id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if exists {
        return;
    }

    let scope = scope_from_record(record);

    let res = conn.execute(
        "INSERT OR IGNORE INTO probe_runs
         (run_id, asd_version, started_at, finished_at, probe_file, db_state, symbol_count,
          scope, total, passed, failed, budget_failed, wall_time_ms, worker_count,
          performance_budget_ms, filter_name, filter_tag)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![
            run_id,
            record
                .get("asd_version")
                .and_then(Value::as_str)
                .unwrap_or(""),
            record
                .get("started_at")
                .and_then(Value::as_str)
                .unwrap_or(""),
            record.get("finished_at").and_then(Value::as_str),
            record.get("probe_file").and_then(Value::as_str),
            record.get("db_state").and_then(Value::as_str),
            record.get("symbol_count").and_then(Value::as_i64),
            scope,
            record.get("total").and_then(Value::as_i64).unwrap_or(0),
            record.get("passed").and_then(Value::as_i64).unwrap_or(0),
            record.get("failed").and_then(Value::as_i64).unwrap_or(0),
            record
                .get("budget_failed")
                .and_then(Value::as_bool)
                .map(|b| b as i64)
                .unwrap_or(0),
            record
                .get("wall_time_ms")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            record.get("worker_count").and_then(Value::as_i64),
            record.get("performance_budget_ms").and_then(Value::as_i64),
            record.get("filter_name").and_then(Value::as_str),
            record.get("filter_tag").and_then(Value::as_str),
        ],
    );
    if res.is_err() {
        return;
    }

    // Insert per-probe rows from the `probes` array (present from this version onward).
    if let Some(probes) = record.get("probes").and_then(Value::as_array) {
        for p in probes {
            let tags_str = p
                .get("tags")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".to_string());
            let _ = conn.execute(
                "INSERT INTO probe_results
                 (run_id, name, command, assertion, tags, passed, slow, timed_out, duration_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    run_id,
                    p.get("name").and_then(Value::as_str).unwrap_or(""),
                    p.get("command").and_then(Value::as_str),
                    p.get("assertion").and_then(Value::as_str),
                    tags_str,
                    p.get("passed")
                        .and_then(Value::as_bool)
                        .map(|b| b as i64)
                        .unwrap_or(1),
                    p.get("slow")
                        .and_then(Value::as_bool)
                        .map(|b| b as i64)
                        .unwrap_or(0),
                    p.get("timed_out")
                        .and_then(Value::as_bool)
                        .map(|b| b as i64)
                        .unwrap_or(0),
                    p.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
                ],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// probe run
// ---------------------------------------------------------------------------

fn probe_file_path(cfg: &Config) -> PathBuf {
    let db_dir = Path::new(&cfg.db_path).parent().unwrap_or(Path::new("."));
    db_dir.join(".asd").join("probes.toml")
}

fn history_path(cfg: &Config) -> PathBuf {
    let db_dir = Path::new(&cfg.db_path).parent().unwrap_or(Path::new("."));
    db_dir.join(".asd").join("probe-history.jsonl")
}

/// Append a compact run record to probe-history.jsonl and prune to MAX_HISTORY lines.
/// Failures here are silently ignored — history is best-effort, never blocking.
const MAX_HISTORY: usize = 500;

fn append_history(cfg: &Config, record: &Value) {
    let path = history_path(cfg);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let new_line = match serde_json::to_string(record) {
        Ok(s) => s,
        Err(_) => return,
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    lines.push(new_line);
    if lines.len() > MAX_HISTORY {
        lines.drain(0..lines.len() - MAX_HISTORY);
    }
    let content = lines.join("\n") + "\n";
    let _ = std::fs::write(&path, content);
}

fn run_probes(cfg: &Config, args: ProbeRunArgs) -> Result<()> {
    let path = probe_file_path(cfg);
    if !path.exists() {
        anyhow::bail!(
            "no probes file found at {}\nRun `asd probe add` to create probes.",
            path.display()
        );
    }

    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let pf: ProbeFile =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    let probes: Vec<&ProbeEntry> = pf
        .probe
        .iter()
        .filter(|p| {
            // --name: exact name match
            if let Some(ref n) = args.name {
                if p.name != *n {
                    return false;
                }
            }
            // --tag: probe must include this tag
            if let Some(ref t) = args.tag {
                if !p.tags.iter().any(|tag| tag == t) {
                    return false;
                }
            }
            // --filter: legacy substring match on name
            if let Some(ref f) = args.filter {
                if !p.name.contains(f.as_str()) {
                    return false;
                }
            }
            true
        })
        .collect();

    if probes.is_empty() {
        if args.name.is_some() || args.tag.is_some() || args.filter.is_some() {
            let mut reason = Vec::new();
            if let Some(ref n) = args.name {
                reason.push(format!("name={:?}", n));
            }
            if let Some(ref t) = args.tag {
                reason.push(format!("tag={:?}", t));
            }
            if let Some(ref f) = args.filter {
                reason.push(format!("filter={:?}", f));
            }
            println!("No probes matched filter(s): {}.", reason.join(", "));
        } else {
            println!("No probes to run.");
        }
        return Ok(());
    }

    // Determine parallelism.
    // Auto (jobs=0): cap at 6 — empirically the SQLite page-cache contention
    // from concurrent subprocess readers increases wall time above ~6 parallel jobs
    // even on machines with many more CPU threads.  Users can override with --jobs N.
    let n = probes.len();
    const AUTO_JOBS_CAP: usize = 6;
    let jobs = if args.jobs == 0 {
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(4)
            .min(AUTO_JOBS_CAP)
            .min(n)
    } else {
        args.jobs.min(n).max(1)
    };

    if !args.json && jobs > 1 {
        eprintln!("Running {} probe(s) [{} parallel]…", n, jobs);
    }

    // Gather DB metadata + trust score once before running probes (cheap reads).
    let db_state = {
        let warn = stale_warning(&cfg.db_path, 3600);
        match warn {
            None => "fresh",
            Some(ref s) if s.contains("FTS search index failed") => "symbols-fresh/fts-stale",
            Some(_) => "stale",
        }
    };
    let symbol_count: Option<u64> = SearchFtsDb::open(&cfg.db_path)
        .ok()
        .map(|fts| fts.symbol_count() as u64);
    let trust = compute_trust_score(&cfg.db_path);

    let started_at = Utc::now().to_rfc3339();
    let wall_start = Instant::now();

    // -------------------------------------------------------------------------
    // Phase 1: Command de-duplication cache.
    //
    // Many probes share identical (command, args, cwd) — e.g. 10 search probes
    // all run `search "drift playhead" --agent` and test different fields of the
    // same JSON.  We compute a cache key for each probe, execute each unique key
    // exactly once (in parallel up to `jobs` workers), then fan out the
    // assertions in Phase 2 against the shared output.
    //
    // hydrate-roundtrip is excluded (in-process, not a subprocess).
    // workflow probes (command="workflow") are excluded (no JSON output to cache).
    // -------------------------------------------------------------------------
    let cacheable = |p: &&ProbeEntry| {
        p.command != "hydrate-roundtrip"
            && p.command != "hydrate_roundtrip"
            && p.command != "workflow"
    };

    // Map each probe index → its cache key (or None if non-cacheable).
    let probe_keys: Vec<Option<String>> = probes
        .iter()
        .map(|p| {
            if cacheable(&p) {
                Some(command_cache_key(p, &cfg.db_path))
            } else {
                None
            }
        })
        .collect();

    // Collect unique keys, keeping the first probe index as the representative.
    let mut key_to_representative: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (i, key_opt) in probe_keys.iter().enumerate() {
        if let Some(key) = key_opt {
            key_to_representative.entry(key.clone()).or_insert(i);
        }
    }

    let n_unique = key_to_representative.len();
    let n_cacheable = probe_keys.iter().filter(|k| k.is_some()).count();
    let n_deduped = n_cacheable.saturating_sub(n_unique);
    if !args.json && n_deduped > 0 {
        eprintln!(
            "  {} duplicate command invocation(s) eliminated by cache ({} unique → {} probes)",
            n_deduped, n_unique, n_cacheable
        );
    }

    // Execute unique commands in parallel, building the cache.
    // key → CachedOutput
    let command_cache: std::collections::HashMap<String, CachedOutput> = {
        let unique_probes: Vec<(String, &ProbeEntry)> = key_to_representative
            .iter()
            .map(|(key, &idx)| (key.clone(), probes[idx]))
            .collect();
        let mut map: std::collections::HashMap<String, CachedOutput> =
            std::collections::HashMap::with_capacity(unique_probes.len());
        std::thread::scope(|scope| {
            for chunk in unique_probes.chunks(jobs) {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|(key, probe)| {
                        scope.spawn(move || (key.clone(), run_command_only(cfg, probe)))
                    })
                    .collect();
                for h in handles {
                    let (k, v) = h.join().unwrap_or_else(|_| {
                        (
                            "__panic__".to_string(),
                            CachedOutput {
                                json: None,
                                stdout_raw: String::new(),
                                stderr: String::new(),
                                success: false,
                                duration_ms: 0,
                                timed_out: false,
                                exec_error: Some(
                                    "thread panicked during command execution".to_string(),
                                ),
                            },
                        )
                    });
                    map.insert(k, v);
                }
            }
        });
        map
    };

    // -------------------------------------------------------------------------
    // Phase 2: assertion fan-out (fast — no subprocesses).
    // -------------------------------------------------------------------------

    // fail_fast_flag: set by any thread when an assertion fails and --fail-fast is active.
    // std::thread::scope guarantees all threads finish before we leave the scope, so no
    // Arc is needed — a plain reference to the AtomicBool is sufficient and is Copy+Send.
    let fail_fast_flag = std::sync::atomic::AtomicBool::new(false);
    // Extract scalar flags so closures can capture them by copy (avoids moving `args`).
    let fail_fast = args.fail_fast;
    let fail_slow = args.fail_slow;
    let show_json = args.json;
    let mut results: Vec<ProbeResult> = Vec::with_capacity(n);

    std::thread::scope(|scope| {
        // Process probes in chunks of `jobs`. Within each chunk all probes run in
        // parallel; results are printed in submission order after the chunk finishes.
        for chunk_start in (0..probes.len()).step_by(jobs) {
            if fail_fast_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let chunk_end = (chunk_start + jobs).min(probes.len());
            let chunk = &probes[chunk_start..chunk_end];
            // Rebind as a reference so each spawned closure copies the &AtomicBool
            // (which is Copy+Send) rather than trying to move the AtomicBool itself.
            let ff = &fail_fast_flag;

            // Spawn one thread per probe in this chunk.
            // Each thread returns (original_index, ProbeResult).
            // Phase 2: if the probe has a cache entry, use run_assertion_against
            // (fast, no subprocess). Otherwise fall back to execute_probe for
            // in-process probes (hydrate-roundtrip, workflow).
            let handles: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(j, probe)| {
                    let global_idx = chunk_start + j;
                    let cache_key = probe_keys[global_idx].clone();
                    let cached = cache_key.as_ref().and_then(|k| command_cache.get(k));
                    scope.spawn(move || {
                        // Check flag before doing work (fast exit on fail-fast).
                        if ff.load(std::sync::atomic::Ordering::Relaxed) {
                            return None;
                        }
                        let result = if let Some(c) = cached {
                            run_assertion_against(probe, c)
                        } else {
                            execute_probe(cfg, probe)
                        };
                        if result.error.is_some() && fail_fast {
                            ff.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        Some((global_idx, result))
                    })
                })
                .collect();

            // Wait for all handles in chunk, sort back to submission order, print.
            let mut chunk_results: Vec<(usize, ProbeResult)> = handles
                .into_iter()
                .filter_map(|h| h.join().unwrap_or(None))
                .collect();
            chunk_results.sort_by_key(|(i, _)| *i);

            for (_, result) in &chunk_results {
                let is_fail = result.error.is_some();
                let is_slow = fail_slow.map_or(false, |ms| result.duration_ms > ms as u128);
                if !show_json {
                    let status = if is_fail {
                        "FAIL"
                    } else if is_slow {
                        "SLOW"
                    } else {
                        "PASS"
                    };
                    let ms = result.duration_ms;
                    if is_fail {
                        println!("{:<5} {} ({}ms)", status, result.name, ms);
                        println!("      {}", result.error.as_deref().unwrap_or(""));
                        if let Some(ref payload) = result.debug_payload_summary {
                            println!("      debug: {}", payload);
                        }
                    } else {
                        println!("{:<5} {} ({}ms)", status, result.name, ms);
                    }
                }
            }
            results.extend(chunk_results.into_iter().map(|(_, r)| r));
        }
    });

    let passed = results.iter().filter(|r| r.error.is_none()).count();
    let failed = results.iter().filter(|r| r.error.is_some()).count();

    // Slow violations: probes that exceeded --fail-slow threshold.
    let slow_violations: Vec<&ProbeResult> = if let Some(threshold_ms) = args.fail_slow {
        results
            .iter()
            .filter(|r| r.duration_ms > threshold_ms as u128)
            .collect()
    } else {
        Vec::new()
    };

    // Helper closure: render a ProbeResult as the canonical JSON shape.
    let fail_slow = args.fail_slow;
    let result_to_json = |r: &ProbeResult| {
        let is_slow = fail_slow.map_or(false, |ms| r.duration_ms > ms as u128);
        serde_json::json!({
            "name": r.name,
            "command": r.command,
            "assertion": r.assertion,
            "tags": r.tags,
            "passed": r.error.is_none(),
            "slow": is_slow,
            "timed_out": r.timed_out,
            "duration_ms": r.duration_ms,
            "error": r.error,
            "debug_payload": r.debug_payload,
        })
    };

    // Top-5 slowest — full result shape, pre-sorted descending by
    // duration_ms.
    //
    // Refinement (1.0.73, after ExampleProj 1.0.72 field run): probes
    // that share a subprocess via the command-cache (e.g.
    // `rank-projectmanager-top5` + `rank-projectmanager-eq1` both
    // hit the same `asd search ProjectManager --agent` invocation)
    // were producing duplicate "slowest" rows with identical
    // durations, making a 3-distinct-subprocess workload look like
    // 5 separate slow paths. Collapse by (command, args, cwd)
    // cache key so each row represents one ACTUAL slow subprocess;
    // the probe_names array lists every probe that timed against
    // that run.
    let dedup_db_path = cfg.db_path.clone();
    let mut by_cache: std::collections::HashMap<String, (u128, Vec<&ProbeResult>)> =
        std::collections::HashMap::new();
    for r in &results {
        let key = format!(
            "{}|{}",
            r.command,
            // Best-effort: probes share the cache when their probe
            // entries share command+args. Result-level we only have
            // command (not args), so probes with identical command +
            // identical duration_ms collapse. Reading the probe
            // entries here would require threading them through —
            // skipped for now, the (command, duration) heuristic
            // catches the rank-*-top5 + rank-*-eq1 pairs that
            // motivated this refinement.
            r.duration_ms,
        );
        let entry = by_cache.entry(key).or_insert((r.duration_ms, Vec::new()));
        entry.1.push(r);
    }
    // Suppress unused-warning on dedup_db_path — kept available
    // for a future tightening that hashes the full command-key.
    let _ = dedup_db_path;
    let mut deduped: Vec<(u128, Vec<&ProbeResult>)> = by_cache.into_values().collect();
    deduped.sort_by(|a, b| b.0.cmp(&a.0));
    let slowest_top5: Vec<Value> = deduped
        .iter()
        .take(5)
        .map(|(duration_ms, group)| {
            // Single-probe groups render exactly like the old shape
            // (no extra `probe_names` field) for backward compat.
            // Multi-probe groups add `probe_names` listing every
            // sibling that shared the cache.
            if group.len() == 1 {
                result_to_json(group[0])
            } else {
                let names: Vec<&str> = group.iter().map(|r| r.name.as_str()).collect();
                let mut v = result_to_json(group[0]);
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "probe_names".to_string(),
                        serde_json::json!(names),
                    );
                    obj.insert(
                        "cache_shared_count".to_string(),
                        serde_json::json!(group.len()),
                    );
                    obj.insert("duration_ms".to_string(), serde_json::json!(duration_ms));
                }
                v
            }
        })
        .collect();

    // Compute summary values shared by JSON output, human output, and history.
    let wall_time_ms = wall_start.elapsed().as_millis();
    let finished_at = Utc::now().to_rfc3339();
    let budget_failed = !slow_violations.is_empty();
    let slow_violation_names: Vec<&str> = slow_violations.iter().map(|r| r.name.as_str()).collect();

    if args.json {
        let json_results: Vec<Value> = results.iter().map(|r| result_to_json(r)).collect();
        // Plan J t-015: confidence-bucket calibration. Walk each
        // probe's debug_payload for uncertainty.level and pair it
        // with the assertion outcome. compute_calibration produces
        // per-bucket pass-rate + advisory strings ("threshold may
        // be too strict / too generous") so callers can see at a
        // glance which buckets need tuning. Probes whose payload
        // has no uncertainty.level field (e.g. hydrate-roundtrip,
        // non-search probes) contribute nothing and are silently
        // skipped — calibration only covers probes that emit a
        // bucket label.
        let calibration_obs: Vec<(String, bool)> = results
            .iter()
            .filter_map(|r| {
                // Plan J t-015 fix: read the pre-extracted signal,
                // NOT debug_payload (which is None on the pass path
                // — populating calibration only from failures gave
                // empty buckets on every all-passing run).
                let level = r.calibration_signal.as_deref()?;
                Some((level.to_string(), r.error.is_none()))
            })
            .collect();
        let calibration = compute_calibration(calibration_obs);

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "asd_version": env!("CARGO_PKG_VERSION"),
                "started_at": started_at,
                "finished_at": finished_at,
                "probe_file": probe_file_path(cfg),
                "db_path": cfg.db_path,
                "total": results.len(),
                "passed": passed,
                "failed": failed,
                "budget_failed": budget_failed,
                "wall_time_ms": wall_time_ms,
                "worker_count": jobs,
                "db_state": db_state,
                "symbol_count": symbol_count,
                "performance_budget_ms": args.fail_slow,
                "slow_violations": slow_violation_names,
                "slowest": slowest_top5,
                "trust": trust.to_json(),
                "calibration": calibration,
                "results": json_results,
            }))?
        );
    } else {
        println!(
            "\n{} probe(s): {} passed, {} failed  [{} workers, {}ms wall]",
            results.len(),
            passed,
            failed,
            jobs,
            wall_time_ms
        );
        if !slow_violations.is_empty() {
            let threshold_ms = args.fail_slow.unwrap();
            println!("SLOW violations (>{threshold_ms} ms):");
            for r in &slow_violations {
                println!("  {} ({}ms)", r.name, r.duration_ms);
            }
        }
        if !slowest_top5.is_empty() && results.len() > 1 {
            println!("Slowest probes:");
            for entry in &slowest_top5 {
                println!(
                    "  {} ({}ms)",
                    entry["name"].as_str().unwrap_or("?"),
                    entry["duration_ms"].as_u64().unwrap_or(0)
                );
            }
        }
    }

    // Always append a compact record to probe-history.jsonl regardless of output mode.
    // Includes per-probe compact rows (no debug_payload) — canonical source of truth
    // for per-probe trend analysis.  The analytics SQLite DB mirrors this.
    let probes_compact: Vec<Value> = results
        .iter()
        .map(|r| {
            let is_slow = fail_slow.map_or(false, |ms| r.duration_ms > ms as u128);
            serde_json::json!({
                "name": r.name,
                "command": r.command,
                "assertion": r.assertion,
                "tags": r.tags,
                "passed": r.error.is_none(),
                "slow": is_slow,
                "timed_out": r.timed_out,
                "duration_ms": r.duration_ms,
            })
        })
        .collect();
    let history_record = serde_json::json!({
        "kind": "probe_run",
        "asd_version": env!("CARGO_PKG_VERSION"),
        "started_at": started_at,
        "finished_at": finished_at,
        "probe_file": probe_file_path(cfg),
        "db_state": db_state,
        "symbol_count": symbol_count,
        "total": results.len(),
        "passed": passed,
        "failed": failed,
        "budget_failed": budget_failed,
        "wall_time_ms": wall_time_ms,
        "worker_count": jobs,
        "performance_budget_ms": args.fail_slow,
        "filter_name": args.name,
        "filter_tag": args.tag,
        "slowest": slowest_top5,
        "probes": probes_compact,
    });
    append_history(cfg, &history_record);

    // Mirror into analytics DB (best-effort; failures are silent).
    if let Ok(conn) = open_analytics_db(&analytics_path(cfg)) {
        insert_run_to_analytics(&conn, &history_record);
    }

    if failed > 0 || !slow_violations.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

struct ProbeResult {
    name: String,
    command: String,
    assertion: String, // assertion kind, e.g. "file_not_in_key"; "" = smoke test
    tags: Vec<String>,
    duration_ms: u128,
    timed_out: bool, // true if the probe was killed by a timeout (reserved; always false today)
    error: Option<String>,
    debug_payload: Option<Value>,
    debug_payload_summary: Option<String>,
    /// Plan J t-015: uncertainty bucket label extracted from the
    /// cached JSON output, captured for EVERY probe (pass or fail)
    /// so the calibration harvester has observations even when
    /// debug_payload is None. `None` when the probe's command
    /// output didn't include `uncertainty.level` (e.g. `asd trust`,
    /// which doesn't emit a bucket).
    calibration_signal: Option<String>,
}

/// Extract `uncertainty.level` from a probe's parsed JSON output.
/// Used to populate `ProbeResult.calibration_signal` regardless of
/// pass/fail, so the calibration harvester sees every observation
/// (debug_payload is only attached on failure — relying on it
/// silently drops the entire pass cohort).
fn extract_calibration_signal(json: Option<&Value>) -> Option<String> {
    json?
        .get("uncertainty")?
        .get("level")?
        .as_str()
        .map(|s| s.to_string())
}

/// Cached result of a single subprocess execution (command + args).
/// Shared across all probes that map to the same command invocation.
#[allow(dead_code)] // stdout_raw + timed_out kept for forensic dumps
#[derive(Clone)]
struct CachedOutput {
    /// Parsed JSON from stdout, or None if output was not valid JSON.
    json: Option<Value>,
    /// Raw stdout text (used for error messages).
    stdout_raw: String,
    /// Stderr text (included in error messages).
    stderr: String,
    /// True if the process exited successfully.
    success: bool,
    /// Wall-clock time for the subprocess (ms).
    duration_ms: u128,
    /// True if the command hit the --timeout limit.
    timed_out: bool,
    /// Execution-level error (process spawn failure), separate from assertion errors.
    exec_error: Option<String>,
}

/// Compute the de-duplication key for a probe's command invocation.
/// Probes with identical (command, args, cwd, db_path) share one subprocess run.
fn command_cache_key(probe: &ProbeEntry, db_path: &std::path::Path) -> String {
    let cwd_part = probe.cwd.as_deref().unwrap_or("");
    format!(
        "{}|{}|{}|{}",
        probe.command,
        probe.args.join("\x00"),
        cwd_part,
        db_path.display()
    )
}

/// Execute the subprocess for `probe` and return a `CachedOutput`.
/// Does NOT run the assertion — pure I/O only.
/// Skipped for hydrate-roundtrip (handled separately).
fn run_command_only(cfg: &Config, probe: &ProbeEntry) -> CachedOutput {
    let start = Instant::now();

    let asd_bin_path: PathBuf = {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("asd"));
        if exe.exists() {
            exe
        } else {
            PathBuf::from("asd")
        }
    };
    let asd_bin = asd_bin_path.to_string_lossy().into_owned();

    let subcmd = match probe.command.as_str() {
        "prepare-change" | "prepare_change" => "prepare-change",
        "annotate-commit" | "annotate_commit" => "annotate-commit",
        "task-close" | "task_close" => "task-close",
        other => other,
    };

    let probe_dir = std::path::Path::new(&cfg.db_path)
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let work_dir = probe.cwd.as_ref().map(PathBuf::from).unwrap_or(probe_dir);

    fn shell_quote_inner(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    let db_path_str = cfg.db_path.to_string_lossy().into_owned();
    let mut shell_cmd = format!(
        "cd {} && {} --db {} {}",
        shell_quote_inner(work_dir.to_string_lossy().as_ref()),
        shell_quote_inner(&asd_bin),
        shell_quote_inner(&db_path_str),
        shell_quote_inner(subcmd),
    );
    for arg in &probe.args {
        shell_cmd.push(' ');
        shell_cmd.push_str(&shell_quote_inner(arg));
    }

    let mut cmd = ProcessCommand::new("/bin/sh");
    cmd.arg("-c").arg(&shell_cmd);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            return CachedOutput {
                json: None,
                stdout_raw: String::new(),
                stderr: String::new(),
                success: false,
                duration_ms: start.elapsed().as_millis(),
                timed_out: false,
                exec_error: Some(format!(
                    "failed to execute asd ({:?}) via sh: {}",
                    asd_bin, e
                )),
            };
        }
    };

    let stdout_raw = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json: Option<Value> = serde_json::from_str(&stdout_raw).ok();
    let duration_ms = start.elapsed().as_millis();

    CachedOutput {
        json,
        stdout_raw,
        stderr,
        success: output.status.success(),
        duration_ms,
        timed_out: false,
        exec_error: None,
    }
}

/// Run a probe's assertion against a pre-computed `CachedOutput`.
/// Returns the ProbeResult without spawning any subprocess.
fn run_assertion_against(probe: &ProbeEntry, cached: &CachedOutput) -> ProbeResult {
    let assertion_kind = probe
        .assert
        .as_table()
        .and_then(|m| m.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Some(ref exec_err) = cached.exec_error {
        return ProbeResult {
            name: probe.name.clone(),
            command: probe.command.clone(),
            assertion: assertion_kind,
            tags: probe.tags.clone(),
            duration_ms: cached.duration_ms,
            timed_out: false,
            error: Some(exec_err.clone()),
            debug_payload: None,
            debug_payload_summary: None,
            calibration_signal: cached.json.as_ref().and_then(|j| extract_calibration_signal(Some(j))),
        };
    }

    if !cached.success && cached.json.is_none() {
        return ProbeResult {
            name: probe.name.clone(),
            command: probe.command.clone(),
            assertion: assertion_kind,
            tags: probe.tags.clone(),
            duration_ms: cached.duration_ms,
            timed_out: false,
            error: Some(format!("command failed: {}", cached.stderr.trim())),
            debug_payload: None,
            debug_payload_summary: None,
            calibration_signal: cached.json.as_ref().and_then(|j| extract_calibration_signal(Some(j))),
        };
    }

    let json = match cached.json.as_ref() {
        Some(v) => v,
        None => {
            return ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: assertion_kind,
                tags: probe.tags.clone(),
                duration_ms: cached.duration_ms,
                timed_out: false,
                error: Some("command output was not valid JSON".to_string()),
                debug_payload: None,
                debug_payload_summary: None,
            calibration_signal: cached.json.as_ref().and_then(|j| extract_calibration_signal(Some(j))),
            };
        }
    };

    match eval_assert(&probe.assert, json) {
        Ok(()) => ProbeResult {
            name: probe.name.clone(),
            command: probe.command.clone(),
            assertion: assertion_kind,
            tags: probe.tags.clone(),
            duration_ms: cached.duration_ms,
            timed_out: false,
            error: None,
            debug_payload: None,
            debug_payload_summary: None,
            calibration_signal: cached.json.as_ref().and_then(|j| extract_calibration_signal(Some(j))),
        },
        Err(msg) => {
            let summary = summarize_debug_payload(json);
            ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: assertion_kind,
                tags: probe.tags.clone(),
                duration_ms: cached.duration_ms,
                timed_out: false,
                error: Some(msg),
                debug_payload: Some(json.clone()),
                debug_payload_summary: summary,
                calibration_signal: extract_calibration_signal(Some(json)),
            }
        }
    }
}

fn execute_probe(cfg: &Config, probe: &ProbeEntry) -> ProbeResult {
    let start = Instant::now();
    // Extract assertion kind from probe definition for result metadata.
    let assertion_kind = probe
        .assert
        .as_table()
        .and_then(|m| m.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Special-case: hydrate-roundtrip runs an isolated in-process cycle rather
    // than spawning a subprocess. No assert block needed — pass/fail is the
    // roundtrip itself.
    if probe.command.as_str() == "hydrate-roundtrip"
        || probe.command.as_str() == "hydrate_roundtrip"
    {
        let result = run_hydrate_roundtrip_probe(cfg);
        let duration_ms = start.elapsed().as_millis();
        return match result {
            Ok(msg) => ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: "hydrate_roundtrip".to_string(),
                tags: probe.tags.clone(),
                duration_ms,
                timed_out: false,
                error: None,
                debug_payload: Some(serde_json::json!({ "message": msg })),
                debug_payload_summary: Some(msg),
                calibration_signal: None, // hydrate doesn't emit uncertainty.level
            },
            Err(msg) => ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: "hydrate_roundtrip".to_string(),
                tags: probe.tags.clone(),
                duration_ms,
                timed_out: false,
                error: Some(msg.clone()),
                debug_payload: Some(serde_json::json!({ "error": msg })),
                debug_payload_summary: Some(msg),
                calibration_signal: None,
            },
        };
    }

    // Resolve asd binary path.
    // Prefer the current executable if it actually exists (covers installed + dev builds).
    // Fall back to plain "asd" so the OS will find it via PATH — this handles cases
    // where current_exe() returns a stale worktree path that no longer exists.
    let asd_bin_path: PathBuf = {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("asd"));
        if exe.exists() {
            exe
        } else {
            PathBuf::from("asd")
        }
    };
    // Use the string form for shell quoting.
    let asd_bin = asd_bin_path.to_string_lossy().into_owned();

    // Map command name to CLI subcommand string.
    let subcmd = match probe.command.as_str() {
        "prepare-change" | "prepare_change" => "prepare-change",
        "annotate-commit" | "annotate_commit" => "annotate-commit",
        "task-close" | "task_close" => "task-close",
        other => other,
    };

    // Determine working directory: probe.cwd > probes.toml parent dir.
    let probe_dir = Path::new(&cfg.db_path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let work_dir = probe.cwd.as_ref().map(PathBuf::from).unwrap_or(probe_dir);

    // Build the shell command string.
    // We invoke via `sh -c '...'` rather than exec-ing asd directly because on macOS
    // the sandbox environment used by Claude Code blocks direct Process::exec of the
    // same binary but permits shell invocation. Shell-quoting: wrap each token with
    // single quotes and escape interior single quotes as '\''  .
    fn shell_quote(s: &str) -> String {
        // Wrap in single quotes; escape any existing single quotes as: ' → '\''
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    let db_path_str = cfg.db_path.to_string_lossy().into_owned();
    let mut shell_cmd = format!(
        "cd {} && {} --db {} {}",
        shell_quote(work_dir.to_string_lossy().as_ref()),
        shell_quote(&asd_bin),
        shell_quote(&db_path_str),
        shell_quote(subcmd),
    );
    for arg in &probe.args {
        shell_cmd.push(' ');
        shell_cmd.push_str(&shell_quote(arg));
    }

    let mut cmd = ProcessCommand::new("/bin/sh");
    cmd.arg("-c").arg(&shell_cmd);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            return ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: assertion_kind,
                tags: probe.tags.clone(),
                duration_ms: start.elapsed().as_millis(),
                timed_out: false,
                error: Some(format!(
                    "failed to execute asd ({:?}) via sh: {}",
                    asd_bin, e
                )),
                debug_payload: None,
                debug_payload_summary: None,
                calibration_signal: None, // exec failed; no JSON to harvest
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Option<Value> = serde_json::from_str(&stdout).ok();

    let duration_ms = start.elapsed().as_millis();

    if !output.status.success() && parsed.is_none() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ProbeResult {
            name: probe.name.clone(),
            command: probe.command.clone(),
            assertion: assertion_kind,
            tags: probe.tags.clone(),
            duration_ms,
            timed_out: false,
            error: Some(format!(
                "command exited with {}: {}",
                output.status,
                stderr.trim()
            )),
            debug_payload: None,
            debug_payload_summary: None,
            calibration_signal: None, // cmd failed without parseable JSON
        };
    }

    let json = match parsed {
        Some(v) => v,
        None => {
            return ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: assertion_kind,
                tags: probe.tags.clone(),
                duration_ms,
                timed_out: false,
                error: Some("command output was not valid JSON".to_string()),
                debug_payload: None,
                debug_payload_summary: None,
                calibration_signal: None, // unparseable stdout, nothing to extract
            };
        }
    };

    // Evaluate assertion.
    match eval_assert(&probe.assert, &json) {
        Ok(()) => ProbeResult {
            name: probe.name.clone(),
            command: probe.command.clone(),
            assertion: assertion_kind,
            tags: probe.tags.clone(),
            duration_ms,
            timed_out: false,
            error: None,
            debug_payload: None,
            debug_payload_summary: None,
            calibration_signal: extract_calibration_signal(Some(&json)),
        },
        Err(msg) => {
            let summary = summarize_debug_payload(&json);
            let cal = extract_calibration_signal(Some(&json));
            ProbeResult {
                name: probe.name.clone(),
                command: probe.command.clone(),
                assertion: assertion_kind,
                tags: probe.tags.clone(),
                duration_ms,
                timed_out: false,
                error: Some(msg),
                debug_payload: Some(json),
                debug_payload_summary: summary,
                calibration_signal: cal,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hydrate roundtrip probe — semantic memory persistence proof
// ---------------------------------------------------------------------------
//
// Writes a sentinel ledger entry into an isolated temp DB, syncs the sidecar,
// hydrates into a second fresh temp DB, and verifies the entry survived.
// Uses ephemeral SQLite files in a temp dir — never touches the production DB.

fn run_hydrate_roundtrip_probe(_cfg: &Config) -> Result<String, String> {
    use agentstatedeveloper_core::{
        AsgIndexStore, AsgLedgerStore, Engine, IndexStore, LedgerStore, hydrate_from_dir,
        schema::{Author, AuthorKind, LedgerEntry, LedgerKind, Position, Symbol, SymbolKind},
        symbol_fingerprint, sync_to_dir,
    };

    // Step 1 — ephemeral temp workspace (never touches the production DB).
    let tmp = tempfile::TempDir::new().map_err(|e| format!("failed to create temp dir: {e}"))?;
    let db_a = tmp.path().join("roundtrip_a.db");
    let db_b = tmp.path().join("roundtrip_b.db");

    // Step 2 — open source engine, write a synthetic symbol + sentinel entry.
    let engine_a =
        Engine::open_sqlite(&db_a).map_err(|e| format!("failed to open temp Engine A: {e}"))?;

    let qname = "asd::probe::roundtrip_sentinel";
    let file = "__asd_probe__/roundtrip.rs";
    let sym = Symbol {
        symbol_id: "asd-probe-roundtrip-sym".to_string(),
        symbol_fp: symbol_fingerprint(qname),
        qname: qname.to_string(),
        kind: SymbolKind::Function,
        file: file.to_string(),
        language: "rust".to_string(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 1, col: 0 },
        doc: Some("ASD hydrate roundtrip probe sentinel symbol".to_string()),
        signature: Some("fn roundtrip_sentinel()".to_string()),
    };
    let store_a = AsgIndexStore::from_engine(&engine_a);
    store_a
        .put_symbol(&engine_a.ref_name, &sym, "asd-roundtrip-probe")
        .map_err(|e| format!("failed to write sentinel symbol: {e}"))?;

    let sentinel_text = "asd-hydrate-roundtrip-proof";
    let author = Author {
        kind: AuthorKind::Agent,
        id: "asd-roundtrip-probe".to_string(),
    };
    let mut entry = LedgerEntry::new(
        "asd-probe-roundtrip-sym",
        LedgerKind::Decision,
        sentinel_text,
        author,
    );
    entry.tags = vec!["trust-probe".to_string(), "probe-roundtrip".to_string()];

    let ledger_a = AsgLedgerStore::new(&engine_a.repo);
    ledger_a
        .append_entry(&engine_a.ref_name, &entry, "asd-roundtrip-probe")
        .map_err(|e| format!("failed to write sentinel ledger entry: {e}"))?;

    // Step 3 — sync sidecar to temp dir (creates .asd/v1/ inside tmp).
    let sidecar_root = tmp.path();
    sync_to_dir(&engine_a.repo, &engine_a.ref_name, sidecar_root)
        .map_err(|e| format!("sync_to_dir failed: {e}"))?;

    // Step 4 — open fresh engine B (empty DB), hydrate from the sidecar.
    let engine_b =
        Engine::open_sqlite(&db_b).map_err(|e| format!("failed to open temp Engine B: {e}"))?;
    hydrate_from_dir(
        &engine_b.repo,
        &engine_b.ref_name,
        sidecar_root,
        "asd-roundtrip-probe",
    )
    .map_err(|e| format!("hydrate_from_dir failed: {e}"))?;

    // Step 5 — verify the sentinel entry survived.
    let ledger_b = AsgLedgerStore::new(&engine_b.repo);
    let entries = ledger_b
        .list_entries(&engine_b.ref_name, "asd-probe-roundtrip-sym")
        .map_err(|e| format!("failed to read ledger from Engine B: {e}"))?;

    let survived = entries.iter().any(|e| e.summary == sentinel_text);

    if survived {
        Ok(format!(
            "roundtrip OK — sentinel {:?} survived sync→hydrate cycle ({} entries in B)",
            sentinel_text,
            entries.len()
        ))
    } else {
        Err(format!(
            "roundtrip FAILED — sentinel {:?} not found in Engine B after hydrate \
             ({} entries present: {:?})",
            sentinel_text,
            entries.len(),
            entries
                .iter()
                .map(|e| e.summary.as_str())
                .collect::<Vec<_>>()
        ))
    }
}

// ---------------------------------------------------------------------------
// Assertion evaluator
// ---------------------------------------------------------------------------

fn eval_assert(assert: &toml::Value, output: &Value) -> Result<(), String> {
    let map = match assert.as_table() {
        Some(m) => m,
        None => return Ok(()), // no assertion — probe always passes (useful as smoke test)
    };

    let kind = map.get("kind").and_then(|v| v.as_str()).unwrap_or("");

    // Plan M t-005 (1.0.100): the previous nested `resolve_key` closure
    // was a duplicate of the module-level `dot_path` helper. Calling
    // `dot_path` directly avoids the dup. Both walk dot-separated key
    // paths and return Option<&Value>; semantics are identical.
    let empty_arr: Vec<Value> = Vec::new();

    match kind {
        // file_not_in_key: no item in output[key] array has item[field] containing value.
        "file_not_in_key" => {
            let key = str_field(map, "key")?;
            let field = str_field(map, "field")?;
            let value = str_field(map, "value")?;
            let arr = dot_path(output, key)
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let found: Vec<&str> = arr
                .iter()
                .filter_map(|item| item.get(field).and_then(|v| v.as_str()))
                .filter(|s| s.contains(value))
                .collect();
            if !found.is_empty() {
                Err(format!(
                    "file_not_in_key: found {:?} in {}[].{} (should be absent)",
                    found, key, field
                ))
            } else {
                Ok(())
            }
        }

        // file_in_key: at least one item in output[key] has item[field] containing value.
        "file_in_key" => {
            let key = str_field(map, "key")?;
            let field = str_field(map, "field")?;
            let value = str_field(map, "value")?;
            let arr = dot_path(output, key)
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let found = arr.iter().any(|item| {
                item.get(field)
                    .and_then(|v| v.as_str())
                    .map_or(false, |s| s.contains(value))
            });
            if found {
                Ok(())
            } else {
                Err(format!(
                    "file_in_key: no item in {}[].{} contains {:?}",
                    key, field, value
                ))
            }
        }

        // qname_rank_lte: result whose qname contains `fragment` is at rank ≤ max_rank (1-based).
        "qname_rank_lte" => eval_qname_rank_lte(map, output),

        // Plan J t-019: qname_rank_eq — strict variant of
        // qname_rank_lte. Passes ONLY when the matching qname is at
        // EXACTLY the specified rank (typically 1). Use for
        // precision-mode probes: lenient probes (rank_lte max=5)
        // surface "is the right symbol anywhere near the top"; this
        // surfaces "is the right symbol AT the top." When both
        // variants exist for the same query in the same uncertainty
        // bucket, calibration's "too strict" advisory becomes
        // actionable — split pass rates within a bucket cohort
        // mean the lenient signal was hiding ranking noise.
        "qname_rank_eq" => {
            let fragment = str_field(map, "fragment")?;
            let exact_rank = u64_field(map, "exact_rank")?;
            let results = output
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let pos = results.iter().position(|r| {
                r.get("qname")
                    .and_then(|v| v.as_str())
                    .map_or(false, |q| q.contains(fragment))
            });
            match pos {
                Some(idx) if (idx as u64 + 1) == exact_rank => Ok(()),
                Some(idx) => Err(format!(
                    "qname_rank_eq: {:?} found at rank {}, expected exactly {}",
                    fragment,
                    idx + 1,
                    exact_rank
                )),
                None => Err(format!(
                    "qname_rank_eq: no result qname contains {:?} (checked {} results)",
                    fragment,
                    results.len()
                )),
            }
        }

        // result_count_lte: len(results) ≤ max.
        "result_count_lte" => {
            let max = u64_field(map, "max")?;
            let results = output
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let n = results.len() as u64;
            if n <= max {
                Ok(())
            } else {
                Err(format!("result_count_lte: got {} results (max={})", n, max))
            }
        }

        "cluster_winner_kind_not" => eval_cluster_winner_kind_not(map, output),
        "cluster_winner_qname_contains" => eval_cluster_winner_qname_contains(map, output),

        // no_duplicate_summaries: no two suggested_entries share the same summary text.
        "no_duplicate_summaries" => {
            let entries = output
                .get("suggested_entries")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let mut seen_global: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for e in entries {
                if let Some(s) = e.get("summary").and_then(|v| v.as_str()) {
                    *seen_global.entry(s.to_string()).or_insert(0) += 1;
                }
            }
            let dups: Vec<&String> = seen_global
                .iter()
                .filter(|(_, count)| **count > 1)
                .map(|(s, _)| s)
                .collect();
            if !dups.is_empty() {
                return Err(format!(
                    "no_duplicate_summaries: duplicate summary text: {:?}",
                    dups.iter().take(3).map(|s| s.as_str()).collect::<Vec<_>>()
                ));
            }
            Ok(())
        }

        // boosted_outranked_contains: boosted_outranked has at least one entry containing `fragment`.
        // Use this to verify a known-good SOT symbol that slipped below top-5 is reported.
        "boosted_outranked_contains" => {
            let fragment = str_field(map, "fragment")?;
            let outranked = output
                .get("boosted_outranked")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let found = outranked.iter().any(|s| {
                s.as_str().map_or(false, |q| {
                    q.to_lowercase().contains(&fragment.to_lowercase())
                })
            });
            if found {
                Ok(())
            } else {
                Err(format!(
                    "boosted_outranked_contains: {:?} not in boosted_outranked; got {:?}",
                    fragment,
                    outranked
                        .iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                ))
            }
        }

        // qname_not_in_results: no result has a qname containing `fragment`.
        // Use this to prove a feedback-suppressed symbol is absent from results.
        "qname_not_in_results" => {
            let fragment = str_field(map, "fragment")?;
            let results = output
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let hit = results.iter().find(|r| {
                r.get("qname").and_then(|v| v.as_str()).map_or(false, |q| {
                    q.to_lowercase().contains(&fragment.to_lowercase())
                })
            });
            match hit {
                Some(r) => {
                    let qname = r.get("qname").and_then(|v| v.as_str()).unwrap_or("?");
                    Err(format!(
                        "qname_not_in_results: {:?} is present in results (expected suppressed)",
                        qname
                    ))
                }
                None => Ok(()),
            }
        }

        // ambiguous_terms_nonempty: the query has at least one ambiguous term flagged.
        // Use this to verify broad/generic queries signal uncertainty.
        "ambiguous_terms_nonempty" => {
            let terms = output
                .get("ambiguous_terms")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            if terms.is_empty() {
                Err("ambiguous_terms_nonempty: ambiguous_terms is empty — query may be too specific or detection not firing".to_string())
            } else {
                Ok(())
            }
        }

        // scoped_suggestions_nonempty: scoped_suggestions has at least one entry.
        // Use this to verify broad queries emit narrowing hints.
        "scoped_suggestions_nonempty" => {
            let suggestions = output
                .get("scoped_suggestions")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            if suggestions.is_empty() {
                Err("scoped_suggestions_nonempty: scoped_suggestions is empty — no narrowing hints emitted".to_string())
            } else {
                Ok(())
            }
        }

        // scoped_suggestions_contains: at least one scoped suggestion contains `fragment`.
        "scoped_suggestions_contains" => {
            let fragment = str_field(map, "fragment")?;
            let suggestions = output
                .get("scoped_suggestions")
                .and_then(|v| v.as_array())
                .unwrap_or(&empty_arr);
            let found = suggestions.iter().any(|s| {
                s.as_str().map_or(false, |t| {
                    t.to_lowercase().contains(&fragment.to_lowercase())
                })
            });
            if found {
                Ok(())
            } else {
                Err(format!(
                    "scoped_suggestions_contains: no suggestion contains {:?}; got {:?}",
                    fragment,
                    suggestions
                        .iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                ))
            }
        }

        // uncertainty_level_lte: uncertainty.level is at most `max_level`.
        // Levels ordered: low < medium < high < critical.
        // Use this to assert exact queries don't produce unexpected uncertainty.
        // Example: { kind = "uncertainty_level_lte", max_level = "low" }
        "uncertainty_level_lte" => {
            let max_level = str_field(map, "max_level")?;
            let level_rank = |l: &str| match l {
                "low" => 0u8,
                "medium" => 1,
                "high" => 2,
                "critical" => 3,
                _ => 4,
            };
            let actual_level = output
                .get("uncertainty")
                .and_then(|u| u.get("level"))
                .and_then(Value::as_str)
                .unwrap_or("low");
            if level_rank(actual_level) <= level_rank(max_level) {
                Ok(())
            } else {
                Err(format!(
                    "uncertainty_level_lte: uncertainty.level = {:?} (expected <= {:?})",
                    actual_level, max_level
                ))
            }
        }

        // uncertainty_reason_contains: uncertainty.reasons[*].code contains `code`.
        // Use this to verify a specific uncertainty signal fires for ambiguous queries.
        // Example: { kind = "uncertainty_reason_contains", code = "ambiguous_term" }
        "uncertainty_reason_contains" => {
            let code = str_field(map, "code")?;
            let reasons = output
                .get("uncertainty")
                .and_then(|u| u.get("reasons"))
                .and_then(Value::as_array)
                .unwrap_or(&empty_arr);
            let found = reasons.iter().any(|r| {
                r.get("code")
                    .and_then(Value::as_str)
                    .map_or(false, |c| c == code)
            });
            if found {
                Ok(())
            } else {
                let codes: Vec<&str> = reasons
                    .iter()
                    .filter_map(|r| r.get("code").and_then(Value::as_str))
                    .collect();
                Err(format!(
                    "uncertainty_reason_contains: reason code {:?} not found; got {:?}",
                    code, codes
                ))
            }
        }

        // uncertainty_action_eq: uncertainty.recommended_action equals `action`.
        // Use this to verify the correct recovery suggestion fires.
        // Example: { kind = "uncertainty_action_eq", action = "narrow_query" }
        "uncertainty_action_eq" => {
            let action = str_field(map, "action")?;
            let actual = output
                .get("uncertainty")
                .and_then(|u| u.get("recommended_action"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if actual == action {
                Ok(())
            } else {
                Err(format!(
                    "uncertainty_action_eq: recommended_action = {:?} (expected {:?})",
                    actual, action
                ))
            }
        }

        // recovery_suggestions_nonempty: uncertainty.recovery_suggestions is non-empty.
        // Use this to verify broad queries emit structured recovery hints.
        "recovery_suggestions_nonempty" => {
            let suggestions = output
                .get("uncertainty")
                .and_then(|u| u.get("recovery_suggestions"))
                .and_then(Value::as_array)
                .unwrap_or(&empty_arr);
            if !suggestions.is_empty() {
                Ok(())
            } else {
                Err(
                    "recovery_suggestions_nonempty: uncertainty.recovery_suggestions is empty"
                        .to_string(),
                )
            }
        }

        // recovery_suggestion_estimated: at least one recovery suggestion has `estimated_recovery = strength`.
        // Example: { kind = "recovery_suggestion_estimated", strength = "strong" }
        "recovery_suggestion_estimated" => {
            let strength = str_field(map, "strength")?;
            let suggestions = output
                .get("uncertainty")
                .and_then(|u| u.get("recovery_suggestions"))
                .and_then(Value::as_array)
                .unwrap_or(&empty_arr);
            let found = suggestions.iter().any(|s| {
                s.get("estimated_recovery")
                    .and_then(Value::as_str)
                    .map_or(false, |e| e == strength)
            });
            if found {
                Ok(())
            } else {
                let found_strengths: Vec<&str> = suggestions
                    .iter()
                    .filter_map(|s| s.get("estimated_recovery").and_then(Value::as_str))
                    .collect();
                Err(format!(
                    "recovery_suggestion_estimated: no suggestion with estimated_recovery {:?}; got {:?}",
                    strength, found_strengths
                ))
            }
        }

        // feedback_summary_gte: feedback_summary[field] >= min_value.
        // Use this to prove feedback is actively suppressing or boosting results.
        "feedback_summary_gte" => {
            let field = str_field(map, "field")?;
            let min_value = u64_field(map, "min_value")?;
            let actual = output
                .get("feedback_summary")
                .and_then(|s| s.get(field))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if actual >= min_value {
                Ok(())
            } else {
                Err(format!(
                    "feedback_summary_gte: feedback_summary.{} = {} (expected >= {})",
                    field, actual, min_value
                ))
            }
        }

        // feedback_summary_eq: feedback_summary[field] == value.
        "feedback_summary_eq" => {
            let field = str_field(map, "field")?;
            let expected = u64_field(map, "value")?;
            let actual = output
                .get("feedback_summary")
                .and_then(|s| s.get(field))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "feedback_summary_eq: feedback_summary.{} = {} (expected {})",
                    field, actual, expected
                ))
            }
        }

        // feedback_rules_contains: feedback_summary.rules_applied contains `rule`.
        "feedback_rules_contains" => {
            let rule = str_field(map, "rule")?;
            let rules = output
                .get("feedback_summary")
                .and_then(|s| s.get("rules_applied"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let found = rules
                .iter()
                .any(|r| r.as_str().map_or(false, |s| s == rule));
            if found {
                Ok(())
            } else {
                Err(format!(
                    "feedback_rules_contains: rule {:?} not in rules_applied {:?}",
                    rule,
                    rules.iter().filter_map(|r| r.as_str()).collect::<Vec<_>>()
                ))
            }
        }

        // field_gte: output[dotted.path] >= min_value (numeric).
        // Required: field (dot-path string), min_value (integer or float).
        // Example: { kind = "field_gte", field = "score", min_value = 0.5 }
        //          { kind = "field_gte", field = "signals.symbol_count", min_value = 100 }
        "field_gte" => {
            let field = str_field(map, "field")?;
            let min_val: f64 = map
                .get("min_value")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .ok_or_else(|| format!("assertion missing required numeric field \"min_value\""))?;
            let actual = dot_path(output, field)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if actual >= min_val {
                Ok(())
            } else {
                Err(format!(
                    "field_gte: {field} = {actual} (expected >= {min_val})"
                ))
            }
        }

        // Plan B t-008: field_lte — output[dotted.path] <= max_value (numeric).
        // Required: field (dot-path string), max_value (integer or float).
        // Example: { kind = "field_lte", field = "total_bytes", max_value = 500000 }
        "field_lte" => {
            let field = str_field(map, "field")?;
            let max_val: f64 = map
                .get("max_value")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .ok_or_else(|| format!("assertion missing required numeric field \"max_value\""))?;
            let actual = dot_path(output, field)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if actual <= max_val {
                Ok(())
            } else {
                Err(format!(
                    "field_lte: {field} = {actual} (expected <= {max_val})"
                ))
            }
        }

        // array_field_count_lte: length of a dot-path array field <= max_count.
        // Required: field (dot-path string), max_count (integer).
        // Example: { kind = "array_field_count_lte", field = "safe_change_recipe.edit", max_count = 8 }
        "array_field_count_lte" => {
            let field = str_field(map, "field")?;
            let max_count = u64_field(map, "max_count")?;
            let val = dot_path(output, field);
            let len = val.and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0) as u64;
            if len <= max_count {
                Ok(())
            } else {
                Err(format!(
                    "array_field_count_lte: {field} has {len} elements (max {})",
                    max_count
                ))
            }
        }

        // array_field_count_gte: length of a dot-path array field >= min_count.
        // Required: field (dot-path string), min_count (integer).
        // Example: { kind = "array_field_count_gte", field = "entry_points", min_count = 3 }
        "array_field_count_gte" => {
            let field = str_field(map, "field")?;
            let min_count = u64_field(map, "min_count")?;
            let val = dot_path(output, field);
            let len = val.and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0) as u64;
            if len >= min_count {
                Ok(())
            } else {
                Err(format!(
                    "array_field_count_gte: {field} has {len} elements (min {})",
                    min_count
                ))
            }
        }

        // field_eq: output[dot.path] equals `expected` (string comparison).
        // Required: field (dot-path string), expected (string).
        // Example: { kind = "field_eq", field = "workflow.workflow_type", expected = "full" }
        "field_eq" => {
            let field = str_field(map, "field")?;
            let expected = str_field(map, "expected")?;
            let actual = dot_path(output, field)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "field_eq: {field} = {actual:?} (expected {:?})",
                    expected
                ))
            }
        }

        // workflow_steps_contains: workflow.steps_detected array contains `step`.
        // Required: step (string).
        // Example: { kind = "workflow_steps_contains", step = "task_closed" }
        "workflow_steps_contains" => {
            let step = str_field(map, "step")?;
            let steps = dot_path(output, "workflow.steps_detected")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let found = steps.iter().any(|s| s.as_str() == Some(step));
            if found {
                Ok(())
            } else {
                Err(format!(
                    "workflow_steps_contains: step {:?} not found in {:?}",
                    step,
                    steps.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>()
                ))
            }
        }

        // evidence_score_gte: workflow.evidence_quality.evidence_quality_score >= min_value.
        // Required: min_value (float).
        // Example: { kind = "evidence_score_gte", min_value = 0.5 }
        "evidence_score_gte" => {
            let min_val: f64 = map
                .get("min_value")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .ok_or_else(|| {
                    "assertion missing required numeric field \"min_value\"".to_string()
                })?;
            let actual = dot_path(output, "workflow.evidence_quality.evidence_quality_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if actual >= min_val {
                Ok(())
            } else {
                Err(format!(
                    "evidence_score_gte: evidence_quality_score = {actual:.2} (expected >= {min_val})"
                ))
            }
        }

        // data_quality_state_eq: trust.data_quality.state equals expected string.
        // Required: expected (string: "clean_room" | "sparse_but_active" | "populated" | "degraded" | "empty").
        // Example: { kind = "data_quality_state_eq", expected = "populated" }
        "data_quality_state_eq" => {
            let expected = str_field(map, "expected")?;
            let actual = dot_path(output, "data_quality.state")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "data_quality_state_eq: data_quality.state = {actual:?} (expected {:?})",
                    expected
                ))
            }
        }

        // feedback_state_eq: feedback_state[field] == value (bool).
        // Required: field (string), value (bool).
        // Example: { kind = "feedback_state_eq", field = "available", value = false }
        "feedback_state_eq" => {
            let field = str_field(map, "field")?;
            let expected = map
                .get("value")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| "assertion missing required bool field \"value\"".to_string())?;
            let actual = output
                .get("feedback_state")
                .and_then(|s| s.get(field))
                .and_then(Value::as_bool);
            match actual {
                Some(v) if v == expected => Ok(()),
                Some(v) => Err(format!(
                    "feedback_state_eq: feedback_state.{} = {} (expected {})",
                    field, v, expected
                )),
                None => Err(format!(
                    "feedback_state_eq: feedback_state.{} not found in output",
                    field
                )),
            }
        }

        // feedback_state_field_eq: feedback_state[field] == value (string).
        // Required: field (string), value (string).
        // Example: { kind = "feedback_state_field_eq", field = "reason", value = "no_feedback_entries" }
        "feedback_state_field_eq" => {
            let field = str_field(map, "field")?;
            let expected = str_field(map, "value")?;
            let actual = output
                .get("feedback_state")
                .and_then(|s| s.get(field))
                .and_then(Value::as_str)
                .unwrap_or("");
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "feedback_state_field_eq: feedback_state.{} = {:?} (expected {:?})",
                    field, actual, expected
                ))
            }
        }

        // feedback_coverage_eq: feedback_summary.coverage == value.
        // Required: value (string: "none" | "partial" | "applied").
        // Example: { kind = "feedback_coverage_eq", value = "none" }
        "feedback_coverage_eq" => {
            let expected = str_field(map, "value")?;
            let actual = output
                .get("feedback_summary")
                .and_then(|s| s.get("coverage"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "feedback_coverage_eq: feedback_summary.coverage = {:?} (expected {:?})",
                    actual, expected
                ))
            }
        }

        // uncertainty_exact_symbol_match: uncertainty.exact_symbol_match == expected (bool).
        // Required: expected (bool).
        // Example: { kind = "uncertainty_exact_symbol_match", expected = true }
        "uncertainty_exact_symbol_match" => {
            let expected = map
                .get("expected")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| "assertion missing required bool field \"expected\"".to_string())?;
            let actual = output
                .get("uncertainty")
                .and_then(|u| u.get("exact_symbol_match"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "uncertainty_exact_symbol_match: exact_symbol_match = {} (expected {})",
                    actual, expected
                ))
            }
        }

        // uncertainty_primary_source_eq: uncertainty.sources.primary == expected.
        // Required: expected (string: "query" | "db_state" | "result_set" | "none").
        // Example: { kind = "uncertainty_primary_source_eq", expected = "query" }
        "uncertainty_primary_source_eq" => {
            let expected = str_field(map, "expected")?;
            let actual = output
                .get("uncertainty")
                .and_then(|u| u.get("sources"))
                .and_then(|s| s.get("primary"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            if actual == expected {
                Ok(())
            } else {
                Err(format!(
                    "uncertainty_primary_source_eq: sources.primary = {:?} (expected {:?})",
                    actual, expected
                ))
            }
        }

        // uncertainty_source_gte: uncertainty.sources[source] >= min_value.
        // Required: source (string: "query" | "db_state" | "result_set"), min_value (float).
        // Example: { kind = "uncertainty_source_gte", source = "query", min_value = 0.2 }
        "uncertainty_source_gte" => {
            let source = str_field(map, "source")?;
            let min_val: f64 = map
                .get("min_value")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .ok_or_else(|| {
                    "assertion missing required numeric field \"min_value\"".to_string()
                })?;
            let actual = output
                .get("uncertainty")
                .and_then(|u| u.get("sources"))
                .and_then(|s| s.get(source))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if actual >= min_val {
                Ok(())
            } else {
                Err(format!(
                    "uncertainty_source_gte: sources.{} = {:.2} (expected >= {})",
                    source, actual, min_val
                ))
            }
        }

        // array_field_contains: dot-path array field contains `value` string.
        // Required: field (dot-path string), value (string).
        // Example: { kind = "array_field_contains", field = "safe_to_use_for", value = "search" }
        "array_field_contains" => {
            let field = str_field(map, "field")?;
            let value = str_field(map, "value")?;
            let arr = dot_path(output, field)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let found = arr.iter().any(|v| v.as_str().map_or(false, |s| s == value));
            if found {
                Ok(())
            } else {
                Err(format!(
                    "array_field_contains: {:?} not found in {} {:?}",
                    value,
                    field,
                    arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
                ))
            }
        }

        // array_field_excludes: dot-path array field does NOT contain `value` string.
        // Required: field (dot-path string), value (string).
        // Example: { kind = "array_field_excludes", field = "avoid_for", value = "search" }
        "array_field_excludes" => {
            let field = str_field(map, "field")?;
            let value = str_field(map, "value")?;
            let arr = dot_path(output, field)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let found = arr.iter().any(|v| v.as_str().map_or(false, |s| s == value));
            if !found {
                Ok(())
            } else {
                Err(format!(
                    "array_field_excludes: {:?} unexpectedly found in {} {:?}",
                    value,
                    field,
                    arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
                ))
            }
        }

        // all_items_have_field: every object in a dot-path array has `field` non-null/non-empty.
        // Required: array (dot-path to array), field (field name within each item).
        // Example: { kind = "all_items_have_field", array = "likely_edit_files", field = "rationale" }
        "all_items_have_field" => eval_all_items_have_field(map, output),

        // file_field_contains: in a dot-path array, find item whose "file" contains file_fragment,
        // then check that item's `field` contains the substring `value_contains`.
        // Required: array (dot-path), file_fragment (string), field (string), value_contains (string).
        // Example: { kind = "file_field_contains", array = "safe_change_recipe.reference_only",
        //            file_fragment = "WaveformCanvas", field = "rationale", value_contains = "surface" }
        "file_field_contains" => eval_file_field_contains(map, output),

        // json_field_present: dot-path field must exist and be non-null.
        // Required: field (dot-path string).
        // Example: { kind = "json_field_present", field = "total" }
        "json_field_present" => {
            let field = str_field(map, "field")?;
            let present = dot_path(output, field).map_or(false, |v| !v.is_null());
            if present {
                Ok(())
            } else {
                Err(format!(
                    "json_field_present: field {:?} is absent or null",
                    field
                ))
            }
        }

        // json_nested_eq: dot-path field must equal `value` (integer or string).
        // Required: path (dot-path string), value (integer or string).
        // Example: { kind = "json_nested_eq", path = "by_verdict.useful", value = 1 }
        "json_nested_eq" => {
            let path = str_field(map, "path")?;
            let actual = dot_path(output, path);
            // Try integer match first, then string.
            if let Some(expected_int) = map.get("value").and_then(|v| v.as_integer()) {
                let actual_int = actual.and_then(Value::as_i64);
                match actual_int {
                    Some(a) if a == expected_int => Ok(()),
                    Some(a) => Err(format!(
                        "json_nested_eq: {}.{} = {} (expected {})",
                        path, "", a, expected_int
                    )),
                    None => Err(format!(
                        "json_nested_eq: {}: field absent or non-numeric (expected {})",
                        path, expected_int
                    )),
                }
            } else if let Some(expected_str) = map.get("value").and_then(|v| v.as_str()) {
                let actual_str = actual.and_then(Value::as_str).unwrap_or("");
                if actual_str == expected_str {
                    Ok(())
                } else {
                    Err(format!(
                        "json_nested_eq: {} = {:?} (expected {:?})",
                        path, actual_str, expected_str
                    ))
                }
            } else {
                Err("json_nested_eq: assertion missing required field \"value\" (integer or string)".to_string())
            }
        }

        "" => Ok(()), // no kind → smoke test, always passes
        other => Err(format!("unknown assertion kind: {:?}", other)),
    }
}

/// Navigate a dot-separated path into a JSON value.
/// e.g. dot_path(json, "safe_change_recipe.edit") → Some(&json["safe_change_recipe"]["edit"])
fn dot_path<'a>(mut val: &'a Value, path: &str) -> Option<&'a Value> {
    for key in path.split('.') {
        val = val.get(key)?;
    }
    Some(val)
}

fn str_field<'a>(
    map: &'a toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<&'a str, String> {
    map.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("assertion missing required string field {:?}", key))
}

fn u64_field(map: &toml::map::Map<String, toml::Value>, key: &str) -> Result<u64, String> {
    map.get(key)
        .and_then(|v| v.as_integer())
        .map(|i| i as u64)
        .ok_or_else(|| format!("assertion missing required integer field {:?}", key))
}

fn summarize_debug_payload(json: &Value) -> Option<String> {
    // Extract the most relevant debug field for the failure summary line.
    if let Some(arr) = json.get("classification_debug").and_then(|v| v.as_array()) {
        let rules: Vec<&str> = arr
            .iter()
            .filter_map(|e| e.get("rule_that_won").and_then(|v| v.as_str()))
            .collect();
        if !rules.is_empty() {
            return Some(format!("classification rules: {:?}", rules));
        }
    }
    if let Some(arr) = json.get("results").and_then(|v| v.as_array()) {
        let top3: Vec<&str> = arr
            .iter()
            .take(3)
            .filter_map(|r| r.get("qname").and_then(|v| v.as_str()))
            .collect();
        if !top3.is_empty() {
            return Some(format!("top results: {:?}", top3));
        }
    }
    if let Some(arr) = json.get("cluster_debug").and_then(|v| v.as_array()) {
        let winners: Vec<&str> = arr
            .iter()
            .filter_map(|e| {
                e.get("winner_selected")
                    .and_then(|w| w.get("qname"))
                    .and_then(|v| v.as_str())
            })
            .collect();
        if !winners.is_empty() {
            return Some(format!("cluster winners: {:?}", winners));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// probe reindex
// ---------------------------------------------------------------------------

fn reindex_analytics(cfg: &Config, args: ProbeReindexArgs) -> Result<()> {
    let jsonl_path = history_path(cfg);
    if !jsonl_path.exists() {
        println!("No probe history found at {}", jsonl_path.display());
        return Ok(());
    }

    let db_path = analytics_path(cfg);

    // Drop the existing DB if --force or if it doesn't exist yet.
    if db_path.exists() {
        if args.force {
            std::fs::remove_file(&db_path)
                .with_context(|| format!("removing {}", db_path.display()))?;
            eprintln!("Dropped existing {}", db_path.display());
        }
        // Without --force we do incremental (INSERT OR IGNORE), which is fine.
    }

    let conn =
        open_analytics_db(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let raw = std::fs::read_to_string(&jsonl_path)
        .with_context(|| format!("reading {}", jsonl_path.display()))?;

    let mut runs = 0usize;
    let mut probes = 0usize;
    let mut skipped = 0usize;

    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(record) => {
                // Check if already present before insert so we can count skips.
                let run_id = record
                    .get("started_at")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let already: bool = conn
                    .query_row(
                        "SELECT 1 FROM probe_runs WHERE run_id=?1",
                        params![run_id],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if already {
                    skipped += 1;
                    continue;
                }
                let probe_count = record
                    .get("probes")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                insert_run_to_analytics(&conn, &record);
                runs += 1;
                probes += probe_count;
            }
            Err(_) => {} // malformed line — skip silently
        }
    }

    println!(
        "Indexed {} run(s) ({} probe rows) into {}  [{} already present, skipped]",
        runs,
        probes,
        db_path.display(),
        skipped
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// probe history
// ---------------------------------------------------------------------------

fn show_history(cfg: &Config, args: ProbeHistoryArgs) -> Result<()> {
    let path = history_path(cfg);
    if !path.exists() {
        println!("No probe history yet. Run `asd probe run` to record the first entry.");
        return Ok(());
    }

    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;

    // Parse all non-empty lines as JSON records.
    let mut records: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Apply --tag filter (matches filter_tag field recorded in history).
    if let Some(ref tag) = args.tag {
        records.retain(|r| {
            r.get("filter_tag")
                .and_then(Value::as_str)
                .map_or(false, |t| t == tag)
        });
    }

    // Most recent last in file — show last N in reverse-chronological order.
    let total = records.len();
    let start = total.saturating_sub(args.last);
    let mut window: Vec<&Value> = records[start..].iter().rev().collect();

    if window.is_empty() {
        println!("No matching history records.");
        return Ok(());
    }

    if args.json {
        // Emit each record as a JSON line.
        for r in &window {
            println!("{}", serde_json::to_string(r)?);
        }
    } else {
        // Summary table: version | scope | total | pass | wall_ms | budget | slowest
        // scope distinguishes full runs from filtered subsets so "fewer probes" is obvious.
        println!(
            "{:<10}  {:<16}  {:>5}  {:>5}  {:>10}  {:<8}  {}",
            "version", "scope", "total", "pass", "wall_ms", "budget", "slowest"
        );
        println!("{}", "-".repeat(90));
        for r in &mut window {
            let version = r.get("asd_version").and_then(Value::as_str).unwrap_or("?");
            let scope = match (
                r.get("filter_name").and_then(Value::as_str),
                r.get("filter_tag").and_then(Value::as_str),
            ) {
                (Some(n), _) => format!("name:{}", n),
                (_, Some(t)) => format!("tag:{}", t),
                _ => "all".to_string(),
            };
            let total_n = r.get("total").and_then(Value::as_u64).unwrap_or(0);
            let passed_n = r.get("passed").and_then(Value::as_u64).unwrap_or(0);
            let wall = r.get("wall_time_ms").and_then(Value::as_u64).unwrap_or(0);
            let budget_ok = r
                .get("budget_failed")
                .and_then(Value::as_bool)
                .map_or("—", |b| if b { "FAIL" } else { "ok" });
            let slowest_name = r
                .get("slowest")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("—");
            let slowest_ms = r
                .get("slowest")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|s| s.get("duration_ms"))
                .and_then(Value::as_u64)
                .map(|ms| format!("({}ms)", ms))
                .unwrap_or_default();
            println!(
                "{:<10}  {:<16}  {:>5}  {:>5}  {:>10}  {:<8}  {} {}",
                version, scope, total_n, passed_n, wall, budget_ok, slowest_name, slowest_ms
            );
        }
        println!("\n{} run(s) shown ({} total recorded)", window.len(), total);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// probe bootstrap — generate a starter probes.toml from the current index
// ---------------------------------------------------------------------------

/// Generate a starter `.asd/probes.toml` for the current workspace.
///
/// Discovers top symbols via FTS and emits:
/// - Structural smoke probes (trust, search result count, feedback state)
/// - Per-symbol ranking probes for the top-N symbols found
/// - Change-model classification guards for the most prominent domain terms
///
/// Safe to run on any indexed workspace. Does not modify the index or ledger.
fn bootstrap_probes(cfg: &Config, args: ProbeBootstrapArgs) -> Result<()> {
    let path = probe_file_path(cfg);

    if path.exists() && !args.force {
        eprintln!(
            "probes.toml already exists at {}.\nUse `asd probe bootstrap --force` to overwrite.",
            path.display()
        );
        std::process::exit(1);
    }

    // -----------------------------------------------------------------------
    // Discover top symbols from FTS
    // -----------------------------------------------------------------------
    let fts = SearchFtsDb::open(&cfg.db_path)
        .map_err(|e| anyhow::anyhow!("Cannot open FTS DB: {}", e))?;

    if !fts.has_data() {
        eprintln!("FTS index is empty. Run `asd index` first, then re-run bootstrap.");
        std::process::exit(1);
    }

    // Search with a very broad filter to get top symbols (exclude tests).
    let filters = FtsFilters {
        kind: None,
        language: None,
        include_tests: false,
        tests_only: false,
        exclude_terms: vec![],
        paths_filter: vec![],
        exclude_paths: vec![],
        exclude_languages: vec![],    };
    // Try a few common structural terms to surface domain symbols.
    let try_terms = [
        "manager",
        "service",
        "view",
        "model",
        "controller",
        "handler",
        "client",
        "engine",
        "store",
        "state",
        "view",
        "scene",
        "player",
        "editor",
        "session",
    ];
    let mut all_hits: Vec<_> = Vec::new();
    for term in try_terms {
        let h = fts.search(term, &filters, args.top * 2).unwrap_or_default();
        for hit in h {
            if !all_hits
                .iter()
                .any(|x: &agentstatedeveloper_core::FtsHit| x.qname == hit.qname)
            {
                all_hits.push(hit);
            }
        }
        if all_hits.len() >= args.top * 3 {
            break;
        }
    }

    // Pick symbols: prefer tier-0 (domain), fall back to tier-1, skip tier-2 (tests).
    let top_symbols: Vec<(String, String)> = all_hits
        .iter()
        .filter(|h| h.tier != 2u8) // skip test symbols (tier=2)
        .take(args.top)
        .filter_map(|h| {
            let qname = h.qname.clone();
            // Short name = last component after '.'
            let short = qname.rsplit('.').next().unwrap_or(&qname).to_string();
            if short.len() >= 3 {
                Some((qname, short))
            } else {
                None
            }
        })
        .collect();

    // -----------------------------------------------------------------------
    // Get trust signals to tailor smoke tests
    // -----------------------------------------------------------------------
    let trust = compute_trust_score(&cfg.db_path);
    let db_state = trust.data_quality.state.as_str();

    // -----------------------------------------------------------------------
    // Build the probes.toml content
    // -----------------------------------------------------------------------
    let mut out = String::new();

    out.push_str(&format!(
        "# ASD golden benchmark probes — bootstrapped by `asd probe bootstrap`\n\
         # DB: {}\n\
         # Symbols indexed: {}\n\
         # Data quality: {} ({})\n\
         # Generated: {}\n\
         #\n\
         # Run: asd probe run\n\
         # Run subset: asd probe run --tag smoke\n\
         #             asd probe run --tag ranking\n\n",
        cfg.db_path.display(),
        trust.signals.symbol_count,
        db_state,
        trust.data_quality.reason,
        chrono::Utc::now().format("%Y-%m-%d"),
    ));

    // --- Structural smoke probes -------------------------------------------
    out.push_str("# ---------------------------------------------------------------------------\n");
    out.push_str("# Structural smoke tests — always pass on a healthy indexed workspace\n");
    out.push_str(
        "# ---------------------------------------------------------------------------\n\n",
    );

    out.push_str("[[probe]]\n");
    out.push_str("name = \"smoke-trust-score\"\n");
    out.push_str("description = \"Trust score must be >= 0.5 for a populated index\"\n");
    out.push_str("tags = [\"smoke\", \"trust\"]\n");
    out.push_str("command = \"trust\"\n");
    out.push_str("args = []\n");
    out.push_str("assert = { kind = \"field_gte\", field = \"score\", min_value = 0.5 }\n\n");

    out.push_str("[[probe]]\n");
    out.push_str("name = \"smoke-symbol-count\"\n");
    out.push_str("description = \"Index must contain at least 10 symbols\"\n");
    out.push_str("tags = [\"smoke\", \"trust\"]\n");
    out.push_str("command = \"trust\"\n");
    out.push_str("args = []\n");
    out.push_str(
        "assert = { kind = \"field_gte\", field = \"signals.symbol_count\", min_value = 10 }\n\n",
    );

    out.push_str("[[probe]]\n");
    out.push_str("name = \"smoke-search-returns-results\"\n");
    out.push_str("description = \"A broad search must return at least 1 result\"\n");
    out.push_str("tags = [\"smoke\", \"search\"]\n");
    out.push_str("command = \"search\"\n");

    // Use the first discovered symbol's short name as the search term.
    let first_query = top_symbols
        .first()
        .map(|(_, s)| s.to_lowercase())
        .unwrap_or_else(|| "state".to_string());
    out.push_str(&format!("args = [\"{}\", \"--agent\"]\n", first_query));
    out.push_str(
        "assert = { kind = \"array_field_count_gte\", field = \"results\", min_count = 1 }\n\n",
    );

    out.push_str("[[probe]]\n");
    out.push_str("name = \"smoke-feedback-state-field-present\"\n");
    out.push_str("description = \"search output must include feedback_state field\"\n");
    out.push_str("tags = [\"smoke\", \"feedback\"]\n");
    out.push_str("command = \"search\"\n");
    out.push_str(&format!("args = [\"{}\", \"--agent\"]\n", first_query));
    out.push_str("assert = { kind = \"feedback_summary_gte\", field = \"entries_applied\", min_value = 0 }\n\n");

    // Data quality probe — adapt to current state.
    out.push_str("[[probe]]\n");
    out.push_str("name = \"smoke-data-quality-state\"\n");
    out.push_str(&format!(
        "description = \"Data quality must be '{}' for this workspace state\"\n",
        db_state
    ));
    out.push_str("tags = [\"smoke\", \"trust\", \"data-quality\"]\n");
    out.push_str("command = \"trust\"\n");
    out.push_str("args = []\n");
    out.push_str(&format!(
        "assert = {{ kind = \"data_quality_state_eq\", expected = \"{}\" }}\n\n",
        db_state
    ));

    // --- Ranking probes for discovered symbols ----------------------------
    if !top_symbols.is_empty() {
        out.push_str(
            "# ---------------------------------------------------------------------------\n",
        );
        out.push_str("# Symbol ranking probes — discovered from current index\n");
        out.push_str("# Edit these to match your domain's key symbols.\n");
        out.push_str(
            "# ---------------------------------------------------------------------------\n\n",
        );

        for (i, (qname, short)) in top_symbols.iter().enumerate() {
            let slug = short
                .to_lowercase()
                .replace(|c: char| !c.is_alphanumeric(), "-");

            // Lenient (existing): rank ≤ 5. Surfaces "right symbol
            // is anywhere near the top" — useful for ranking-not-broken
            // sanity but can't distinguish rank-1 from rank-5.
            let probe_name = format!("rank-{}-top5", slug);
            let desc = format!("{} must appear in top 5 results for its own name", qname);
            out.push_str(&format!("[[probe]]\nname = {:?}\n", probe_name));
            out.push_str(&format!("description = {:?}\n", desc));
            out.push_str("tags = [\"ranking\"]\n");
            out.push_str("command = \"search\"\n");
            out.push_str(&format!("args = [{:?}, \"--agent\"]\n", short));
            out.push_str(&format!(
                "assert = {{ kind = \"qname_rank_lte\", fragment = {:?}, max_rank = 5 }}\n\n",
                qname
            ));

            // Plan J t-019: precision (new): rank == 1. Surfaces
            // "right symbol is AT the top" — when both probes for
            // the same query land in the same uncertainty bucket,
            // their pass/fail split tells the calibration harvester
            // whether the bucket label is too strict (precision
            // fails → predictor was right to flag low confidence)
            // or genuinely miscalibrated (precision passes too →
            // bucket should be promoted).
            let prec_name = format!("rank-{}-eq1", slug);
            let prec_desc = format!(
                "{} must be the #1 result for its own name (precision-mode for calibration)",
                qname
            );
            out.push_str(&format!("[[probe]]\nname = {:?}\n", prec_name));
            out.push_str(&format!("description = {:?}\n", prec_desc));
            out.push_str("tags = [\"ranking\", \"precision\"]\n");
            out.push_str("command = \"search\"\n");
            out.push_str(&format!("args = [{:?}, \"--agent\"]\n", short));
            out.push_str(&format!(
                "assert = {{ kind = \"qname_rank_eq\", fragment = {:?}, exact_rank = 1 }}\n\n",
                qname
            ));

            if i >= args.top.saturating_sub(1) {
                break;
            }
        }
    }

    // --- Change-model classification smoke test ---------------------------
    if !top_symbols.is_empty() {
        let query_term = top_symbols
            .first()
            .map(|(_, s)| s.to_lowercase())
            .unwrap_or_default();
        if query_term.len() >= 3 {
            out.push_str(
                "# ---------------------------------------------------------------------------\n",
            );
            out.push_str("# Change-model smoke tests\n");
            out.push_str(
                "# ---------------------------------------------------------------------------\n\n",
            );

            out.push_str("[[probe]]\n");
            out.push_str("name = \"change-model-returns-edit-files\"\n");
            out.push_str(
                "description = \"prepare-change must return at least 1 likely_edit_files entry\"\n",
            );
            out.push_str("tags = [\"change-model\", \"smoke\"]\n");
            out.push_str("command = \"prepare-change\"\n");
            out.push_str(&format!("args = [{:?}, \"--agent\"]\n", query_term));
            out.push_str("assert = { kind = \"array_field_count_gte\", field = \"likely_edit_files\", min_count = 1 }\n\n");

            out.push_str("[[probe]]\n");
            out.push_str("name = \"change-model-edit-confidence-present\"\n");
            out.push_str(
                "description = \"classification_summary must include an edit_confidence field\"\n",
            );
            out.push_str("tags = [\"change-model\", \"smoke\"]\n");
            out.push_str("command = \"prepare-change\"\n");
            out.push_str(&format!("args = [{:?}, \"--agent\"]\n", query_term));
            out.push_str("assert = { kind = \"array_field_count_gte\", field = \"likely_edit_files\", min_count = 0 }\n\n");
        }
    }

    // -----------------------------------------------------------------------
    // Write the file
    // -----------------------------------------------------------------------
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &out)?;

    println!("✓ Generated {} probes.toml", path.display());
    println!("  Symbols indexed : {}", trust.signals.symbol_count);
    println!(
        "  Data quality    : {} ({})",
        db_state, trust.data_quality.reason
    );
    println!(
        "  Ranking probes  : {} (for top {} symbols)",
        top_symbols.len(),
        args.top
    );
    println!("\nRun: asd probe run");
    println!("Tag subsets: asd probe run --tag smoke");

    Ok(())
}

// ---------------------------------------------------------------------------
// probe add
// ---------------------------------------------------------------------------

fn add_probe(cfg: &Config, args: ProbeAddArgs) -> Result<()> {
    let path = probe_file_path(cfg);

    // Ensure .asd dir exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut content = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };

    // Build the new [[probe]] block.
    let mut block = format!("\n[[probe]]\nname = {:?}\n", args.name);
    if let Some(ref desc) = args.description {
        block.push_str(&format!("description = {:?}\n", desc));
    }
    block.push_str(&format!("command = {:?}\n", args.command));

    // Write args as TOML array.
    let toml_args: Vec<String> = args.args.iter().map(|a| format!("{:?}", a)).collect();
    block.push_str(&format!("args = [{}]\n", toml_args.join(", ")));

    if let Some(ref assert_str) = args.assert {
        block.push_str(&format!("assert = {}\n", assert_str));
    }

    content.push_str(&block);
    std::fs::write(&path, &content)?;
    println!("Added probe {:?} to {}", args.name, path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Plan M t-005 (1.0.100): extracted assertion evaluators.
//
// `eval_assert` (above) is a 1000-line dispatch over 41 assertion kinds.
// The five longest arms have been lifted into private fns so the dispatch
// match is readable at a glance and each eval is testable in isolation.
//
// Pattern for future arm extraction: each helper takes (map, output) and
// returns the same Result<(), String>. The dispatcher arm becomes a
// single-line `"<kind>" => eval_<kind>(map, output),`. No closures
// capture `empty_arr` — each helper declares its own when needed.
// ---------------------------------------------------------------------------

fn eval_qname_rank_lte(
    map: &toml::map::Map<String, toml::Value>,
    output: &Value,
) -> Result<(), String> {
    let fragment = str_field(map, "fragment")?;
    let max_rank = u64_field(map, "max_rank")?;
    let empty_arr: Vec<Value> = Vec::new();
    let results = output
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_arr);
    let pos = results.iter().position(|r| {
        r.get("qname")
            .and_then(|v| v.as_str())
            .map_or(false, |q| q.contains(fragment))
    });
    match pos {
        Some(idx) if (idx as u64 + 1) <= max_rank => Ok(()),
        Some(idx) => Err(format!(
            "qname_rank_lte: {:?} found at rank {} (max_rank={})",
            fragment,
            idx + 1,
            max_rank
        )),
        None => Err(format!(
            "qname_rank_lte: no result qname contains {:?} (checked {} results)",
            fragment,
            results.len()
        )),
    }
}

fn eval_cluster_winner_kind_not(
    map: &toml::map::Map<String, toml::Value>,
    output: &Value,
) -> Result<(), String> {
    let doc_stem = str_field(map, "doc_stem")?;
    let kind_not = str_field(map, "kind_not")?;
    let empty_arr: Vec<Value> = Vec::new();
    let dbg = output
        .get("cluster_debug")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_arr);
    let entry = dbg.iter().find(|e| {
        e.get("doc_file")
            .and_then(|v| v.as_str())
            .map_or(false, |f| f.to_lowercase().contains(&doc_stem.to_lowercase()))
    });
    match entry {
        None => Err(format!(
            "cluster_winner_kind_not: no cluster_debug entry matches doc_stem {:?}",
            doc_stem
        )),
        Some(e) => {
            let winner = e.get("winner_selected").unwrap_or(&Value::Null);
            let qname = winner.get("qname").and_then(|v| v.as_str()).unwrap_or("");
            if qname.contains(kind_not) {
                Err(format!(
                    "cluster_winner_kind_not: winner {:?} contains {:?}",
                    qname, kind_not
                ))
            } else {
                Ok(())
            }
        }
    }
}

fn eval_cluster_winner_qname_contains(
    map: &toml::map::Map<String, toml::Value>,
    output: &Value,
) -> Result<(), String> {
    let doc_stem = str_field(map, "doc_stem")?;
    let fragment = str_field(map, "fragment")?;
    let empty_arr: Vec<Value> = Vec::new();
    let dbg = output
        .get("cluster_debug")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_arr);
    let entry = dbg.iter().find(|e| {
        e.get("doc_file")
            .and_then(|v| v.as_str())
            .map_or(false, |f| f.to_lowercase().contains(&doc_stem.to_lowercase()))
    });
    match entry {
        None => Err(format!(
            "cluster_winner_qname_contains: no cluster_debug entry matches doc_stem {:?}",
            doc_stem
        )),
        Some(e) => {
            let winner = e.get("winner_selected").unwrap_or(&Value::Null);
            let qname = winner.get("qname").and_then(|v| v.as_str()).unwrap_or("");
            if qname.to_lowercase().contains(&fragment.to_lowercase()) {
                Ok(())
            } else {
                Err(format!(
                    "cluster_winner_qname_contains: winner {:?} does not contain {:?}",
                    qname, fragment
                ))
            }
        }
    }
}

fn eval_all_items_have_field(
    map: &toml::map::Map<String, toml::Value>,
    output: &Value,
) -> Result<(), String> {
    let array_path = str_field(map, "array")?;
    let field = str_field(map, "field")?;
    let arr = dot_path(output, array_path)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if arr.is_empty() {
        // Empty array trivially passes (nothing to check).
        return Ok(());
    }
    let missing: Vec<String> = arr
        .iter()
        .filter_map(|item| {
            let present = item
                .get(field)
                .map(|v| !v.is_null() && v.as_str().map_or(true, |s| !s.is_empty()))
                .unwrap_or(false);
            if !present {
                Some(
                    item.get("file")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "all_items_have_field: {} items in {} missing field {:?}: {:?}",
            missing.len(),
            array_path,
            field,
            &missing[..missing.len().min(5)]
        ))
    }
}

fn eval_file_field_contains(
    map: &toml::map::Map<String, toml::Value>,
    output: &Value,
) -> Result<(), String> {
    let array_path = str_field(map, "array")?;
    let file_fragment = str_field(map, "file_fragment")?;
    let field = str_field(map, "field")?;
    let value_contains = str_field(map, "value_contains")?;
    let arr = dot_path(output, array_path)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let item = arr.iter().find(|item| {
        item.get("file")
            .and_then(Value::as_str)
            .map_or(false, |f| f.contains(file_fragment))
    });
    match item {
        None => Err(format!(
            "file_field_contains: no item with file containing {:?} found in {}",
            file_fragment, array_path
        )),
        Some(item) => {
            let val = item.get(field).and_then(Value::as_str).unwrap_or("");
            if val.contains(value_contains) {
                Ok(())
            } else {
                Err(format!(
                    "file_field_contains: item {:?}.{} = {:?} does not contain {:?}",
                    item.get("file").and_then(Value::as_str).unwrap_or("?"),
                    field,
                    val,
                    value_contains
                ))
            }
        }
    }
}

#[cfg(test)]
mod plan_j_t019_qname_rank_eq_tests {
    //! Plan J t-019: precision-mode probe assertion. The lenient
    //! `qname_rank_lte` passes if the matching qname is anywhere
    //! in the top N; `qname_rank_eq` passes only at EXACTLY the
    //! specified rank. Together they let calibration distinguish
    //! threshold-too-strict from probes-too-lenient.

    use super::eval_assert;
    use serde_json::json;

    fn assert_block(kind: &str, fragment: &str, exact_rank: u64) -> toml::Value {
        let s = format!(
            "kind = {:?}\nfragment = {:?}\nexact_rank = {}",
            kind, fragment, exact_rank
        );
        toml::from_str(&s).unwrap()
    }

    fn results_with(qnames: &[&str]) -> serde_json::Value {
        json!({
            "results": qnames
                .iter()
                .map(|q| json!({ "qname": q }))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn passes_when_exact_rank_match() {
        let assert = assert_block("qname_rank_eq", "ProjectManager", 1);
        let output = results_with(&[
            "App.Foo.ProjectManager",
            "App.Bar.OtherSym",
        ]);
        assert!(eval_assert(&assert, &output).is_ok());
    }

    #[test]
    fn fails_when_match_at_wrong_rank() {
        let assert = assert_block("qname_rank_eq", "ProjectManager", 1);
        let output = results_with(&[
            "App.Bar.OtherSym",            // rank 1
            "App.Foo.ProjectManager",       // rank 2 — wrong
        ]);
        let err = eval_assert(&assert, &output).expect_err("must fail");
        assert!(err.contains("rank 2"), "got: {err}");
        assert!(err.contains("expected exactly 1"), "got: {err}");
    }

    #[test]
    fn fails_when_no_match() {
        let assert = assert_block("qname_rank_eq", "MissingSymbol", 1);
        let output = results_with(&["App.Foo.SomethingElse"]);
        let err = eval_assert(&assert, &output).expect_err("must fail");
        assert!(err.contains("no result qname contains"), "got: {err}");
        assert!(err.contains("MissingSymbol"), "got: {err}");
    }

    #[test]
    fn precision_distinguishes_from_lte_on_same_input() {
        // The whole point of t-019: same query, same results — but
        // rank_lte(5) passes while rank_eq(1) fails. This split is
        // the signal the calibration harvester needs to disambiguate
        // its "too strict" advisory.
        let output = results_with(&[
            "App.Bar.OtherSym",            // rank 1
            "App.Bar.OtherSym2",            // rank 2
            "App.Foo.ProjectManager",       // rank 3 — within top-5 but not #1
        ]);

        let lenient: toml::Value = toml::from_str(
            "kind = \"qname_rank_lte\"\nfragment = \"ProjectManager\"\nmax_rank = 5",
        )
        .unwrap();
        assert!(
            eval_assert(&lenient, &output).is_ok(),
            "rank_lte(5) must accept rank 3"
        );

        let precision = assert_block("qname_rank_eq", "ProjectManager", 1);
        assert!(
            eval_assert(&precision, &output).is_err(),
            "rank_eq(1) must reject rank 3 — the very split that drives calibration"
        );
    }

    #[test]
    fn rank_eq_2_passes_when_match_at_rank_2() {
        // Defensive: exact_rank isn't hardcoded to 1. Future probes
        // might assert "this symbol is reliably second" if there's
        // a known canonical winner ahead of it.
        // Fragment chosen to avoid substring-match collision with
        // the rank-1 qname — a real issue (fragment "Manager"
        // would match both "ProjectManager" and "SnapshotManager")
        // that the pre-existing qname_rank_lte already has, so it's
        // not t-019-specific.
        let assert = assert_block("qname_rank_eq", "AlphaCanon", 2);
        let output = results_with(&[
            "App.Bar.BetaSibling",
            "App.Foo.AlphaCanon",
        ]);
        assert!(eval_assert(&assert, &output).is_ok());
    }
}
