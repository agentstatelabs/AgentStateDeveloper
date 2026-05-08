//! `asd annotate-commit [sha] [--write]` — derive ledger annotations from a commit.
//!
//! Reads a git commit's changed files and commit message, resolves the symbols
//! in those files, and derives candidate ledger entries (decisions, invariants,
//! hazards, proofs, validation scenarios).
//!
//! By default this is a dry-run: it prints the suggested entries as JSON.
//! Pass `--write` to actually append them to the ASD ledger.
//!
//! Annotation syntax recognised in commit message body:
//!   invariant: <text>          → LedgerKind::Invariant
//!   hazard: <text>             → LedgerKind::Hazard
//!   proof: <text>              → LedgerKind::Proof
//!   validation_scenario: <text> → LedgerKind::ValidationScenario
//!   known_bug: <text>          → LedgerKind::KnownBug
//!   decision: <text>           → LedgerKind::Decision   (explicit)
//!   (any other body line)      → LedgerKind::Decision   (implicit, from subject)
//!
//! The subject line always produces a Decision entry for every touched symbol
//! that doesn't already have that exact summary recorded.
//!
//! Integrates with CTX task closure via `--task-description`:
//!   asd annotate-commit --write --task-description "$(ctx task show t-042)"

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgLedgerStore, Engine, LedgerKind, LedgerStore, Symbol,
    schema::{Author, AuthorKind, LedgerEntry},
};

fn is_doc_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    let p = Path::new(&lower);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(ext, "md" | "txt" | "rst" | "adoc" | "asciidoc" | "tex" | "org")
        || name.starts_with("readme")
        || name.starts_with("changelog")
        || name.starts_with("license")
        || name.starts_with("authors")
        || name.starts_with("contributing")
        || lower.contains("/docs/")
        || lower.starts_with("docs/")
        || lower.contains("/doc/")
        || lower.starts_with("doc/")
}

use crate::config::Config;

#[derive(Debug, Args)]
pub struct AnnotateCommitArgs {
    /// Git commit SHA to annotate (default: HEAD).
    pub sha: Option<String>,

    /// Write derived entries to the ASD ledger.
    /// Without this flag the command is a dry-run and only prints suggestions.
    #[arg(long, default_value_t = false)]
    pub write: bool,

    /// Author id written into ledger entries (default: git user.name).
    #[arg(long)]
    pub author: Option<String>,

    /// Additional context from the active CTX task (its title or description).
    /// Appended to the commit message body when deriving annotations.
    #[arg(long)]
    pub task_description: Option<String>,

    /// CTX task ID — written as a `ctx:task:<id>` provenance tag on every entry.
    /// Also accepts CTXONE_TASK env var.
    #[arg(long)]
    pub ctx_task: Option<String>,

    /// CTX plan ID — written as a `ctx:plan:<id>` provenance tag.
    /// Also accepts CTXONE_PLAN env var.
    #[arg(long)]
    pub ctx_plan: Option<String>,

    /// Suppress informational stderr output.
    #[arg(long)]
    pub quiet: bool,
}

