//! `asd ledger …` — append ledger entries against a symbol.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use agentstatedeveloper_core::{
    actions, event_types, AsgIndexStore, AsgLedgerStore, AuditEvent, AuditSink, Author, AuthorKind,
    Decision, Engine, IndexStore, LedgerEntry, LedgerKind, LedgerStore, Situation,
};
use serde_json::json;

use crate::config::Config;

/// Open the engine with optional policy + audit wiring based on cfg.
fn open_engine(cfg: &Config) -> anyhow::Result<Engine> {
    let mut engine = Engine::open_sqlite(&cfg.db_path)?;
    if let Some(ref p) = cfg.policy_path {
        engine
            .load_policy_file(p)
            .map_err(|e| anyhow::anyhow!("failed to load policy file {}: {e}", p.display()))?;
    }
    if let Some(ref p) = cfg.audit_log_path {
        engine
            .set_audit_log_file(p)
            .map_err(|e| anyhow::anyhow!("failed to open audit log {}: {e}", p.display()))?;
    }
    Ok(engine)
}

/// Emit an audit event, logging any sink failure to stderr but never
/// propagating — audit issues must never block the user's operation.
fn emit_audit(sink: &dyn AuditSink, event: AuditEvent) {
    if let Err(e) = sink.emit(&event) {
        eprintln!("warning: audit emit failed: {}", e);
    }
}

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
    let engine = open_engine(cfg)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let result = ledger_store.approve_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.approver,
        &args.approver_kind,
        args.message.as_deref(),
        &cfg.agent_id,
    );

    match result {
        Ok(outcome) => {
            let status = if outcome.already_approved {
                "already-approved"
            } else {
                "approved"
            };
            let event = AuditEvent::new(
                event_types::LEDGER_APPROVE,
                &args.approver,
                &args.approver_kind,
                status,
            )
            .with_subject(&outcome.entry.entry_id)
            .with_secondary(&outcome.entry.symbol_id)
            .with_matched_policy(outcome.entry.matched_policy.clone())
            .with_payload(json!({ "tags": outcome.entry.tags }));
            emit_audit(engine.audit.as_ref(), event);

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": status,
                    "entry_id": outcome.entry.entry_id,
                    "symbol_id": outcome.entry.symbol_id,
                    "tags": outcome.entry.tags,
                }))?
            );
            Ok(())
        }
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_APPROVE,
                &args.approver,
                &args.approver_kind,
                "error",
            )
            .with_subject(&args.entry_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(e.into())
        }
    }
}

fn reject(cfg: &Config, args: RejectArgs) -> Result<()> {
    let engine = open_engine(cfg)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let result = ledger_store.reject_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.reviewer,
        &args.reviewer_kind,
        &args.reason,
        &cfg.agent_id,
    );
    match result {
        Ok(outcome) => {
            let status = if outcome.already_resolved {
                "already-rejected"
            } else {
                "rejected"
            };
            let event = AuditEvent::new(
                event_types::LEDGER_REJECT,
                &args.reviewer,
                &args.reviewer_kind,
                status,
            )
            .with_subject(&outcome.entry.entry_id)
            .with_secondary(&outcome.entry.symbol_id)
            .with_matched_policy(outcome.entry.matched_policy.clone())
            .with_reason(&args.reason)
            .with_payload(json!({ "tags": outcome.entry.tags }));
            emit_audit(engine.audit.as_ref(), event);

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": status,
                    "entry_id": outcome.entry.entry_id,
                    "symbol_id": outcome.entry.symbol_id,
                    "tags": outcome.entry.tags,
                }))?
            );
            Ok(())
        }
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_REJECT,
                &args.reviewer,
                &args.reviewer_kind,
                "error",
            )
            .with_subject(&args.entry_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(e.into())
        }
    }
}

fn withdraw(cfg: &Config, args: WithdrawArgs) -> Result<()> {
    let engine = open_engine(cfg)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let result = ledger_store.withdraw_entry(
        &engine.ref_name,
        &args.entry_id,
        &args.author_id,
        &cfg.agent_id,
    );
    match result {
        Ok(outcome) => {
            let status = if outcome.already_resolved {
                "already-withdrawn"
            } else {
                "withdrawn"
            };
            let event = AuditEvent::new(
                event_types::LEDGER_WITHDRAW,
                &args.author_id,
                "agent",
                status,
            )
            .with_subject(&outcome.entry.entry_id)
            .with_secondary(&outcome.entry.symbol_id)
            .with_payload(json!({ "tags": outcome.entry.tags }));
            emit_audit(engine.audit.as_ref(), event);

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "status": status,
                    "entry_id": outcome.entry.entry_id,
                    "symbol_id": outcome.entry.symbol_id,
                    "tags": outcome.entry.tags,
                }))?
            );
            Ok(())
        }
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_WITHDRAW,
                &args.author_id,
                "agent",
                "error",
            )
            .with_subject(&args.entry_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(e.into())
        }
    }
}

