//! `asd think <verb>` — capture agent thinking (Plan G t-003).
//!
//! Five verbs:
//!   - speculate <qname> --conf F --summary S    → Hypothesis
//!   - model NAME --symbols q1,q2 --summary S    → MentalModel
//!   - failed <qname> --tried T --because B       → FailedAttempt
//!   - question <qname> --q Q                    → OpenQuestion
//!   - list [--kind X] [--symbol Q]              → read-side
//!
//! Entries use deterministic blake3-derived IDs so re-running the
//! initial-read prompt overwrites instead of duplicating.

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, ConclusionClass, Engine, IndexStore,
    LedgerEntry, LedgerKind, LedgerStore,
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum ThinkCmd {
    /// Record a Hypothesis: speculation with a confidence (0.0–1.0).
    Speculate(SpeculateArgs),
    /// Record a MentalModel: multi-symbol structural understanding.
    Model(ModelArgs),
    /// Record a FailedAttempt: negative evidence.
    Failed(FailedArgs),
    /// Record an OpenQuestion: known unknown blocking confident action.
    Question(QuestionArgs),
    /// List captured thinking entries.
    List(ListArgs),
}

#[derive(Debug, Args)]
pub struct SpeculateArgs {
    /// Fully-qualified symbol this hypothesis is about.
    pub qname: String,
    /// Confidence in [0.0, 1.0]. Below 0.3 is excluded from
    /// prior_thinking auto-surface by default.
    #[arg(long)]
    pub conf: f64,
    /// One-line claim.
    #[arg(long)]
    pub summary: String,
    /// Optional evidence body (markdown).
    #[arg(long)]
    pub body: Option<String>,
}

#[derive(Debug, Args)]
pub struct ModelArgs {
    /// Short name for the model (becomes the entry summary prefix).
    pub name: String,
    /// Comma-separated qnames the model spans.
    #[arg(long)]
    pub symbols: String,
    /// One-line description of how data/control flows.
    #[arg(long)]
    pub summary: String,
}

#[derive(Debug, Args)]
pub struct FailedArgs {
    /// Fully-qualified symbol the attempt targeted.
    pub qname: String,
    /// What was tried (one line).
    #[arg(long)]
    pub tried: String,
    /// Why it didn't work (one line).
    #[arg(long)]
    pub because: String,
}

#[derive(Debug, Args)]
pub struct QuestionArgs {
    /// Fully-qualified symbol the question is about.
    pub qname: String,
    /// The question (one line).
    #[arg(long = "q")]
    pub question: String,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter to one thinking kind.
    #[arg(long, value_enum)]
    pub kind: Option<ThinkKind>,
    /// Filter to one symbol.
    #[arg(long)]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ThinkKind {
    Hypothesis,
    Model,
    Failed,
    Question,
}

pub fn run(cfg: &Config, cmd: ThinkCmd) -> Result<()> {
    match cmd {
        ThinkCmd::Speculate(a) => run_speculate(cfg, a),
        ThinkCmd::Model(a) => run_model(cfg, a),
        ThinkCmd::Failed(a) => run_failed(cfg, a),
        ThinkCmd::Question(a) => run_question(cfg, a),
        ThinkCmd::List(a) => run_list(cfg, a),
    }
}

fn open_with_symbol(cfg: &Config, qname: &str) -> Result<(Engine, String)> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index = AsgIndexStore::from_engine(&engine);
    let sym = index
        .get_symbol_by_qname(&engine.ref_name, qname)?
        .ok_or_else(|| anyhow!("symbol not found: {}", qname))?;
    Ok((engine, sym.symbol_id))
}

fn agent_author(cfg: &Config) -> Author {
    Author { kind: AuthorKind::Agent, id: cfg.agent_id.clone() }
}

/// Deterministic entry id so re-running the initial-read prompt
/// overwrites instead of duplicating. Same (intent, qname, summary)
/// → same id.
fn det_id(intent: &str, qname: &str, content: &str) -> String {
    let key = format!("think:{intent}:{qname}:{content}");
    let h = blake3::hash(key.as_bytes()).to_hex();
    let short: String = h.chars().take(24).collect();
    format!("led_think_{short}")
}

fn run_speculate(cfg: &Config, args: SpeculateArgs) -> Result<()> {
    if !(0.0..=1.0).contains(&args.conf) {
        bail!("--conf must be in [0.0, 1.0]; got {}", args.conf);
    }
    let (engine, symbol_id) = open_with_symbol(cfg, &args.qname)?;
    let ledger = AsgLedgerStore::from_engine(&engine);
    let mut entry = LedgerEntry::new(
        &symbol_id,
        LedgerKind::Hypothesis,
        &args.summary,
        agent_author(cfg),
    );
    entry.entry_id = det_id("hypothesis", &args.qname, &args.summary);
    entry.confidence = Some(args.conf);
    entry.body = args.body;
    entry.tags.push("source:asd-think".into());
    ledger.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;
    println!(
        "{}",
        json!({
            "ok": true, "kind": "hypothesis", "qname": args.qname,
            "confidence": args.conf, "entry_id": entry.entry_id,
        })
    );
    Ok(())
}

