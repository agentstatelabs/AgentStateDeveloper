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
//!                     total/passed/failed, slowest top-5, slow_violations list)
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
use clap::{Args, Subcommand};
use serde_json::Value;

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

    let mut results: Vec<ProbeResult> = Vec::new();

    for probe in &probes {
        let result = execute_probe(cfg, probe);
        let is_fail = result.error.is_some();
        let is_slow = args.fail_slow.map_or(false, |ms| result.duration_ms > ms as u128);

        if !args.json {
            let status = if is_fail { "FAIL" } else if is_slow { "SLOW" } else { "PASS" };
            let ms = result.duration_ms;
            if is_fail {
                println!("{:<5} {} ({}ms)", status, probe.name, ms);
                println!("      {}", result.error.as_deref().unwrap_or(""));
                if let Some(ref payload) = result.debug_payload_summary {
                    println!("      debug: {}", payload);
                }
            } else {
                println!("{:<5} {} ({}ms)", status, probe.name, ms);
            }
        }
        results.push(result);
        if is_fail && args.fail_fast {
            break;
        }
    }

    let passed = results.iter().filter(|r| r.error.is_none()).count();
    let failed = results.iter().filter(|r| r.error.is_some()).count();

    // Slow violations: probes that exceeded --fail-slow threshold.
    let slow_violations: Vec<&ProbeResult> = if let Some(threshold_ms) = args.fail_slow {
        results.iter().filter(|r| r.duration_ms > threshold_ms as u128).collect()
    } else {
        Vec::new()
    };

    // Top-5 slowest (for JSON output and summary).
    let mut by_duration: Vec<(usize, u128)> = results.iter().enumerate()
        .map(|(i, r)| (i, r.duration_ms))
        .collect();
    by_duration.sort_by(|a, b| b.1.cmp(&a.1));
    let slowest_top5: Vec<Value> = by_duration.iter().take(5).map(|(i, ms)| {
        serde_json::json!({ "name": results[*i].name, "duration_ms": ms })
    }).collect();

    if args.json {
        let json_results: Vec<Value> = results.iter().map(|r| {
            let is_slow = args.fail_slow.map_or(false, |ms| r.duration_ms > ms as u128);
            serde_json::json!({
                "name": r.name,
                "passed": r.error.is_none(),
                "slow": is_slow,
                "duration_ms": r.duration_ms,
                "error": r.error,
                "debug_payload": r.debug_payload,
            })
        }).collect();
        let slow_violation_names: Vec<&str> = slow_violations.iter()
            .map(|r| r.name.as_str())
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "total": results.len(),
            "passed": passed,
            "failed": failed,
            "slow_violations": slow_violation_names,
            "slowest": slowest_top5,
            "results": json_results,
        }))?);
    } else {
        println!("\n{} probe(s): {} passed, {} failed", results.len(), passed, failed);
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

    if failed > 0 || !slow_violations.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

struct ProbeResult {
    name: String,
    duration_ms: u128,
    error: Option<String>,
    debug_payload: Option<Value>,
    debug_payload_summary: Option<String>,
}

fn execute_probe(cfg: &Config, probe: &ProbeEntry) -> ProbeResult {
    let start = Instant::now();

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
                duration_ms: start.elapsed().as_millis(),
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
            duration_ms,
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
                duration_ms,
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
            duration_ms,
            error: None,
            debug_payload: None,
            debug_payload_summary: None,
        },
        Err(msg) => {
            let summary = summarize_debug_payload(&json);
            ProbeResult {
                name: probe.name.clone(),
                duration_ms,
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
