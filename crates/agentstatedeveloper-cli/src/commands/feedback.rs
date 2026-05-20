//! `asd feedback` — record and list search-quality verdicts.
//!
//! Subcommands:
//!   mark  — attach a verdict (useful/noisy/missing/wrong_layer) to a (query, symbol) pair
//!   list  — display all recorded verdicts, optionally filtered to one symbol

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use uuid::Uuid;

use agentstatedeveloper_core::{
    AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, FeedbackEntry,
    FeedbackStore, FeedbackVerdict, IndexStore, LedgerEntry, LedgerKind, LedgerStore,
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum FeedbackCmd {
    /// Record a verdict for a (query, symbol) result.
    Mark(MarkArgs),
    /// List recorded feedback verdicts.
    List(ListArgs),
    /// Designate a symbol as the canonical source-of-truth for a domain concept.
    PromoteAsTruth(PromoteAsTruthArgs),
    /// Export all feedback entries to a JSON file (or stdout).
    Export(ExportArgs),
    /// Import feedback entries from a JSON file (or stdin).
    Import(ImportArgs),
}

#[derive(Debug, Args)]
pub struct MarkArgs {
    /// The search query that produced this result.
    pub query: String,
    /// Fully-qualified symbol name being rated. Omit (use "") when --file-scope is set.
    pub qname: String,
    /// Verdict: useful, noisy, missing, or wrong_layer.
    pub verdict: String,
    /// Optional free-text note.
    #[arg(long)]
    pub note: Option<String>,
    /// Author identifier recorded with the verdict.
    #[arg(long, default_value = "asd-cli")]
    pub author: String,
    /// Apply verdict to all symbols in files matching this glob (e.g. "src/adapters/*.rs").
    #[arg(long)]
    pub file_scope: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter to a specific symbol qname. Omit to list all.
    pub qname: Option<String>,
    /// Emit JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PromoteAsTruthArgs {
    /// Fully-qualified symbol name to promote.
    pub qname: String,
    /// The domain concept this symbol is the source-of-truth for.
    #[arg(long)]
    pub concept: String,
    /// Author identifier recorded with the ownership entry.
    #[arg(long, default_value = "asd-cli")]
    pub author: String,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Output file path. Omit to write to stdout.
    #[arg(long)]
    pub output: Option<String>,
    /// Emit a JSON summary to stdout instead of the full entry list.
    #[arg(long)]
    pub summary: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Input file path. Omit to read from stdin.
    #[arg(long)]
    pub input: Option<String>,
    /// Skip entries whose entry_id already exists in the store.
    #[arg(long, default_value = "true")]
    pub skip_existing: bool,
    /// Author recorded for imported entries.
    #[arg(long, default_value = "asd-import")]
    pub author: String,
    /// Show what would be imported without writing to the store.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(cfg: &Config, cmd: FeedbackCmd) -> Result<()> {
    match cmd {
        FeedbackCmd::Mark(args) => run_mark(cfg, args),
        FeedbackCmd::List(args) => run_list(cfg, args),
        FeedbackCmd::PromoteAsTruth(args) => run_promote_as_truth(cfg, args),
        FeedbackCmd::Export(args) => run_export(cfg, args),
        FeedbackCmd::Import(args) => run_import(cfg, args),
    }
}

fn parse_verdict(s: &str) -> Result<FeedbackVerdict> {
    // Plan C t-005: delegate to FeedbackVerdict::from_str so the verdict
    // taxonomy stays single-sourced in core. Forgiving on kebab/snake.
    if let Some(v) = FeedbackVerdict::from_str(s) {
        return Ok(v);
    }
    bail!(
        "unknown verdict {:?}; valid: useful, noisy, missing, wrong_layer, already_covered, diagnostic_only",
        s
    )
}

fn run_mark(cfg: &Config, args: MarkArgs) -> Result<()> {
    let verdict = parse_verdict(&args.verdict)?;
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let (symbol_id, symbol_qname) = if let Some(ref glob) = args.file_scope {
        // File-scoped verdict: no specific symbol required.
        (format!("__file_scope__{}", Uuid::new_v4().simple()), glob.clone())
    } else {
        let index_store = AsgIndexStore::from_engine(&engine);
        let symbol = match index_store.get_symbol_by_qname(&engine.ref_name, &args.qname)? {
            Some(s) => s,
            None => bail!("symbol not found: {}", args.qname),
        };
        (symbol.symbol_id, args.qname.clone())
    };
    let entry = FeedbackEntry {
        entry_id: format!("fb_{}", Uuid::new_v4().simple()),
        symbol_id,
        symbol_qname: symbol_qname.clone(),
        query: args.query.to_lowercase().trim().to_string(),
        verdict,
        note: args.note.clone(),
        author: args.author.clone(),
        created_at: chrono::Utc::now(),
        file_scope: args.file_scope.clone(),
    };
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    feedback_store.record(&engine.ref_name, &entry, &args.author)?;
    if args.file_scope.is_some() {
        println!("recorded {} for files matching {:?} ({})", args.verdict, symbol_qname, entry.entry_id);
    } else {
        println!("recorded {} for {} ({})", args.verdict, args.qname, entry.entry_id);
    }
    Ok(())
}

fn run_promote_as_truth(cfg: &Config, args: PromoteAsTruthArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore::from_engine(&engine);
    let symbol = match index_store.get_symbol_by_qname(&engine.ref_name, &args.qname)? {
        Some(s) => s,
        None => bail!("symbol not found: {}", args.qname),
    };
    let author_kind = if args.author == "asd-cli" { AuthorKind::Human } else { AuthorKind::Agent };
    let mut entry = LedgerEntry::new(
        &symbol.symbol_id,
        LedgerKind::Ownership,
        &args.concept,
        Author { kind: author_kind, id: args.author.clone() },
    );
    entry.tags = vec!["promote-as-truth".to_string()];
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    ledger_store.append_entry(&engine.ref_name, &entry, &args.author)?;
    println!("promoted {} as source-of-truth for \"{}\" ({})", args.qname, args.concept, entry.entry_id);
    Ok(())
}

fn run_list(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    let entries = if let Some(ref qname) = args.qname {
        let index_store = AsgIndexStore::from_engine(&engine);
        match index_store.get_symbol_by_qname(&engine.ref_name, qname)? {
            Some(sym) => feedback_store.list_for_symbol(&engine.ref_name, &sym.symbol_id)?,
            None => bail!("symbol not found: {}", qname),
        }
    } else {
        feedback_store.list_all(&engine.ref_name)?
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("no feedback recorded");
        return Ok(());
    }

    for e in &entries {
        println!(
            "[{}] {:?}  {}  query={:?}  author={}",
            e.entry_id, e.verdict, e.symbol_qname, e.query, e.author
        );
        if let Some(ref note) = e.note {
            println!("    note: {}", note);
        }
    }
    Ok(())
}

fn verdict_breakdown(entries: &[FeedbackEntry]) -> (usize, usize, usize, usize) {
    let useful  = entries.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::Useful)).count();
    let noisy   = entries.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::Noisy)).count();
    let missing = entries.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::Missing)).count();
    let wl      = entries.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::WrongLayer)).count();
    (useful, noisy, missing, wl)
}

