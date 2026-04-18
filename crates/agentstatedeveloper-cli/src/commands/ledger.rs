//! `asd ledger …` — append ledger entries against a symbol.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use agentstatedeveloper_core::{
    actions, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Decision, Engine, IndexStore,
    LedgerEntry, LedgerKind, LedgerStore, Situation,
};
use serde_json::json;

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum LedgerCmd {
    /// Append a new ledger entry for a symbol.
    Append(AppendArgs),

    /// Approve an entry currently tagged `awaiting-approval`.
    Approve(ApproveArgs),

    /// Reject an entry currently tagged `awaiting-approval`.
    Reject(RejectArgs),

    /// Withdraw an awaiting-approval entry — must be called by the
    /// original author.
    Withdraw(WithdrawArgs),

    /// Write a new entry that supersedes one or more existing entries.
    /// Non-superseded entries remain; superseded ones are filtered out
    /// of the default `list_entries` view.
    Supersede(SupersedeArgs),
}

#[derive(Debug, Args)]
pub struct ApproveArgs {
    /// Entry id (e.g., `led_abc…`) to approve.
    pub entry_id: String,

    /// Approver identifier. Recorded as `approved-by:<id>`.
    #[arg(long)]
    pub approver: String,

    /// Approver kind. Must match one of the `approver:*` tags unless
    /// the id itself matches.
    #[arg(long, default_value = "human")]
    pub approver_kind: String,

    /// Optional rationale — appended to the entry body as an
    /// "Approver note" section.
    #[arg(long)]
    pub message: Option<String>,
}

#[derive(Debug, Args)]
pub struct RejectArgs {
    /// Entry id to reject.
    pub entry_id: String,

    /// Reviewer identifier. Recorded as `rejected-by:<id>`.
    #[arg(long)]
    pub reviewer: String,

    /// Reviewer kind. Same approver-match rule as approve.
    #[arg(long, default_value = "human")]
    pub reviewer_kind: String,

    /// Rejection reason (required). Appended to the entry body.
    #[arg(long)]
    pub reason: String,
}

#[derive(Debug, Args)]
pub struct WithdrawArgs {
    /// Entry id to withdraw.
    pub entry_id: String,

    /// Author id — must match `entry.author.id`.
    #[arg(long)]
    pub author_id: String,
}

#[derive(Debug, Args)]
pub struct SupersedeArgs {
    /// Qname the new entry attaches to.
    pub qname: String,

    /// One or more entry ids to supersede. All must belong to the same
    /// symbol as `qname`.
    #[arg(long = "supersede", required = true)]
    pub supersedes: Vec<String>,

    /// Ledger kind for the new entry.
    #[arg(long, value_enum)]
    pub kind: CliLedgerKind,

    /// One-line summary for the new entry.
    #[arg(long)]
    pub summary: String,

    /// Optional body file.
    #[arg(long)]
    pub body: Option<PathBuf>,

    /// Author kind.
    #[arg(long, value_enum, default_value_t = CliAuthorKind::Agent)]
    pub author_kind: CliAuthorKind,

    /// Author id.
    #[arg(long, default_value = "asd-cli-user")]
    pub author_id: String,
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
        LedgerCmd::Approve(args) => approve(cfg, args),
        LedgerCmd::Reject(args) => reject(cfg, args),
        LedgerCmd::Withdraw(args) => withdraw(cfg, args),
        LedgerCmd::Supersede(args) => supersede(cfg, args),
    }
}

fn approve(cfg: &Config, args: ApproveArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store.approve_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.approver,
        &args.approver_kind,
        args.message.as_deref(),
        &cfg.agent_id,
    )?;

    let out = json!({
        "status": if outcome.already_approved { "already-approved" } else { "approved" },
        "entry_id": outcome.entry.entry_id,
        "symbol_id": outcome.entry.symbol_id,
        "tags": outcome.entry.tags,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn reject(cfg: &Config, args: RejectArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store.reject_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.reviewer,
        &args.reviewer_kind,
        &args.reason,
        &cfg.agent_id,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": if outcome.already_resolved { "already-rejected" } else { "rejected" },
            "entry_id": outcome.entry.entry_id,
            "symbol_id": outcome.entry.symbol_id,
            "tags": outcome.entry.tags,
        }))?
    );
    Ok(())
}

fn withdraw(cfg: &Config, args: WithdrawArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let outcome = ledger_store.withdraw_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.author_id,
        &cfg.agent_id,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": if outcome.already_resolved { "already-withdrawn" } else { "withdrawn" },
            "entry_id": outcome.entry.entry_id,
            "symbol_id": outcome.entry.symbol_id,
            "tags": outcome.entry.tags,
        }))?
    );
    Ok(())
}

fn supersede(cfg: &Config, args: SupersedeArgs) -> Result<()> {
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
        entry.body = Some(std::fs::read_to_string(body_path)?);
    }
    entry.supersedes = args.supersedes;
    entry.tags.push("supersedes".to_string());

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    ledger_store.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "superseded",
            "entry_id": entry.entry_id,
            "symbol_id": entry.symbol_id,
            "supersedes": entry.supersedes,
        }))?
    );
    Ok(())
}

fn append(cfg: &Config, args: AppendArgs) -> Result<()> {
    let mut engine = Engine::open_sqlite(&cfg.db_path)?;
    if let Some(ref p) = cfg.policy_path {
        engine
            .load_policy_file(p)
            .map_err(|e| anyhow::anyhow!("failed to load policy file {}: {e}", p.display()))?;
    }

    let index_store = AsgIndexStore { repo: &engine.repo };
    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let ledger_kind: LedgerKind = args.kind.into();
    let action = actions::ledger_append_action(ledger_kind.as_str());
    let situation = Situation {
        description: format!("ledger.append for {}", args.qname),
        qualifiers: json!({ "qname": &args.qname, "kind": ledger_kind.as_str() }),
    };
    let decision = engine
        .policy
        .evaluate(&situation, &action, &args.author_id)?;

    match &decision {
        Decision::Deny {
            matched_policy,
            reason,
        } => {
            let err = json!({
                "status": "denied",
                "action": action,
                "matched_policy": matched_policy,
                "reason": reason,
            });
            println!("{}", serde_json::to_string_pretty(&err)?);
            return Err(anyhow::anyhow!("policy denied: {}", reason));
        }
        _ => {}
    }

    let author = Author {
        kind: args.author_kind.into(),
        id: args.author_id.clone(),
    };
    let mut entry = LedgerEntry::new(&symbol.symbol_id, ledger_kind, args.summary, author);
    if let Some(body_path) = args.body {
        let text = std::fs::read_to_string(&body_path)?;
        entry.body = Some(text);
    }
    entry.matched_policy = decision.matched_policy();

    // RequireApproval: tag the entry so downstream reviewers see it.
    if let Decision::RequireApproval {
        approvers, reason, ..
    } = &decision
    {
        entry.tags.push("awaiting-approval".to_string());
        for a in approvers {
            entry.tags.push(format!("approver:{}", a));
        }
        if let Some(r) = reason {
            if entry.body.is_none() {
                entry.body = Some(format!("Approval reason: {}", r));
            }
        }
    }

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    ledger_store.append_entry(&engine.ref_name, &entry, &cfg.agent_id)?;

    let out = json!({
        "entry_id": entry.entry_id,
        "matched_policy": entry.matched_policy,
        "status": match &decision {
            Decision::Allow { .. } => "allowed",
            Decision::RequireApproval { .. } => "awaiting-approval",
            Decision::Deny { .. } => "denied",
            Decision::NoPolicyMatch => "no-policy-match",
        },
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
