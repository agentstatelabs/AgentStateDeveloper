//! `asd ledger …` — append ledger entries against a symbol.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore,
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum LedgerCmd {
    /// Append a new ledger entry for a symbol.
    Append(AppendArgs),
}

#[derive(Debug, Args)]
pub struct AppendArgs {
    /// Fully-qualified symbol name.
    pub qname: String,

    /// Ledger kind.
    #[arg(long, value_enum)]
    pub kind: CliLedgerKind,

    /// One-line summary.
    #[arg(long)]
    pub summary: String,

    /// Optional path to a body file (markdown or plain text).
    #[arg(long)]
    pub body: Option<PathBuf>,

    /// Author kind — `agent` or `human`.
    #[arg(long, value_enum, default_value_t = CliAuthorKind::Agent)]
    pub author_kind: CliAuthorKind,

    /// Author identifier (email, agent-slug, etc.).
    #[arg(long, default_value = "asd-cli-user")]
    pub author_id: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliLedgerKind {
    Decision,
    Assumption,
    Constraint,
    Rationale,
    Hazard,
    Tradeoff,
}

impl From<CliLedgerKind> for LedgerKind {
    fn from(k: CliLedgerKind) -> Self {
        match k {
            CliLedgerKind::Decision => LedgerKind::Decision,
            CliLedgerKind::Assumption => LedgerKind::Assumption,
            CliLedgerKind::Constraint => LedgerKind::Constraint,
            CliLedgerKind::Rationale => LedgerKind::Rationale,
            CliLedgerKind::Hazard => LedgerKind::Hazard,
            CliLedgerKind::Tradeoff => LedgerKind::Tradeoff,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliAuthorKind {
    Agent,
    Human,
}

impl From<CliAuthorKind> for AuthorKind {
    fn from(k: CliAuthorKind) -> Self {
        match k {
            CliAuthorKind::Agent => AuthorKind::Agent,
            CliAuthorKind::Human => AuthorKind::Human,
        }
    }
}

pub fn run(cfg: &Config, cmd: LedgerCmd) -> Result<()> {
    match cmd {
        LedgerCmd::Append(args) => append(cfg, args),
    }
}

fn append(cfg: &Config, args: AppendArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let index_store = AsgIndexStore { repo: &engine.repo };
    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let author = Author {
        kind: args.author_kind.into(),
        id: args.author_id,
    };

    let mut entry = LedgerEntry::new(&symbol.symbol_id, args.kind.into(), args.summary, author);
    if let Some(body_path) = args.body {
        let text = std::fs::read_to_string(&body_path)?;
        entry.body = Some(text);
    }

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    ledger_store.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;

    println!("{}", entry.entry_id);
    Ok(())
}
