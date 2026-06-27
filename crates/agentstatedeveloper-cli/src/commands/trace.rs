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
    RuntimeEvidence, Verification, VerificationSource, VerificationStatus, paths,
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
        return Err(anyhow!(
            "asd trace: no command given (use `-- <cmd> [args...]`)"
        ));
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
    let index_store = AsgIndexStore::from_engine(&engine);
    let effect_store = AsgEffectStore::from_engine(&engine);

    let mut traced_qnames: usize = 0;
    let mut updates: Vec<serde_json::Value> = Vec::new();

    for obs in &report.observations {
        let Some(symbol) = index_store.get_symbol_by_qname(&engine.ref_name, &obs.qname)? else {
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
                runtime: None,
                matched_policy: None,
            });

        let declared_set: HashSet<EffectCategory> =
            decl.declared.iter().map(|e| e.effect.clone()).collect();
        let observed_set: HashSet<EffectCategory> = obs
            .observed_effects
            .iter()
            .map(|e| e.effect.clone())
            .collect();

        let mut mismatches: Vec<Mismatch> = Vec::new();
        for obs_e in &obs.observed_effects {
            if !declared_set.contains(&obs_e.effect) && obs_e.effect != EffectCategory::Pure {
                mismatches.push(Mismatch {
                    kind: "undeclared".to_string(),
                    effect: obs_e.effect.clone(),
                    detected_in: Some(obs.qname.clone()),
                    note: obs_e.note.clone(),
                });
            }
        }
        for declared in &decl.declared {
            if !observed_set.contains(&declared.effect) && declared.effect != EffectCategory::Pure {
                mismatches.push(Mismatch {
                    kind: "unobserved".to_string(),
                    effect: declared.effect.clone(),
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

        // Fold this run into the accumulated runtime confidence.
        let contradicted = mismatches.iter().any(|m| m.kind == "undeclared");
        let observed_real = obs
            .observed_effects
            .iter()
            .any(|e| e.effect != EffectCategory::Pure);
        let declared_real = decl
            .declared
            .iter()
            .any(|e| e.effect != EffectCategory::Pure);
        let outcome =
            fold_runtime_evidence(&mut decl, declared_real, observed_real, contradicted, &trace_id);

        effect_store.put_effects(&engine.ref_name, &symbol.symbol_id, &decl, &cfg.agent_id)?;

        updates.push(json!({
            "qname": obs.qname,
            "status": match status {
                VerificationStatus::Ok => "ok",
                VerificationStatus::Mismatch => "mismatch",
                VerificationStatus::Unverified => "unverified",
            },
            "runtime_outcome": outcome,
            "confidence": decl.confidence,
            "runtime_evidence": decl.runtime.as_ref().map(|r| json!({
                "confirmations": r.confirmations,
                "contradictions": r.contradictions,
                "prior": r.prior,
            })),
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

/// Fold one runtime observation into a symbol's accumulated runtime evidence,
/// updating its derived confidence in place. Returns the outcome label.
///
/// Outcomes (see [`RuntimeEvidence`] for the absence-safety rationale):
///   - `"contradiction"` — `contradicted` is true: an effect was observed that
///     wasn't declared. Demotes confidence.
///   - `"confirmation"` — not contradicted AND there is positive evidence: a
///     real (non-Pure) effect was observed, or a pure declaration ran and
///     produced nothing (`!declared_real`). Promotes confidence.
///   - `"neutral"` — not contradicted but no positive evidence (a declared
///     effect simply wasn't exercised). Confidence is left unchanged — absence
///     of observation is not evidence of absence.
fn fold_runtime_evidence(
    decl: &mut agentstatedeveloper_core::EffectDecl,
    declared_real: bool,
    observed_real: bool,
    contradicted: bool,
    trace_id: &str,
) -> &'static str {
    let outcome = if contradicted {
        "contradiction"
    } else if observed_real || !declared_real {
        "confirmation"
    } else {
        "neutral"
    };

    if outcome != "neutral" {
        let mut rt = decl.runtime.take().unwrap_or_else(|| RuntimeEvidence {
            confirmations: 0,
            contradictions: 0,
            prior: decl.confidence.unwrap_or(RuntimeEvidence::NEUTRAL_PRIOR),
            last_trace_id: None,
            last_observed_at: Utc::now(),
        });
        if outcome == "confirmation" {
            rt.confirmations += 1;
        } else {
            rt.contradictions += 1;
        }
        rt.last_trace_id = Some(trace_id.to_string());
        rt.last_observed_at = Utc::now();
        decl.confidence = Some(rt.confidence());
        decl.runtime = Some(rt);
    }

    outcome
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentstatedeveloper_core::EffectDecl;

    fn decl_with(prior: Option<f64>) -> EffectDecl {
        EffectDecl {
            symbol_id: "sym".into(),
            declared: Vec::new(),
            transitive: Vec::new(),
            verification: None,
            confidence: prior,
            runtime: None,
            matched_policy: None,
        }
    }

    // --- classification truth table -------------------------------------

    #[test]
    fn contradiction_always_wins() {
        let mut d = decl_with(None);
        // Even with positive observation, an undeclared effect ⇒ contradiction.
        assert_eq!(fold_runtime_evidence(&mut d, true, true, true, "t"), "contradiction");
    }

    #[test]
    fn observed_real_effect_is_confirmation() {
        let mut d = decl_with(None);
        assert_eq!(fold_runtime_evidence(&mut d, true, true, false, "t"), "confirmation");
    }

    #[test]
    fn pure_declared_pure_observed_is_confirmation() {
        let mut d = decl_with(None);
        // declared_real=false (pure), observed_real=false, not contradicted.
        assert_eq!(fold_runtime_evidence(&mut d, false, false, false, "t"), "confirmation");
    }

    #[test]
    fn declared_effect_not_exercised_is_neutral() {
        let mut d = decl_with(Some(0.5));
        // declared_real=true but nothing observed and no contradiction ⇒ absence.
        let outcome = fold_runtime_evidence(&mut d, true, false, false, "t");
        assert_eq!(outcome, "neutral");
        // Neutral must not create runtime evidence or move confidence.
        assert!(d.runtime.is_none());
        assert_eq!(d.confidence, Some(0.5));
    }

    // --- folding into confidence ----------------------------------------

    #[test]
    fn confirmation_raises_contradiction_lowers_from_prior() {
        let mut d = decl_with(Some(0.5));
        fold_runtime_evidence(&mut d, true, true, false, "t1"); // confirm
        let after_confirm = d.confidence.unwrap();
        assert!(after_confirm > 0.5);
        assert_eq!(d.runtime.as_ref().unwrap().confirmations, 1);
        assert_eq!(d.runtime.as_ref().unwrap().prior, 0.5);

        fold_runtime_evidence(&mut d, true, false, true, "t2"); // contradict
        assert!(d.confidence.unwrap() < after_confirm);
        assert_eq!(d.runtime.as_ref().unwrap().contradictions, 1);
        // Prior stays frozen across ingests.
        assert_eq!(d.runtime.as_ref().unwrap().prior, 0.5);
        assert_eq!(d.runtime.as_ref().unwrap().last_trace_id.as_deref(), Some("t2"));
    }

    #[test]
    fn prior_seeds_from_static_confidence_else_neutral() {
        // No static confidence ⇒ neutral 0.5 prior is captured on first evidence.
        let mut d = decl_with(None);
        fold_runtime_evidence(&mut d, true, true, false, "t");
        assert_eq!(d.runtime.as_ref().unwrap().prior, RuntimeEvidence::NEUTRAL_PRIOR);
    }

    #[test]
    fn neutral_then_confirmation_still_seeds_prior_from_static() {
        let mut d = decl_with(Some(0.8));
        // A neutral run leaves everything untouched...
        assert_eq!(fold_runtime_evidence(&mut d, true, false, false, "t0"), "neutral");
        assert!(d.runtime.is_none());
        // ...so the first decisive run still captures the static prior.
        fold_runtime_evidence(&mut d, true, true, false, "t1");
        assert_eq!(d.runtime.as_ref().unwrap().prior, 0.8);
    }
}