fn run_export(cfg: &Config, args: ExportArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    let entries = feedback_store.list_all(&engine.ref_name)?;

    if args.summary {
        let (useful, noisy, missing, wl) = verdict_breakdown(&entries);
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "total": entries.len(),
            "by_verdict": {
                "useful": useful,
                "noisy": noisy,
                "missing": missing,
                "wrong_layer": wl,
            },
            "db": cfg.db_path.display().to_string(),
        }))?);
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&entries)?;
    let (useful, noisy, missing, wl) = verdict_breakdown(&entries);
    match args.output {
        Some(ref path) => {
            std::fs::write(path, &json)?;
            eprintln!(
                "asd: exported {} feedback entries to {} (useful={}, noisy={}, missing={}, wrong_layer={})",
                entries.len(), path, useful, noisy, missing, wl
            );
        }
        None => println!("{}", json),
    }
    Ok(())
}

fn run_import(cfg: &Config, args: ImportArgs) -> Result<()> {
    use agentstatedeveloper_core::schema::FeedbackEntry;
    let raw = match args.input {
        Some(ref path) => std::fs::read_to_string(path)?,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    let incoming: Vec<FeedbackEntry> = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("failed to parse feedback JSON: {e}"))?;

    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore::from_engine(&engine);

    let existing_ids: std::collections::HashSet<String> = if args.skip_existing || args.dry_run {
        feedback_store
            .list_all(&engine.ref_name)?
            .into_iter()
            .map(|e| e.entry_id)
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut to_import: Vec<&FeedbackEntry> = Vec::new();
    let mut skipped = 0usize;
    for entry in &incoming {
        if (args.skip_existing || args.dry_run) && existing_ids.contains(&entry.entry_id) {
            skipped += 1;
            continue;
        }
        to_import.push(entry);
    }

    let (useful, noisy, missing, wl) = {
        let u = to_import.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::Useful)).count();
        let n = to_import.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::Noisy)).count();
        let m = to_import.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::Missing)).count();
        let w = to_import.iter().filter(|e| matches!(e.verdict, FeedbackVerdict::WrongLayer)).count();
        (u, n, m, w)
    };

    if args.dry_run {
        eprintln!(
            "asd: [dry-run] would import {} entries, skip {} duplicates (useful={}, noisy={}, missing={}, wrong_layer={})",
            to_import.len(), skipped, useful, noisy, missing, wl
        );
        return Ok(());
    }

    for entry in &to_import {
        feedback_store.record(&engine.ref_name, entry, &args.author)?;
    }
    eprintln!(
        "asd: imported {} entries, skipped {} duplicates (useful={}, noisy={}, missing={}, wrong_layer={})",
        to_import.len(), skipped, useful, noisy, missing, wl
    );
    Ok(())
}