fn run_model(cfg: &Config, args: ModelArgs) -> Result<()> {
    let symbols: Vec<String> = args
        .symbols
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if symbols.is_empty() {
        bail!("--symbols must list at least one qname (comma-separated)");
    }
    // Anchor the model on the FIRST symbol — that's the lookup target
    // for prepare-change's prior_thinking surfacing.
    let (engine, symbol_id) = open_with_symbol(cfg, &symbols[0])?;
    let ledger = AsgLedgerStore::from_engine(&engine);
    let body = json!({ "symbols": symbols, "name": args.name }).to_string();
    let mut entry = LedgerEntry::new(
        &symbol_id,
        LedgerKind::MentalModel,
        format!("{}: {}", args.name, args.summary),
        agent_author(cfg),
    );
    entry.entry_id = det_id("model", &args.name, &args.summary);
    entry.body = Some(body);
    entry.tags.push("source:asd-think".into());
    ledger.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;
    println!(
        "{}",
        json!({
            "ok": true, "kind": "mental_model", "name": args.name,
            "symbols": symbols, "entry_id": entry.entry_id,
        })
    );
    Ok(())
}

fn run_failed(cfg: &Config, args: FailedArgs) -> Result<()> {
    let (engine, symbol_id) = open_with_symbol(cfg, &args.qname)?;
    let ledger = AsgLedgerStore::from_engine(&engine);
    let body = json!({ "tried": &args.tried, "because": &args.because }).to_string();
    let mut entry = LedgerEntry::new(
        &symbol_id,
        LedgerKind::FailedAttempt,
        format!("failed: {} — because {}", args.tried, args.because),
        agent_author(cfg),
    );
    entry.entry_id = det_id("failed", &args.qname, &args.tried);
    entry.body = Some(body);
    entry.tags.push("source:asd-think".into());
    ledger.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;
    println!(
        "{}",
        json!({
            "ok": true, "kind": "failed_attempt", "qname": args.qname,
            "entry_id": entry.entry_id,
        })
    );
    Ok(())
}

fn run_question(cfg: &Config, args: QuestionArgs) -> Result<()> {
    let (engine, symbol_id) = open_with_symbol(cfg, &args.qname)?;
    let ledger = AsgLedgerStore::from_engine(&engine);
    let mut entry = LedgerEntry::new(
        &symbol_id,
        LedgerKind::OpenQuestion,
        &args.question,
        agent_author(cfg),
    );
    entry.entry_id = det_id("question", &args.qname, &args.question);
    entry.tags.push("source:asd-think".into());
    ledger.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;
    println!(
        "{}",
        json!({
            "ok": true, "kind": "open_question", "qname": args.qname,
            "entry_id": entry.entry_id,
        })
    );
    Ok(())
}

fn run_list(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let ref_name = engine.ref_name.clone();

    let kind_filter = args.kind.map(|k| match k {
        ThinkKind::Hypothesis => LedgerKind::Hypothesis,
        ThinkKind::Model => LedgerKind::MentalModel,
        ThinkKind::Failed => LedgerKind::FailedAttempt,
        ThinkKind::Question => LedgerKind::OpenQuestion,
    });

    // Resolve which symbols to scan.
    let symbol_ids: Vec<(String, String)> = if let Some(qname) = args.symbol.as_deref() {
        match index.get_symbol_by_qname(&ref_name, qname)? {
            Some(sym) => vec![(sym.symbol_id, sym.qname)],
            None => return Err(anyhow!("symbol not found: {qname}")),
        }
    } else {
        let prefix = format!(
            "{}/index/by-qname",
            agentstatedeveloper_core::ASD_PATH_PREFIX
        );
        let tree = engine
            .repo
            .get_tree(&ref_name, &prefix)
            .unwrap_or(serde_json::Value::Null);
        let qnames: Vec<String> = match tree {
            serde_json::Value::Object(m) => m.keys().cloned().collect(),
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for qn in qnames {
            if let Some(sym) = index.get_symbol_by_qname(&ref_name, &qn)? {
                out.push((sym.symbol_id, sym.qname));
            }
        }
        out
    };

    let mut entries = Vec::new();
    for (sid, qname) in &symbol_ids {
        let les = ledger.list_entries(&ref_name, sid).unwrap_or_default();
        for entry in les {
            if entry.kind.conclusion_class() != ConclusionClass::Thinking {
                continue;
            }
            if let Some(filter) = kind_filter {
                if entry.kind != filter {
                    continue;
                }
            }
            entries.push(json!({
                "entry_id": entry.entry_id,
                "kind": entry.kind.as_str(),
                "qname": qname,
                "summary": entry.summary,
                "confidence": entry.confidence,
                "body": entry.body,
                "tags": entry.tags,
                "created_at": entry.created_at,
            }));
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "total": entries.len(),
            "kind_filter": kind_filter.map(|k| k.as_str()),
            "symbol_filter": args.symbol,
            "entries": entries,
        }))?
    );
    Ok(())
}
