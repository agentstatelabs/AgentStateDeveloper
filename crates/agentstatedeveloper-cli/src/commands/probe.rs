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
//!   result_count_lte — results array length ≤ `max`
//!   cluster_winner_kind_not        — cluster_debug entry matching `doc_stem` winner kind ≠ `kind_not`
//!   cluster_winner_qname_contains  — cluster_debug entry matching `doc_stem` winner qname contains `fragment`
//!   no_duplicate_summaries         — no two suggested_entries share the same summary per symbol
//!   qname_not_in_results           — no result's qname contains `fragment` (feedback suppression check)
//!   boosted_outranked_contains     — boosted_outranked has an entry containing `fragment`
//!   ambiguous_terms_nonempty       — ambiguous_terms array is non-empty (broad query uncertainty check)
//!   scoped_suggestions_nonempty    — scoped_suggestions array is non-empty
//!   scoped_suggestions_contains    — scoped_suggestions contains an entry matching `fragment`

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{Connection, params};
use serde_json::Value;

use agentstatedeveloper_core::{SearchFtsDb, stale_warning};

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
    /// Show probe run history from .asd/probe-history.jsonl.
    History(ProbeHistoryArgs),
    /// Rebuild probe-analytics.db from probe-history.jsonl.
    Reindex(ProbeReindexArgs),
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
    #[serde(default)]
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
        ProbeSub::History(args) => show_history(cfg, args),
        ProbeSub::Reindex(args) => reindex_analytics(cfg, args),
    }
}

// ---------------------------------------------------------------------------
// probe-analytics.db — schema + helpers
// ---------------------------------------------------------------------------

fn analytics_path(cfg: &Config) -> PathBuf {
    let db_dir = Path::new(&cfg.db_path)
        .parent()
        .unwrap_or(Path::new("."));
    db_dir.join(".asd").join("probe-analytics.db")
}

