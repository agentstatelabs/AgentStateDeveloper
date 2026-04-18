//! `asd trace -- <cmd> [args...]` — run a Python program under the ASD
//! runtime tracer (`tools/asd_tracer.py`), then ingest the resulting JSON
//! report into ASG as Trace records. Updates each symbol's EffectDecl
//! verification block to reflect what the tracer actually observed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, EffectCategory, EffectStore, Engine, IndexStore, Mismatch,
    Verification, VerificationSource, VerificationStatus, paths,
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct TraceArgs {
    /// Where the tracer should write its JSON report.
    #[arg(long, default_value = ".asd-trace.json")]
    pub out: PathBuf,

    /// Command to run under the tracer (use `--` to separate).
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Report {
    #[serde(default)]
    #[allow(dead_code)]
    command: Vec<String>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    observations: Vec<Observation>,
}

#[derive(Debug, Deserialize)]
struct Observation {
    qname: String,
    observed_effects: Vec<ObservedEffect>,
    call_count: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ObservedEffect {
    effect: EffectCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn run(cfg: &Config, args: TraceArgs) -> Result<()> {
    if args.command.is_empty() {
        return Err(anyhow!("asd trace: no command given (use `-- <cmd> [args...]`)"));
    }

    let tracer = locate_tracer()?;

    let status = Command::new("python3")
        .arg(&tracer)
        .arg("--")
        .args(&args.command)
        .env("ASD_TRACE_OUT", &args.out)
        .status()
        .with_context(|| "failed to invoke python3 asd_tracer.py")?;

    // Even if the traced program failed, we still try to ingest the report —
    // the tracer writes it in a `finally` block.
    let report_bytes = std::fs::read(&args.out)
        .with_context(|| format!("read trace report at {}", args.out.display()))?;
    let report: Report = serde_json::from_slice(&report_bytes)
        .with_context(|| format!("parse trace report at {}", args.out.display()))?;

    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };

    let mut traced_qnames: usize = 0;
    let mut updates: Vec<serde_json::Value> = Vec::new();

    for obs in &report.observations {
        let Some(symbol) =
            index_store.get_symbol_by_qname(&engine.ref_name, &obs.qname)?
        else {
            // qname not in the ASG — user code wasn't indexed; skip silently.
            continue;
        };

        traced_qnames += 1;

        // 1. Write the raw Trace record at /asd/v1/traces/<symbol_id>/<trc_*>.
        let trace_id = format!("trc_{}", Uuid::new_v4().simple());
        let trace_path = paths::traces_path(&symbol.symbol_id, &trace_id);
        let trace_value = json!({
            "qname": obs.qname,
            "observed_effects": obs.observed_effects,
            "call_count": obs.call_count,
            "started_at": report.started_at,
            "finished_at": report.finished_at,
        });
        let opts = CommitOptions::new(
            &cfg.agent_id,
            IntentCategory::Explore,
            format!("runtime trace for {}", obs.qname),
        );
        engine
            .repo
            .set_json(&engine.ref_name, &trace_path, &trace_value, opts)?;

        // 2. Diff declared vs observed, update the EffectDecl.verification.
        let mut decl = effect_store
            .get_effects(&engine.ref_name, &symbol.symbol_id)?
            .unwrap_or_else(|| agentstatedeveloper_core::EffectDecl {
                symbol_id: symbol.symbol_id.clone(),
                declared: Vec::new(),
                transitive: Vec::new(),
                verification: None,
                confidence: None,
                matched_policy: None,
            });

        let declared_set: HashSet<EffectCategory> =
            decl.declared.iter().map(|e| e.effect).collect();
        let observed_set: HashSet<EffectCategory> =
            obs.observed_effects.iter().map(|e| e.effect).collect();

        let mut mismatches: Vec<Mismatch> = Vec::new();
        for obs_e in &obs.observed_effects {
            if !declared_set.contains(&obs_e.effect) && obs_e.effect != EffectCategory::Pure {
                mismatches.push(Mismatch {
                    kind: "undeclared".to_string(),
                    effect: obs_e.effect,
                    detected_in: Some(obs.qname.clone()),
                    note: obs_e.note.clone(),
                });
            }
        }
        for declared in &decl.declared {
            if !observed_set.contains(&declared.effect) && declared.effect != EffectCategory::Pure
            {
                mismatches.push(Mismatch {
                    kind: "unobserved".to_string(),
                    effect: declared.effect,
                    detected_in: Some(obs.qname.clone()),
                    note: Some("declared but not seen at runtime".to_string()),
                });
            }
        }

        let status = if mismatches.is_empty() {
            VerificationStatus::Ok
        } else {
            VerificationStatus::Mismatch
        };

        decl.verification = Some(Verification {
            by: VerificationSource::RuntimeTracer,
            at: Utc::now(),
            status,
            mismatches: mismatches.clone(),
        });

        effect_store.put_effects(&engine.ref_name, &symbol.symbol_id, &decl, &cfg.agent_id)?;

        updates.push(json!({
            "qname": obs.qname,
            "status": match status {
                VerificationStatus::Ok => "ok",
                VerificationStatus::Mismatch => "mismatch",
                VerificationStatus::Unverified => "unverified",
            },
            "mismatches": mismatches,
        }));
    }

    let summary = json!({
        "exit_code": status.code(),
        "report_path": args.out.display().to_string(),
        "traced_qnames": traced_qnames,
        "updates": updates,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);

    // Exit with the underlying command's status so CI can pick it up.
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Locate `asd_tracer.py`. Tries:
///   1. `./tools/asd_tracer.py` (relative to cwd)
///   2. `<binary_dir>/../../tools/asd_tracer.py`
///   3. `<binary_dir>/../tools/asd_tracer.py`
fn locate_tracer() -> Result<PathBuf> {
    let cwd_candidate = PathBuf::from("tools/asd_tracer.py");
    if cwd_candidate.exists() {
        return Ok(cwd_candidate);
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1) {
            let candidate = ancestor.join("tools").join("asd_tracer.py");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Walk up from cwd too, in case user runs from a nested directory.
    if let Ok(cwd) = std::env::current_dir() {
        let mut cur: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cur {
            let candidate = dir.join("tools").join("asd_tracer.py");
            if candidate.exists() {
                return Ok(candidate);
            }
            cur = dir.parent();
        }
    }
    Err(anyhow!(
        "could not find tools/asd_tracer.py — run from the repo root or install the tracer alongside the binary"
    ))
}
