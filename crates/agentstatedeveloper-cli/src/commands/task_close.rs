//! `asd task-close` — capture proof, validation result, and affected symbols
//! and write them atomically to the ledger when closing a task.
//!
//! Reads CTXONE_PLAN / CTXONE_TASK env vars for provenance.
//! Changed files are resolved from git HEAD..HEAD~1 by default or passed
//! via --symbols.

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Engine, IndexStore, LedgerKind, LedgerStore, Symbol,
    schema::{Author, AuthorKind, LedgerEntry},
    append_workflow_session, compute_trust_score,
    detect_workflow, score_evidence_quality, WorkflowSummary,
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct TaskCloseArgs {
    /// Free-text description of what was completed (written as Proof entry).
    /// Omit to default to "task completed".
    pub proof: Option<String>,

    /// Comma-separated fully-qualified symbol names to annotate.
    /// If omitted, symbols are resolved from files changed in HEAD.
    #[arg(long)]
    pub symbols: Option<String>,

    /// Mark the task as validated (writes a ValidationScenario entry too).
    #[arg(long)]
    pub validated: bool,

    /// Optional validation note when --validated is set.
    #[arg(long)]
    pub validation_note: Option<String>,

    /// Reference to validation evidence (file path, URL, or test name).
    /// Appended to the Proof entry summary and written as a KnownBug tag if pointing to a failure.
    #[arg(long)]
    pub evidence: Option<String>,

    /// CTX plan ID (overrides CTXONE_PLAN env var).
    #[arg(long)]
    pub plan: Option<String>,

    /// CTX task ID (overrides CTXONE_TASK env var).
    #[arg(long)]
    pub task: Option<String>,

    /// Author id written into ledger entries.
    #[arg(long, default_value = "asd-task-close")]
    pub author: String,

    /// Suppress informational output.
    #[arg(long)]
    pub quiet: bool,

    /// Emit JSON closure summary (default: true; set --no-json to suppress).
    #[arg(long, default_value_t = true)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: TaskCloseArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore::with_cache(&engine.repo, &cfg.db_path);

    // Resolve CTX plan/task from args or env vars (t-001).
    let plan_id = args.plan.clone()
        .or_else(|| std::env::var("CTXONE_PLAN").ok())
        .unwrap_or_default();
    let task_id = args.task.clone()
        .or_else(|| std::env::var("CTXONE_TASK").ok())
        .unwrap_or_default();

    // Build provenance tags.
    let mut ctx_tags: Vec<String> = Vec::new();
    if !plan_id.is_empty() { ctx_tags.push(format!("ctx:plan:{}", plan_id)); }
    if !task_id.is_empty() { ctx_tags.push(format!("ctx:task:{}", task_id)); }

    // Resolve affected symbols.
    let target_symbols: Vec<Symbol> = if let Some(ref sym_list) = args.symbols {
        sym_list.split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .filter_map(|q| index_store.get_symbol_by_qname(&engine.ref_name, q).ok().flatten())
            .collect()
    } else {
        // Auto-detect from git HEAD changed files.
        let out = std::process::Command::new("git")
            .args(["diff-tree", "--no-commit-id", "-r", "--name-only", "HEAD"])
            .output()
            .unwrap_or_else(|_| std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: vec![],
                stderr: vec![],
            });
        let changed: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        let tree = engine.repo
            .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let mut syms: Vec<Symbol> = tree.as_object()
            .map(|m| m.values()
                .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                .filter(|s| changed.iter().any(|f| s.file.ends_with(f.as_str()) || s.file == *f))
                .collect())
            .unwrap_or_default();
        syms.truncate(20);
        syms
    };

    if target_symbols.is_empty() {
        eprintln!("asd: no symbols resolved — pass --symbols or ensure HEAD has changed files");
        println!("{}", json!({"written": [], "ctx": {"plan": plan_id, "task": task_id}}));
        return Ok(());
    }

    // Build the proof text, incorporating evidence reference if provided.
    let proof_base = args.proof.clone().unwrap_or_else(|| "task completed".to_string());
    let proof_text = if let Some(ref ev) = args.evidence {
        format!("{} [evidence: {}]", proof_base, ev)
    } else {
        proof_base.clone()
    };

    let author = Author { kind: AuthorKind::Human, id: args.author.clone() };
    let mut written: Vec<serde_json::Value> = Vec::new();
    let closed_at = chrono::Utc::now().to_rfc3339();

    for sym in &target_symbols {
        // Write Proof entry.
        let mut proof_entry = LedgerEntry::new(
            &sym.symbol_id, LedgerKind::Proof, &proof_text, author.clone(),
        );
        proof_entry.tags.extend(ctx_tags.iter().cloned());
        if let Some(ref ev) = args.evidence {
            proof_entry.tags.push(format!("evidence:{}", ev));
        }
        ledger_store.append_entry(&engine.ref_name, &proof_entry, &args.author)?;
        written.push(json!({"symbol": sym.qname, "kind": "proof", "summary": proof_text}));

        // Write ValidationScenario if --validated.
        if args.validated {
            let validation_text = args.validation_note.clone()
                .unwrap_or_else(|| format!("validated: {}", proof_base));
            let mut vs_entry = LedgerEntry::new(
                &sym.symbol_id, LedgerKind::ValidationScenario, &validation_text, author.clone(),
            );
            vs_entry.tags.extend(ctx_tags.iter().cloned());
            ledger_store.append_entry(&engine.ref_name, &vs_entry, &args.author)?;
            written.push(json!({"symbol": sym.qname, "kind": "validation_scenario", "summary": validation_text}));
        }
    }

    if !args.quiet {
        eprintln!("asd: wrote {} ledger entries across {} symbols", written.len(), target_symbols.len());
        if !ctx_tags.is_empty() {
            eprintln!("asd: provenance: {}", ctx_tags.join(", "));
        }
    }

    // ── Workflow Integration: evidence quality + recipe detection ──────────
    // Gather all pre-existing ledger entries for touched symbols (exclude the
    // entries we just wrote so they don't inflate the detection signals).
    let pre_existing: Vec<LedgerEntry> = target_symbols.iter()
        .flat_map(|sym| {
            ledger_store.list_entries(&engine.ref_name, &sym.symbol_id)
                .unwrap_or_default()
        })
        .filter(|e| {
            // Exclude entries whose summary matches what we just wrote.
            !written.iter().any(|w| {
                w.get("summary").and_then(|s| s.as_str()) == Some(e.summary.as_str())
                    && w.get("kind").and_then(|k| k.as_str())
                        .map(|k| k == format!("{:?}", e.kind).to_lowercase())
                        .unwrap_or(false)
            })
        })
        .collect();

    let proof_was_explicit = args.proof.is_some();
    let eq = score_evidence_quality(
        &pre_existing,
        args.validated,
        args.evidence.as_deref(),
        proof_was_explicit,
        target_symbols.len(),
        written.len(),
    );

    // Check whether any touched symbols have existing Invariant entries.
    let has_invariants = pre_existing.iter().any(|e| e.kind == LedgerKind::Invariant);
    let (wf_type, steps_detected, missing_steps) = detect_workflow(&pre_existing, &eq, has_invariants);

    // Capture db_state for context — helps agents understand low evidence scores
    // on fresh/unannotated workspaces.
    let trust = compute_trust_score(&cfg.db_path);
    let db_state = trust.data_quality.state.clone();
    let db_state_note = match db_state.as_str() {
        "clean_room" => "fresh workspace — low evidence score is expected before annotations are written".to_string(),
        "unannotated" => "index built but no prior annotations — low evidence score is expected; run `asd annotate-commit` to enrich".to_string(),
        "degraded" => "warning: sparse ledger despite prior activity — possible state loss or DB reset".to_string(),
        _ => String::new(),
    };

    let workflow_summary = WorkflowSummary {
        workflow_type: wf_type,
        steps_detected,
        missing_recommended_steps: missing_steps,
        evidence_quality: eq,
        task_id: task_id.clone(),
        plan_id: plan_id.clone(),
        closed_at: closed_at.clone(),
        symbols_annotated: target_symbols.len(),
        ledger_entries_written: written.len(),
        db_state,
        db_state_note,
    };

    // Persist to .asd/workflow-sessions.jsonl.
    append_workflow_session(&cfg.db_path, &workflow_summary);

    let closure_summary = json!({
        "status": "closed",
        "closed_at": closed_at,
        "proof": proof_text,
        "validated": args.validated,
        "evidence": args.evidence,
        "symbols_annotated": target_symbols.len(),
        "ledger_entries_written": written.len(),
        "ctx": {
            "plan": if plan_id.is_empty() { serde_json::Value::Null } else { json!(plan_id) },
            "task": if task_id.is_empty() { serde_json::Value::Null } else { json!(task_id) },
        },
        "workflow": workflow_summary.to_json(),
        "written": written,
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&closure_summary)?);
    }
    Ok(())
}