fn supersede(cfg: &Config, args: SupersedeArgs) -> Result<()> {
    let engine = open_engine(cfg)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let author_kind_str: AuthorKind = args.author_kind.into();
    let author_kind_label = match author_kind_str {
        AuthorKind::Agent => "agent",
        AuthorKind::Human => "human",
    };
    let author = Author {
        kind: author_kind_str,
        id: args.author_id.clone(),
    };
    let mut entry = LedgerEntry::new(&symbol.symbol_id, args.kind.into(), args.summary, author);
    if let Some(body_path) = args.body {
        entry.body = Some(std::fs::read_to_string(body_path)?);
    }
    entry.supersedes = args.supersedes.clone();
    entry.tags.push("supersedes".to_string());

    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    match ledger_store.append_entry(&engine.ref_name, &entry, &cfg.agent_id) {
        Ok(()) => {
            let event = AuditEvent::new(
                event_types::LEDGER_SUPERSEDE,
                &args.author_id,
                author_kind_label,
                "success",
            )
            .with_subject(&entry.entry_id)
            .with_secondary(&entry.symbol_id)
            .with_payload(json!({
                "supersedes": entry.supersedes,
                "qname": args.qname,
            }));
            emit_audit(engine.audit.as_ref(), event);

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
        Err(e) => {
            let event = AuditEvent::new(
                event_types::LEDGER_SUPERSEDE,
                &args.author_id,
                author_kind_label,
                "error",
            )
            .with_secondary(&symbol.symbol_id)
            .with_reason(e.to_string());
            emit_audit(engine.audit.as_ref(), event);
            Err(e.into())
        }
    }
}

fn append(cfg: &Config, args: AppendArgs) -> Result<()> {
    let engine = open_engine(cfg)?;

    let index_store = AsgIndexStore { repo: &engine.repo };
    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let ledger_kind: LedgerKind = args.kind.into();
    let author_kind_str: AuthorKind = args.author_kind.into();
    let author_kind_label = match author_kind_str {
        AuthorKind::Agent => "agent",
        AuthorKind::Human => "human",
    };
    let action = actions::ledger_append_action(ledger_kind.as_str());
    let situation = Situation {
        description: format!("ledger.append for {}", args.qname),
        qualifiers: json!({ "qname": &args.qname, "kind": ledger_kind.as_str() }),
    };
    let decision = engine
        .policy
        .evaluate(&situation, &action, &args.author_id)?;

    if let Decision::Deny {
        matched_policy,
        reason,
    } = &decision
    {
        // Emit a denied audit event even though no entry is written.
        let event = AuditEvent::new(
            event_types::LEDGER_APPEND,
            &args.author_id,
            author_kind_label,
            "denied",
        )
        .with_secondary(&symbol.symbol_id)
        .with_matched_policy(Some(matched_policy.clone()))
        .with_reason(reason.clone())
        .with_payload(json!({ "qname": &args.qname, "kind": ledger_kind.as_str() }));
        emit_audit(engine.audit.as_ref(), event);

        let err = json!({
            "status": "denied",
            "action": action,
            "matched_policy": matched_policy,
            "reason": reason,
        });
        println!("{}", serde_json::to_string_pretty(&err)?);
        return Err(anyhow::anyhow!("policy denied: {}", reason));
    }

    let author = Author {
        kind: author_kind_str,
        id: args.author_id.clone(),
    };
    let mut entry = LedgerEntry::new(&symbol.symbol_id, ledger_kind, args.summary, author);
    if let Some(body_path) = args.body {
        let text = std::fs::read_to_string(&body_path)?;
        entry.body = Some(text);
    }
    entry.matched_policy = decision.matched_policy();

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

    let status = match &decision {
        Decision::Allow { .. } => "allowed",
        Decision::RequireApproval { .. } => "awaiting-approval",
        Decision::Deny { .. } => "denied",
        Decision::NoPolicyMatch => "no-policy-match",
    };
    let event = AuditEvent::new(
        event_types::LEDGER_APPEND,
        &args.author_id,
        author_kind_label,
        status,
    )
    .with_subject(&entry.entry_id)
    .with_secondary(&entry.symbol_id)
    .with_matched_policy(entry.matched_policy.clone())
    .with_payload(json!({
        "qname": &args.qname,
        "kind": ledger_kind.as_str(),
        "tags": &entry.tags,
    }));
    emit_audit(engine.audit.as_ref(), event);

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "entry_id": entry.entry_id,
            "matched_policy": entry.matched_policy,
            "status": status,
        }))?
    );
    Ok(())
}
