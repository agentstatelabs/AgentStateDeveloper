//! `asd ledger …` — append ledger entries against a symbol.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};

use agentstatedeveloper_core::{
    actions, emit_audit, event_types, paths, AsgIndexStore, AsgLedgerStore, AuditEvent, Author,
    AuthorKind, Decision, Engine, IndexStore, LedgerEntry, LedgerKind, LedgerStore, Rebind,
    Situation,
};

use serde_json::json;

use crate::config::Config;

/// Open the engine with optional policy + audit wiring based on cfg.
/// If the process has installed a sink override (see
/// `crate::set_audit_sink_override`), that sink is used. Otherwise
/// events are swallowed by the default `NullSink`; when a log path
/// was configured we surface a warning so the OSS user knows their
/// `--audit-log` was a no-op.
fn open_engine(cfg: &Config) -> anyhow::Result<Engine> {
    let mut engine = Engine::open_sqlite(&cfg.db_path)?;
    if let Some(ref p) = cfg.policy_path {
        engine
            .load_policy_file(p)
            .map_err(|e| anyhow::anyhow!("failed to load policy file {}: {e}", p.display()))?;
    }
    if let Some(sink) = crate::audit_sink_override() {
        engine.set_audit_sink(sink);
    } else if cfg.audit_log_path.is_some() {
        eprintln!(
            "warning: audit log path configured but tamper-evident \
             logging is a commercial feature — install asd-pro \
             (Enterprise tier) to enable. Running with in-memory \
             NullSink."
        );
    }
    if let Some(ratify) = crate::ratify_ops_override() {
        engine.set_ratify_ops(ratify);
    }
    Ok(engine)
}

/// Public variant of [`open_engine`] for downstream crates that want
/// to reuse the same policy + audit wiring. Used by `asd-pro`'s
/// commercial subcommand handlers.
pub fn open_engine_public(cfg: &Config) -> anyhow::Result<Engine> {
    open_engine(cfg)
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

    /// Record that a symbol was renamed/moved so its ledger history follows.
    /// Writes a rebind record and re-parents all ledger entries to the new
    /// symbol_id. Use this when you rename a function/class with an ASD-aware
    /// tool so context doesn't become orphaned.
    Rebind(RebindArgs),
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
pub struct RebindArgs {
    /// The old symbol_id (e.g., `sym_abc123…`) whose history should follow the rename.
    #[arg(long)]
    pub from: String,

    /// The new qualified name the symbol was renamed to. Must already exist in the index.
    #[arg(long)]
    pub to: String,

    /// Author/agent performing the rebind.
    #[arg(long, default_value = "asd-cli-user")]
    pub agent_id: String,
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

    /// Plan B t-002: classification role/intent tag (e.g. "diagnostic-test",
    /// "fast-test", "fixture-path", "stale-api"). Optional; most meaningful
    /// for kind=ownership/concept entries.
    #[arg(long)]
    pub role: Option<String>,

    /// Plan B t-002: canonical reproduction or validation command
    /// (e.g. "swift test --filter SongPlayersTests"). Optional; most
    /// meaningful for kind=validation_scenario/proof/follow_up entries.
    #[arg(long)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliLedgerKind {
    Decision,
    Assumption,
    Constraint,
    Rationale,
    Hazard,
    Tradeoff,
    /// Invariant that must always hold at this symbol.
    Invariant,
    /// Ownership: which subsystem/team owns this symbol.
    Ownership,
    /// Proof that an invariant holds (test, review, trace).
    Proof,
    /// Concrete scenario to validate (behaviour + expected outcome).
    #[value(name = "validation_scenario")]
    ValidationScenario,
    /// Known bug or defect not yet fixed.
    #[value(name = "known_bug")]
    KnownBug,
    /// Domain concept (e.g. "Drift Pad clip playhead") — first-class queryable entity.
    Concept,
    /// Plan B t-002: replacement-coverage mapping ("legacy X is covered by new Y").
    Mapping,
    /// Plan B t-002: open follow-up tied to an external task.
    #[value(name = "follow_up")]
    FollowUp,
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
            CliLedgerKind::Invariant => LedgerKind::Invariant,
            CliLedgerKind::Ownership => LedgerKind::Ownership,
            CliLedgerKind::Proof => LedgerKind::Proof,
            CliLedgerKind::ValidationScenario => LedgerKind::ValidationScenario,
            CliLedgerKind::KnownBug => LedgerKind::KnownBug,
            CliLedgerKind::Concept => LedgerKind::Concept,
            CliLedgerKind::Mapping => LedgerKind::Mapping,
            CliLedgerKind::FollowUp => LedgerKind::FollowUp,
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
        LedgerCmd::Rebind(args) => rebind(cfg, args),
    }
}