fn open_analytics_db(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch("
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
    ")?;
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
        _            => "all".to_string(),
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
        .query_row("SELECT 1 FROM probe_runs WHERE run_id=?1", params![run_id], |_| Ok(true))
        .unwrap_or(false);
    if exists { return; }

    let scope = scope_from_record(record);

    let res = conn.execute(
        "INSERT OR IGNORE INTO probe_runs
         (run_id, asd_version, started_at, finished_at, probe_file, db_state, symbol_count,
          scope, total, passed, failed, budget_failed, wall_time_ms, worker_count,
          performance_budget_ms, filter_name, filter_tag)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        params![
            run_id,
            record.get("asd_version").and_then(Value::as_str).unwrap_or(""),
            record.get("started_at").and_then(Value::as_str).unwrap_or(""),
            record.get("finished_at").and_then(Value::as_str),
            record.get("probe_file").and_then(Value::as_str),
            record.get("db_state").and_then(Value::as_str),
            record.get("symbol_count").and_then(Value::as_i64),
            scope,
            record.get("total").and_then(Value::as_i64).unwrap_or(0),
            record.get("passed").and_then(Value::as_i64).unwrap_or(0),
            record.get("failed").and_then(Value::as_i64).unwrap_or(0),
            record.get("budget_failed").and_then(Value::as_bool).map(|b| b as i64).unwrap_or(0),
            record.get("wall_time_ms").and_then(Value::as_i64).unwrap_or(0),
            record.get("worker_count").and_then(Value::as_i64),
            record.get("performance_budget_ms").and_then(Value::as_i64),
            record.get("filter_name").and_then(Value::as_str),
            record.get("filter_tag").and_then(Value::as_str),
        ],
    );
    if res.is_err() { return; }

    // Insert per-probe rows from the `probes` array (present from this version onward).
    if let Some(probes) = record.get("probes").and_then(Value::as_array) {
        for p in probes {
            let tags_str = p.get("tags")
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
                    p.get("passed").and_then(Value::as_bool).map(|b| b as i64).unwrap_or(1),
                    p.get("slow").and_then(Value::as_bool).map(|b| b as i64).unwrap_or(0),
                    p.get("timed_out").and_then(Value::as_bool).map(|b| b as i64).unwrap_or(0),
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
    let db_dir = Path::new(&cfg.db_path)
        .parent()
        .unwrap_or(Path::new("."));
    db_dir.join(".asd").join("probes.toml")
}

fn history_path(cfg: &Config) -> PathBuf {
    let db_dir = Path::new(&cfg.db_path)
        .parent()
        .unwrap_or(Path::new("."));
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
    let mut lines: Vec<String> = existing.lines()
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

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let pf: ProbeFile = toml::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;

    let probes: Vec<&ProbeEntry> = pf.probe.iter()
        .filter(|p| {
            // --name: exact name match
            if let Some(ref n) = args.name {
                if p.name != *n { return false; }
            }
            // --tag: probe must include this tag
            if let Some(ref t) = args.tag {
                if !p.tags.iter().any(|tag| tag == t) { return false; }
            }
            // --filter: legacy substring match on name
            if let Some(ref f) = args.filter {
                if !p.name.contains(f.as_str()) { return false; }
            }
            true
        })
        .collect();

    if probes.is_empty() {
        if args.name.is_some() || args.tag.is_some() || args.filter.is_some() {
            let mut reason = Vec::new();
            if let Some(ref n) = args.name   { reason.push(format!("name={:?}", n)); }
            if let Some(ref t) = args.tag    { reason.push(format!("tag={:?}", t)); }
            if let Some(ref f) = args.filter { reason.push(format!("filter={:?}", f)); }
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

    // Gather DB metadata once before running probes (cheap reads).
    let db_state = if stale_warning(&cfg.db_path, 3600).is_none() { "fresh" } else { "stale" };
    let symbol_count: Option<u64> = SearchFtsDb::open(&cfg.db_path).ok()
        .map(|fts| fts.symbol_count() as u64);

    let started_at = Utc::now().to_rfc3339();
    let wall_start = Instant::now();

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
            let handles: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(j, probe)| {
                    let global_idx = chunk_start + j;
                    scope.spawn(move || {
                        // Check flag before doing work (fast exit on fail-fast).
                        if ff.load(std::sync::atomic::Ordering::Relaxed) {
                            return None;
                        }
                        let result = execute_probe(cfg, probe);
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
                    let status =
                        if is_fail { "FAIL" } else if is_slow { "SLOW" } else { "PASS" };
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
        results.iter().filter(|r| r.duration_ms > threshold_ms as u128).collect()
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

    // Top-5 slowest — full result shape, pre-sorted descending by duration_ms.
    let mut by_duration: Vec<(usize, u128)> = results.iter().enumerate()
        .map(|(i, r)| (i, r.duration_ms))
        .collect();
    by_duration.sort_by(|a, b| b.1.cmp(&a.1));
    let slowest_top5: Vec<Value> = by_duration.iter().take(5)
        .map(|(i, _)| result_to_json(&results[*i]))
        .collect();

    // Compute summary values shared by JSON output, human output, and history.
    let wall_time_ms = wall_start.elapsed().as_millis();
    let finished_at = Utc::now().to_rfc3339();
    let budget_failed = !slow_violations.is_empty();
    let slow_violation_names: Vec<&str> = slow_violations.iter()
        .map(|r| r.name.as_str())
        .collect();

    if args.json {
        let json_results: Vec<Value> = results.iter().map(|r| result_to_json(r)).collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
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
            "results": json_results,
        }))?);
    } else {
        println!("\n{} probe(s): {} passed, {} failed  [{} workers, {}ms wall]",
            results.len(), passed, failed, jobs, wall_time_ms);
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
                println!("  {} ({}ms)",
                    entry["name"].as_str().unwrap_or("?"),
                    entry["duration_ms"].as_u64().unwrap_or(0));
            }
        }
    }

    // Always append a compact record to probe-history.jsonl regardless of output mode.
    // Includes per-probe compact rows (no debug_payload) — canonical source of truth
    // for per-probe trend analysis.  The analytics SQLite DB mirrors this.
    let probes_compact: Vec<Value> = results.iter().map(|r| {
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
    }).collect();
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
    assertion: String,      // assertion kind, e.g. "file_not_in_key"; "" = smoke test
    tags: Vec<String>,
    duration_ms: u128,
    timed_out: bool,        // true if the probe was killed by a timeout (reserved; always false today)
    error: Option<String>,
    debug_payload: Option<Value>,
    debug_payload_summary: Option<String>,
}

fn execute_probe(cfg: &Config, probe: &ProbeEntry) -> ProbeResult {
    let start = Instant::now();
    // Extract assertion kind from probe definition for result metadata.
    let assertion_kind = probe.assert.as_table()
        .and_then(|m| m.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Resolve asd binary path.
    // Prefer the current executable if it actually exists (covers installed + dev builds).
    // Fall back to plain "asd" so the OS will find it via PATH — this handles cases
    // where current_exe() returns a stale worktree path that no longer exists.
    let asd_bin_path: PathBuf = {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("asd"));
        if exe.exists() { exe } else { PathBuf::from("asd") }
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
    let work_dir = probe.cwd.as_ref()
        .map(PathBuf::from)
        .unwrap_or(probe_dir);

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
                error: Some(format!("failed to execute asd ({:?}) via sh: {}", asd_bin, e)),
                debug_payload: None,
                debug_payload_summary: None,
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
            error: Some(format!("command exited with {}: {}", output.status, stderr.trim())),
            debug_payload: None,
            debug_payload_summary: None,
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
        },
        Err(msg) => {
            let summary = summarize_debug_payload(&json);
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
            }
        }
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

    let kind = map.get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Resolve a possibly dotted key path (e.g. "safe_change_recipe.reference_only") into
    // the nested Value.
    fn resolve_key<'a>(output: &'a Value, key: &str) -> Option<&'a Value> {
        let mut cur = output;
        for part in key.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur)
    }

    let empty_arr: Vec<Value> = Vec::new();

    match kind {
        // file_not_in_key: no item in output[key] array has item[field] containing value.
        "file_not_in_key" => {
            let key = str_field(map, "key")?;
            let field = str_field(map, "field")?;
            let value = str_field(map, "value")?;
            let arr = resolve_key(output, key).and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let found: Vec<&str> = arr.iter()
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
            let arr = resolve_key(output, key).and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let found = arr.iter()
                .any(|item| item.get(field)
                    .and_then(|v| v.as_str())
                    .map_or(false, |s| s.contains(value)));
            if found {
                Ok(())
            } else {
                Err(format!("file_in_key: no item in {}[].{} contains {:?}", key, field, value))
            }
        }

        // qname_rank_lte: result whose qname contains `fragment` is at rank ≤ max_rank (1-based).
        "qname_rank_lte" => {
            let fragment = str_field(map, "fragment")?;
            let max_rank = u64_field(map, "max_rank")?;
            let results = output.get("results").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let pos = results.iter().position(|r| {
                r.get("qname").and_then(|v| v.as_str())
                    .map_or(false, |q| q.contains(fragment))
            });
            match pos {
                Some(idx) if (idx as u64 + 1) <= max_rank => Ok(()),
                Some(idx) => Err(format!(
                    "qname_rank_lte: {:?} found at rank {} (max_rank={})",
                    fragment, idx + 1, max_rank
                )),
                None => Err(format!(
                    "qname_rank_lte: no result qname contains {:?} (checked {} results)",
                    fragment, results.len()
                )),
            }
        }

        // result_count_lte: len(results) ≤ max.
        "result_count_lte" => {
            let max = u64_field(map, "max")?;
            let results = output.get("results").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let n = results.len() as u64;
            if n <= max {
                Ok(())
            } else {
                Err(format!("result_count_lte: got {} results (max={})", n, max))
            }
        }

        // cluster_winner_kind_not: cluster_debug entry whose doc_file contains `doc_stem`
        // must not have winner qname containing `kind_not` (e.g. "Tests").
        "cluster_winner_kind_not" => {
            let doc_stem = str_field(map, "doc_stem")?;
            let kind_not = str_field(map, "kind_not")?;
            let dbg = output.get("cluster_debug").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let entry = dbg.iter().find(|e| {
                e.get("doc_file").and_then(|v| v.as_str())
                    .map_or(false, |f| f.to_lowercase().contains(&doc_stem.to_lowercase()))
            });
            match entry {
                None => Err(format!("cluster_winner_kind_not: no cluster_debug entry matches doc_stem {:?}", doc_stem)),
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

        // cluster_winner_qname_contains: cluster_debug entry whose doc_file contains `doc_stem`
        // must have winner qname containing `fragment`.
        "cluster_winner_qname_contains" => {
            let doc_stem = str_field(map, "doc_stem")?;
            let fragment = str_field(map, "fragment")?;
            let dbg = output.get("cluster_debug").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let entry = dbg.iter().find(|e| {
                e.get("doc_file").and_then(|v| v.as_str())
                    .map_or(false, |f| f.to_lowercase().contains(&doc_stem.to_lowercase()))
            });
            match entry {
                None => Err(format!("cluster_winner_qname_contains: no cluster_debug entry matches doc_stem {:?}", doc_stem)),
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

        // no_duplicate_summaries: no two suggested_entries share the same summary text.
        "no_duplicate_summaries" => {
            let entries = output.get("suggested_entries").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let mut seen_global: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for e in entries {
                if let Some(s) = e.get("summary").and_then(|v| v.as_str()) {
                    *seen_global.entry(s.to_string()).or_insert(0) += 1;
                }
            }
            let dups: Vec<&String> = seen_global.iter()
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
            let outranked = output.get("boosted_outranked").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let found = outranked.iter().any(|s| {
                s.as_str().map_or(false, |q| q.to_lowercase().contains(&fragment.to_lowercase()))
            });
            if found {
                Ok(())
            } else {
                Err(format!(
                    "boosted_outranked_contains: {:?} not in boosted_outranked; got {:?}",
                    fragment,
                    outranked.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>()
                ))
            }
        }

        // qname_not_in_results: no result has a qname containing `fragment`.
        // Use this to prove a feedback-suppressed symbol is absent from results.
        "qname_not_in_results" => {
            let fragment = str_field(map, "fragment")?;
            let results = output.get("results").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let hit = results.iter().find(|r| {
                r.get("qname").and_then(|v| v.as_str())
                    .map_or(false, |q| q.to_lowercase().contains(&fragment.to_lowercase()))
            });
            match hit {
                Some(r) => {
                    let qname = r.get("qname").and_then(|v| v.as_str()).unwrap_or("?");
                    Err(format!("qname_not_in_results: {:?} is present in results (expected suppressed)", qname))
                }
                None => Ok(()),
            }
        }

        // ambiguous_terms_nonempty: the query has at least one ambiguous term flagged.
        // Use this to verify broad/generic queries signal uncertainty.
        "ambiguous_terms_nonempty" => {
            let terms = output.get("ambiguous_terms").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            if terms.is_empty() {
                Err("ambiguous_terms_nonempty: ambiguous_terms is empty — query may be too specific or detection not firing".to_string())
            } else {
                Ok(())
            }
        }

        // scoped_suggestions_nonempty: scoped_suggestions has at least one entry.
        // Use this to verify broad queries emit narrowing hints.
        "scoped_suggestions_nonempty" => {
            let suggestions = output.get("scoped_suggestions").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            if suggestions.is_empty() {
                Err("scoped_suggestions_nonempty: scoped_suggestions is empty — no narrowing hints emitted".to_string())
            } else {
                Ok(())
            }
        }

        // scoped_suggestions_contains: at least one scoped suggestion contains `fragment`.
        "scoped_suggestions_contains" => {
            let fragment = str_field(map, "fragment")?;
            let suggestions = output.get("scoped_suggestions").and_then(|v| v.as_array()).unwrap_or(&empty_arr);
            let found = suggestions.iter().any(|s| {
                s.as_str().map_or(false, |t| t.to_lowercase().contains(&fragment.to_lowercase()))
            });
            if found {
                Ok(())
            } else {
                Err(format!(
                    "scoped_suggestions_contains: no suggestion contains {:?}; got {:?}",
                    fragment,
                    suggestions.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>()
                ))
            }
        }

        "" => Ok(()), // no kind → smoke test, always passes
        other => Err(format!("unknown assertion kind: {:?}", other)),
    }
}

fn str_field<'a>(map: &'a toml::map::Map<String, toml::Value>, key: &str) -> Result<&'a str, String> {
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
        let rules: Vec<&str> = arr.iter()
            .filter_map(|e| e.get("rule_that_won").and_then(|v| v.as_str()))
            .collect();
        if !rules.is_empty() {
            return Some(format!("classification rules: {:?}", rules));
        }
    }
    if let Some(arr) = json.get("results").and_then(|v| v.as_array()) {
        let top3: Vec<&str> = arr.iter().take(3)
            .filter_map(|r| r.get("qname").and_then(|v| v.as_str()))
            .collect();
        if !top3.is_empty() {
            return Some(format!("top results: {:?}", top3));
        }
    }
    if let Some(arr) = json.get("cluster_debug").and_then(|v| v.as_array()) {
        let winners: Vec<&str> = arr.iter()
            .filter_map(|e| e.get("winner_selected")
                .and_then(|w| w.get("qname"))
                .and_then(|v| v.as_str()))
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

    let conn = open_analytics_db(&db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;

    let raw = std::fs::read_to_string(&jsonl_path)
        .with_context(|| format!("reading {}", jsonl_path.display()))?;

    let mut runs = 0usize;
    let mut probes = 0usize;
    let mut skipped = 0usize;

    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(record) => {
                // Check if already present before insert so we can count skips.
                let run_id = record.get("started_at").and_then(Value::as_str).unwrap_or("");
                let already: bool = conn
                    .query_row("SELECT 1 FROM probe_runs WHERE run_id=?1", params![run_id], |_| Ok(true))
                    .unwrap_or(false);
                if already {
                    skipped += 1;
                    continue;
                }
                let probe_count = record.get("probes")
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

    println!("Indexed {} run(s) ({} probe rows) into {}  [{} already present, skipped]",
        runs, probes, db_path.display(), skipped);
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

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Parse all non-empty lines as JSON records.
    let mut records: Vec<Value> = raw.lines()
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
        println!("{:<10}  {:<16}  {:>5}  {:>5}  {:>10}  {:<8}  {}",
            "version", "scope", "total", "pass", "wall_ms", "budget", "slowest");
        println!("{}", "-".repeat(90));
        for r in &mut window {
            let version  = r.get("asd_version").and_then(Value::as_str).unwrap_or("?");
            let scope    = match (
                r.get("filter_name").and_then(Value::as_str),
                r.get("filter_tag").and_then(Value::as_str),
            ) {
                (Some(n), _) => format!("name:{}", n),
                (_, Some(t)) => format!("tag:{}", t),
                _            => "all".to_string(),
            };
            let total_n  = r.get("total").and_then(Value::as_u64).unwrap_or(0);
            let passed_n = r.get("passed").and_then(Value::as_u64).unwrap_or(0);
            let wall     = r.get("wall_time_ms").and_then(Value::as_u64).unwrap_or(0);
            let budget_ok = r.get("budget_failed").and_then(Value::as_bool)
                .map_or("—", |b| if b { "FAIL" } else { "ok" });
            let slowest_name = r.get("slowest")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("—");
            let slowest_ms = r.get("slowest")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|s| s.get("duration_ms"))
                .and_then(Value::as_u64)
                .map(|ms| format!("({}ms)", ms))
                .unwrap_or_default();
            println!("{:<10}  {:<16}  {:>5}  {:>5}  {:>10}  {:<8}  {} {}",
                version, scope, total_n, passed_n, wall, budget_ok,
                slowest_name, slowest_ms);
        }
        println!("\n{} run(s) shown ({} total recorded)", window.len(), total);
    }
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
