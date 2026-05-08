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
    /// Fully-qualified symbol name being rated.
    pub qname: String,
    /// Verdict: useful, noisy, missing, or wrong_layer.
    pub verdict: String,
    /// Optional free-text note.
    #[arg(long)]
    pub note: Option<String>,
    /// Author identifier recorded with the verdict.
    #[arg(long, default_value = "asd-cli")]
    pub author: String,
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
    match s.to_lowercase().as_str() {
        "useful" => Ok(FeedbackVerdict::Useful),
        "noisy" => Ok(FeedbackVerdict::Noisy),
        "missing" => Ok(FeedbackVerdict::Missing),
        "wrong_layer" => Ok(FeedbackVerdict::WrongLayer),
        other => bail!(
            "unknown verdict {:?}; valid: useful, noisy, missing, wrong_layer",
            other
        ),
    }
}

fn run_mark(cfg: &Config, args: MarkArgs) -> Result<()> {
    let verdict = parse_verdict(&args.verdict)?;
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let symbol = match index_store.get_symbol_by_qname(&engine.ref_name, &args.qname)? {
        Some(s) => s,
        None => bail!("symbol not found: {}", args.qname),
    };
    let entry = FeedbackEntry {
        entry_id: format!("fb_{}", Uuid::new_v4().simple()),
        symbol_id: symbol.symbol_id.clone(),
        symbol_qname: args.qname.clone(),
        query: args.query.to_lowercase().trim().to_string(),
        verdict,
        note: args.note.clone(),
        author: args.author.clone(),
        created_at: chrono::Utc::now(),
    };
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };
    feedback_store.record(&engine.ref_name, &entry, &args.author)?;
    println!("recorded {} for {} ({})", args.verdict, args.qname, entry.entry_id);
    Ok(())
}

fn run_promote_as_truth(cfg: &Config, args: PromoteAsTruthArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
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
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    ledger_store.append_entry(&engine.ref_name, &entry, &args.author)?;
    println!("promoted {} as source-of-truth for \"{}\" ({})", args.qname, args.concept, entry.entry_id);
    Ok(())
}

fn run_list(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };
    let entries = if let Some(ref qname) = args.qname {
        let index_store = AsgIndexStore { repo: &engine.repo };
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

fn run_export(cfg: &Config, args: ExportArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };
    let entries = feedback_store.list_all(&engine.ref_name)?;
    let json = serde_json::to_string_pretty(&entries)?;
    match args.output {
        Some(ref path) => {
            std::fs::write(path, &json)?;
            eprintln!("asd: exported {} feedback entries to {}", entries.len(), path);
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
    let feedback_store = AsgFeedbackStore { repo: &engine.repo };

    let existing_ids: std::collections::HashSet<String> = if args.skip_existing {
        feedback_store
            .list_all(&engine.ref_name)?
            .into_iter()
            .map(|e| e.entry_id)
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for entry in &incoming {
        if args.skip_existing && existing_ids.contains(&entry.entry_id) {
            skipped += 1;
            continue;
        }
        feedback_store.record(&engine.ref_name, entry, &args.author)?;
        imported += 1;
    }
    eprintln!("asd: imported {imported} entries, skipped {skipped} duplicates");
    Ok(())
}