fn approve(cfg: &Config, args: ApproveArgs) -> Result<()> {
    let engine = open_engine(cfg)?;
    let situation = Situation {
        description: format!("ledger.approve {}", args.entry_id),
        qualifiers: json!({ "entry_id": &args.entry_id }),
    };
    if let Decision::Deny { matched_policy, reason } =
        engine.policy.evaluate(&situation, actions::LEDGER_APPROVE, &args.approver)?
    {
        anyhow::bail!("policy denied: {} (matched {})", reason, matched_policy);
    }
    let result = if let Some(ref ratify) = engine.ratify {
        ratify.approve_entry(
            &engine.repo,
            &engine.ref_name,
            &args.entry_id,
            &args.approver,
            &args.approver_kind,
            args.message.as_deref(),
            &cfg.agent_id,
        )
    } else {
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        ledger_store.approve_entry(
            &engine.ref_name,
            &args.entry_id,
            &args.approver,
            &args.approver_kind,
            args.message.as_deref(),
            &cfg.agent_id,
        )
    };

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
    let situation = Situation {
        description: format!("ledger.reject {}", args.entry_id),
        qualifiers: json!({ "entry_id": &args.entry_id }),
    };
    if let Decision::Deny { matched_policy, reason } =
        engine.policy.evaluate(&situation, actions::LEDGER_REJECT, &args.reviewer)?
    {
        anyhow::bail!("policy denied: {} (matched {})", reason, matched_policy);
    }
    let result = if let Some(ref ratify) = engine.ratify {
        ratify.reject_entry(
            &engine.repo,
            &engine.ref_name,
            &args.entry_id,
            &args.reviewer,
            &args.reviewer_kind,
            &args.reason,
            &cfg.agent_id,
        )
    } else {
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        ledger_store.reject_entry(
            &engine.ref_name,
            &args.entry_id,
            &args.reviewer,
            &args.reviewer_kind,
            &args.reason,
            &cfg.agent_id,
        )
    };
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
    let situation = Situation {
        description: format!("ledger.withdraw {}", args.entry_id),
        qualifiers: json!({ "entry_id": &args.entry_id }),
    };
    if let Decision::Deny { matched_policy, reason } =
        engine.policy.evaluate(&situation, actions::LEDGER_WITHDRAW, &args.author_id)?
    {
        anyhow::bail!("policy denied: {} (matched {})", reason, matched_policy);
    }
    let result = if let Some(ref ratify) = engine.ratify {
        ratify.withdraw_entry(
            &engine.repo,
            &engine.ref_name,
            &args.entry_id,
            &args.author_id,
            &cfg.agent_id,
        )
    } else {
        let ledger_store = AsgLedgerStore::from_engine(&engine);
        ledger_store.withdraw_entry(
            &engine.ref_name,
            &args.entry_id,
            &args.author_id,
            &cfg.agent_id,
        )
    };
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
    let index_store = AsgIndexStore::from_engine(&engine);
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
    let situation = Situation {
        description: format!("ledger.supersede for {}", args.qname),
        qualifiers: json!({
            "qname": &args.qname,
            "symbol_id": &symbol.symbol_id,
            "file": &symbol.file,
            "language": &symbol.language,
        }),
    };
    if let Decision::Deny { matched_policy, reason } =
        engine.policy.evaluate(&situation, actions::LEDGER_SUPERSEDE, &args.author_id)?
    {
        anyhow::bail!("policy denied: {} (matched {})", reason, matched_policy);
    }

    let mut entry = LedgerEntry::new(&symbol.symbol_id, args.kind.into(), args.summary, author);
    if let Some(body_path) = args.body {
        entry.body = Some(std::fs::read_to_string(body_path)?);
    }
    entry.supersedes = args.supersedes.clone();
    entry.tags.push("supersedes".to_string());

    let ledger_store = AsgLedgerStore::from_engine(&engine);
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

    let index_store = AsgIndexStore::from_engine(&engine);
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
        qualifiers: json!({
            "qname": &args.qname,
            "kind": ledger_kind.as_str(),
            "symbol_id": &symbol.symbol_id,
            "file": &symbol.file,
            "language": &symbol.language,
        }),
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
    // Plan C t-002: validate role against the canonical RoleTag set;
    // emit a stderr warning on unknown so unknown tags don't break old
    // data but the user notices the typo.
    if let Some(ref r) = args.role {
        if agentstatedeveloper_core::RoleTag::from_str(r).is_none() {
            let valid: Vec<&str> = agentstatedeveloper_core::RoleTag::all()
                .iter()
                .map(|t| t.as_str())
                .collect();
            eprintln!(
                "asd: warning: role={:?} is not a canonical RoleTag. Valid: {}",
                r,
                valid.join(", ")
            );
        }
    }
    entry.role = args.role;
    entry.command = args.command;

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

    // t-001: inject CTX plan/task provenance from env vars.
    for tag in ctx_provenance_tags() {
        if !entry.tags.contains(&tag) {
            entry.tags.push(tag);
        }
    }

    let ledger_store = AsgLedgerStore::from_engine(&engine);
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

fn rebind(cfg: &Config, args: RebindArgs) -> Result<()> {
    use agentstategraph::CommitOptions;
    use agentstategraph_core::IntentCategory;
    use chrono::Utc;

    let engine = open_engine(cfg)?;

    // Policy gate — must pass before any writes.
    let situation = Situation::new("rebind symbol")
        .with_qualifier("from_symbol_id", &args.from)
        .with_qualifier("to_qname", &args.to);
    match engine.policy.evaluate(&situation, actions::LEDGER_REBIND, &args.agent_id)? {
        Decision::Deny { matched_policy, reason } => {
            anyhow::bail!("policy denied by {matched_policy}: {reason}");
        }
        _ => {}
    }

    // Resolve qnames → symbol_ids.
    let index_store = AsgIndexStore::from_engine(&engine);
    let from_symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.from)?
        .ok_or_else(|| anyhow::anyhow!("from qname not found in index: {} — run `asd index` first", args.from))?;
    let new_symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.to)?
        .ok_or_else(|| anyhow::anyhow!("qname not found in index: {} — run `asd index` first", args.to))?;

    if new_symbol.symbol_id == from_symbol.symbol_id {
        anyhow::bail!("from and to resolve to the same symbol_id — nothing to rebind");
    }

    // Write the rebind record.
    let rebind = Rebind {
        from_symbol_id: from_symbol.symbol_id.clone(),
        to_symbol_id: new_symbol.symbol_id.clone(),
        to_qname: args.to.clone(),
        at: Utc::now(),
        by: args.agent_id.clone(),
    };
    let rebind_path = paths::rebind_path(&from_symbol.symbol_id);
    let opts = CommitOptions::new(
        &args.agent_id,
        IntentCategory::Refine,
        format!("rebind {} → {}", args.from, args.to),
    );
    engine
        .repo
        .set_json(&engine.ref_name, &rebind_path, &serde_json::to_value(&rebind)?, opts)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Re-parent all ledger entries from old symbol_id to new symbol_id.
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    let entries = ledger_store.list_entries_with_superseded(&engine.ref_name, &from_symbol.symbol_id)?;
    let count = entries.len();
    for mut entry in entries {
        // Write under the new symbol_id path.
        entry.symbol_id = new_symbol.symbol_id.clone();
        let new_path = paths::ledger_entry_path(&new_symbol.symbol_id, &entry.entry_id);
        let opts = CommitOptions::new(
            &args.agent_id,
            IntentCategory::Refine,
            format!("rebind entry {} to {}", entry.entry_id, args.to),
        );
        engine
            .repo
            .set_json(&engine.ref_name, &new_path, &serde_json::to_value(&entry)?, opts)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    let event = AuditEvent::new(event_types::LEDGER_REBIND, &args.agent_id, "agent", "allow")
        .with_subject(from_symbol.symbol_id.clone())
        .with_secondary(new_symbol.symbol_id.clone())
        .with_payload(json!({
            "from_qname": args.from,
            "from_symbol_id": from_symbol.symbol_id,
            "to_symbol_id": new_symbol.symbol_id,
            "to_qname": args.to,
            "entries_moved": count,
        }));
    emit_audit(engine.audit.as_ref(), event);

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "rebound",
            "from_symbol_id": from_symbol.symbol_id,
            "to_symbol_id": new_symbol.symbol_id,
            "to_qname": args.to,
            "entries_moved": count,
        }))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// t-001: CTX plan/task provenance from env vars
// ---------------------------------------------------------------------------

/// Read CTXONE_PLAN and CTXONE_TASK env vars and return them as ledger tags.
/// Call this before every ledger append to attach workflow provenance.
pub fn ctx_provenance_tags() -> Vec<String> {
    let mut tags = Vec::new();
    if let Ok(plan) = std::env::var("CTXONE_PLAN") {
        if !plan.is_empty() {
            tags.push(format!("ctx:plan:{}", plan));
        }
    }
    if let Ok(task) = std::env::var("CTXONE_TASK") {
        if !task.is_empty() {
            tags.push(format!("ctx:task:{}", task));
        }
    }
    tags
}