pub fn run(cfg: &Config, args: AnnotateCommitArgs) -> Result<()> {
    let sha = args.sha.clone().unwrap_or_else(|| "HEAD".to_string());

    // ---- Commit metadata ------------------------------------------------
    let log_out = Command::new("git")
        .args(["log", "-1", "--format=%H%n%s%n%b", &sha])
        .output()
        .context("git log failed")?;
    let log_str = String::from_utf8_lossy(&log_out.stdout);
    let mut log_lines = log_str.lines();
    let commit_hash = log_lines.next().unwrap_or("").trim().to_string();
    let subject = log_lines.next().unwrap_or("").trim().to_string();
    let body: String = log_lines.collect::<Vec<_>>().join("\n");

    if commit_hash.is_empty() {
        anyhow::bail!("could not resolve commit: {}", sha);
    }

    // Append optional task description to body for annotation extraction.
    let full_body = if let Some(ref td) = args.task_description {
        format!("{body}\n{td}")
    } else {
        body
    };

    // ---- Changed files ---------------------------------------------------
    let diff_out = Command::new("git")
        .args(["diff-tree", "--no-commit-id", "-r", "--name-only", &commit_hash])
        .output()
        .context("git diff-tree failed")?;
    let changed_files: Vec<String> = String::from_utf8_lossy(&diff_out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if changed_files.is_empty() {
        println!("{}", json!({ "commit": commit_hash, "subject": subject, "note": "no changed files detected", "suggested_entries": [] }));
        return Ok(());
    }

    // ---- Resolve symbols for changed files --------------------------------
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };

    // Read all indexed symbols from the git tree (same approach as build_id_map).
    let tree = engine.repo
        .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap_or(serde_json::Value::Object(Default::default()));
    let all_syms: Vec<Symbol> = tree.as_object()
        .map(|m| m.values()
            .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
            .collect())
        .unwrap_or_default();

    let mut touched_symbols: Vec<Symbol> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for sym in all_syms {
        if changed_files.iter().any(|f| sym.file.ends_with(f.as_str()) || sym.file == *f) {
            if seen_ids.insert(sym.symbol_id.clone()) {
                touched_symbols.push(sym);
            }
        }
    }

    // Limit to 20 most relevant symbols to keep output manageable.
    touched_symbols.truncate(20);

    // ---- Docs-only path: find nearest domain symbol when all changed files
    //      are documentation (*.md, docs/ dirs, etc.) and no symbols were found.
    let all_docs = !changed_files.is_empty() && changed_files.iter().all(|f| is_doc_file(f));
    if all_docs && touched_symbols.is_empty() {
        // Stage 1: extract domain terms from doc filenames and score symbols by name match.
        let doc_terms: Vec<String> = changed_files.iter()
            .flat_map(|f| {
                let stem = Path::new(f).file_stem()
                    .and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                // Split on _, -, whitespace, and camelCase boundaries.
                let with_spaces = stem.replace(['-', '_', '.'], " ");
                with_spaces.split_whitespace()
                    .filter(|w| w.len() >= 3 && !matches!(*w, "the" | "and" | "for" | "doc"
                        | "docs" | "readme" | "design" | "notes" | "plan" | "spec"))
                    .map(|w| w.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();

        let tree2 = engine.repo
            .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let candidate_syms: Vec<Symbol> = tree2.as_object()
            .map(|m| m.values()
                .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                .filter(|s| !is_doc_file(&s.file))
                .collect())
            .unwrap_or_default();

        let mut scored: Vec<(usize, Symbol)> = candidate_syms.iter()
            .map(|s| {
                let haystack = format!("{} {}", s.qname.to_lowercase(), s.file.to_lowercase());
                let name_score: usize = doc_terms.iter()
                    .filter(|t| haystack.contains(t.as_str()))
                    .count();
                (name_score, s.clone())
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        // If name-term matching found strong hits (score ≥ 1), use them.
        // Otherwise fall back to directory heuristics.
        let best_score = scored.first().map(|(s, _)| *s).unwrap_or(0);
        let candidates: Vec<Symbol> = if best_score >= 1 {
            scored.into_iter()
                .filter(|(s, _)| *s >= best_score.max(1))
                .map(|(_, sym)| sym)
                .take(5)
                .collect()
        } else {
            let doc_dirs: Vec<&str> = changed_files.iter()
                .filter_map(|f| Path::new(f).parent()?.to_str())
                .collect();
            let mut dir_scored: Vec<(usize, Symbol)> = candidate_syms.into_iter()
                .map(|s| {
                    let score = doc_dirs.iter().filter(|d| {
                        !d.is_empty() && **d != "." && s.file.contains(**d)
                    }).count();
                    (score, s)
                })
                .collect();
            dir_scored.sort_by(|a, b| b.0.cmp(&a.0));
            dir_scored.into_iter().take(5).map(|(_, sym)| sym).collect()
        };

        for sym in candidates {
            if seen_ids.insert(sym.symbol_id.clone()) {
                touched_symbols.push(sym);
            }
        }
        if !args.quiet {
            eprintln!("asd: docs-only commit detected — annotating nearest domain symbols");
        }
    }

    // ---- Parse commit message for structured annotations -----------------
    #[derive(Debug)]
    struct Annotation {
        kind: LedgerKind,
        summary: String,
    }

    let mut annotations: Vec<Annotation> = Vec::new();

    // Docs-only commits: subject becomes a Concept entry (documentation, not code decision).
    // Code commits: subject becomes a Decision.
    if !subject.is_empty() {
        let subject_kind = if all_docs { LedgerKind::Concept } else { LedgerKind::Decision };
        annotations.push(Annotation { kind: subject_kind, summary: subject.clone() });
    }
    // For docs-only commits with no structured body annotations, derive a Concept
    // from the changed file names to capture what was documented.
    if all_docs && full_body.lines().all(|l| {
        let l = l.trim();
        l.is_empty() || !["invariant:", "hazard:", "proof:", "validation_scenario:",
            "known_bug:", "concept:", "decision:"].iter().any(|p| l.starts_with(p))
    }) {
        for file in changed_files.iter().take(3) {
            let name = Path::new(file).file_stem()
                .and_then(|s| s.to_str()).unwrap_or(file);
            let concept_summary = format!("documentation: {}", name.replace(['-', '_'], " "));
            annotations.push(Annotation { kind: LedgerKind::Concept, summary: concept_summary });
        }
    }

    // Parse body lines for structured annotations.
    for line in full_body.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }

        let (kind, text) = if let Some(rest) = line.strip_prefix("invariant:") {
            (LedgerKind::Invariant, rest.trim())
        } else if let Some(rest) = line.strip_prefix("hazard:") {
            (LedgerKind::Hazard, rest.trim())
        } else if let Some(rest) = line.strip_prefix("proof:") {
            (LedgerKind::Proof, rest.trim())
        } else if let Some(rest) = line.strip_prefix("validation_scenario:") {
            (LedgerKind::ValidationScenario, rest.trim())
        } else if let Some(rest) = line.strip_prefix("known_bug:") {
            (LedgerKind::KnownBug, rest.trim())
        } else if let Some(rest) = line.strip_prefix("concept:") {
            (LedgerKind::Concept, rest.trim())
        } else if let Some(rest) = line.strip_prefix("decision:") {
            (LedgerKind::Decision, rest.trim())
        } else {
            continue; // unstructured body lines are skipped
        };

        if !text.is_empty() {
            annotations.push(Annotation { kind, summary: text.to_string() });
        }
    }

    // ---- CTX provenance tags (t-001) -------------------------------------
    let ctx_plan = args.ctx_plan.clone()
        .or_else(|| std::env::var("CTXONE_PLAN").ok())
        .unwrap_or_default();
    let ctx_task = args.ctx_task.clone()
        .or_else(|| std::env::var("CTXONE_TASK").ok())
        .unwrap_or_default();
    let mut ctx_tags: Vec<String> = Vec::new();
    if !ctx_plan.is_empty() { ctx_tags.push(format!("ctx:plan:{}", ctx_plan)); }
    if !ctx_task.is_empty() { ctx_tags.push(format!("ctx:task:{}", ctx_task)); }
    ctx_tags.push(format!("commit:{}", &commit_hash[..8.min(commit_hash.len())]));

    // ---- Determine author ------------------------------------------------
    let author_id = args.author.clone().unwrap_or_else(|| {
        let out = Command::new("git")
            .args(["config", "user.name"])
            .output()
            .ok();
        out.and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    });
    let author = Author { kind: AuthorKind::Human, id: author_id };

    // ---- Build suggested entries -----------------------------------------
    let mut suggested: Vec<Value> = Vec::new();
    let mut written: Vec<Value> = Vec::new();

    for sym in &touched_symbols {
        // Only write Decision entries for each annotation; other kinds only
        // when explicitly tagged in the commit body.
        for ann in &annotations {
            // Check for duplicate: skip if same summary + kind already recorded.
            let existing = ledger_store
                .list_entries(&engine.ref_name, &sym.symbol_id)
                .unwrap_or_default();
            let already_exists = existing.iter().any(|e| {
                e.kind == ann.kind && e.summary.to_lowercase() == ann.summary.to_lowercase()
            });
            if already_exists { continue; }

            let entry_val = json!({
                "symbol": sym.qname,
                "file": sym.file,
                "kind": format!("{:?}", ann.kind).to_lowercase(),
                "summary": ann.summary,
            });

            if args.write {
                let mut entry = LedgerEntry::new(
                    sym.symbol_id.clone(),
                    ann.kind,
                    ann.summary.clone(),
                    author.clone(),
                );
                entry.tags.extend(ctx_tags.iter().cloned());
                match ledger_store.append_entry(&engine.ref_name, &entry, &author.id) {
                    Ok(()) => written.push(entry_val),
                    Err(e) => eprintln!("warn: could not write entry for {}: {e}", sym.qname),
                }
            } else {
                suggested.push(entry_val);
            }
        }
    }

    let out = if args.write {
        json!({
            "commit": commit_hash,
            "subject": subject,
            "changed_files": changed_files,
            "touched_symbols": touched_symbols.len(),
            "written_entries": written,
        })
    } else {
        json!({
            "commit": commit_hash,
            "subject": subject,
            "changed_files": changed_files,
            "touched_symbols": touched_symbols.iter().map(|s| &s.qname).collect::<Vec<_>>(),
            "suggested_entries": suggested,
            "note": "dry-run — pass --write to record these entries",
        })
    };

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
