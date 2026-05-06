//! `asd scratch …` — ephemeral working notes with a promote-to-ledger path.
//!
//! Scratch entries are local-only (not synced via `asd sync`) and not
//! subject to the policy gate. Only `promote` goes through policy + audit
//! because it creates a real [`LedgerEntry`].
//!
//! ## Subcommands
//!
//! ```text
//! asd scratch write <content> [--symbol <qname>] [--workflow <name>] [--tag <tag>]... [--ttl 24h|7d]
//! asd scratch list  [--symbol <qname>] [--workflow <name>] [--session <id>] [--status draft|promoted|discarded] [--all] [--json]
//! asd scratch read  <scratch_id> [--json]
//! asd scratch update <scratch_id> <content>
//! asd scratch discard <scratch_id>
//! asd scratch promote <scratch_id> --kind <ledger-kind> [--summary <text>] [--symbol <qname>]
//! asd scratch clean  --older-than <7d> [--status discarded,promoted] [--dry-run]
//! ```

use anyhow::{bail, Result};
use clap::{Args, Subcommand};

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, AsgScratchStore, Author, AuthorKind, CleanFilter, Engine,
    IndexStore, LedgerEntry, LedgerKind, LedgerStore, ScratchEntry, ScratchFilter,
    ScratchStatus, ScratchStore,
};

use crate::commands::ledger::open_engine_public;
use crate::config::Config;

// ---------------------------------------------------------------------------
// Subcommand definitions
// ---------------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum ScratchCmd {
    /// Write a new draft scratch entry.
    Write(WriteArgs),

    /// List scratch entries (default: draft, non-expired).
    List(ListArgs),

    /// Read a single scratch entry by ID.
    Read(ReadArgs),

    /// Update the content of an existing draft entry.
    Update(UpdateArgs),

    /// Mark an entry as discarded (soft-delete; use `clean` to purge).
    Discard(DiscardArgs),

    /// Promote a draft entry to a durable ledger entry.
    Promote(PromoteArgs),

    /// Permanently delete entries matching the filter.
    Clean(CleanArgs),
}

#[derive(Debug, Args)]
pub struct WriteArgs {
    /// Content of the scratch note (markdown OK).
    pub content: String,

    /// Scope to a symbol by qualified name (resolved to symbol_id).
    #[arg(long)]
    pub symbol: Option<String>,

    /// Named investigation context (e.g. "tracing-sync-bug").
    #[arg(long)]
    pub workflow: Option<String>,

    /// One or more tags. Can be repeated: `--tag perf --tag regression`.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Time-to-live: `24h`, `7d`, `30d` etc. When not set, no expiry.
    #[arg(long)]
    pub ttl: Option<String>,

    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter by symbol qualified name.
    #[arg(long)]
    pub symbol: Option<String>,

    /// Filter by workflow name.
    #[arg(long)]
    pub workflow: Option<String>,

    /// Filter by session/agent_id.
    #[arg(long)]
    pub session: Option<String>,

    /// Filter by status: draft, promoted, discarded.
    #[arg(long)]
    pub status: Option<String>,

    /// Include all statuses and expired entries (overrides --status).
    #[arg(long)]
    pub all: bool,

    /// Emit JSON array instead of human-readable table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ReadArgs {
    /// Scratch entry ID (`scr_…`).
    pub scratch_id: String,

    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Scratch entry ID to update.
    pub scratch_id: String,

    /// Replacement content (replaces previous content entirely).
    pub content: String,
}

#[derive(Debug, Args)]
pub struct DiscardArgs {
    /// Scratch entry ID to discard.
    pub scratch_id: String,
}

#[derive(Debug, Args)]
pub struct PromoteArgs {
    /// Scratch entry ID to promote.
    pub scratch_id: String,

    /// Ledger entry kind: decision, assumption, constraint, rationale, hazard,
    /// tradeoff, invariant, ownership, proof.
    #[arg(long, short = 'k')]
    pub kind: String,

    /// One-line summary. Defaults to the first non-empty line of the scratch
    /// content when not supplied.
    #[arg(long, short = 's')]
    pub summary: Option<String>,

    /// Attach the ledger entry to this symbol (qualified name). When not set,
    /// the scratch entry's existing symbol_id is reused. At least one of
    /// --symbol or an existing symbol_id must be present.
    #[arg(long)]
    pub symbol: Option<String>,

    /// Author id to record on the ledger entry (default: ASD_AGENT_ID or "asd-cli").
    #[arg(long)]
    pub author: Option<String>,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Delete entries older than this duration (e.g. `7d`, `24h`, `30d`).
    #[arg(long)]
    pub older_than: String,

    /// Comma-separated list of statuses to clean: discarded, promoted, draft.
    /// Defaults to `discarded,promoted`.
    #[arg(long, default_value = "discarded,promoted")]
    pub status: String,

    /// Report what would be deleted without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn run(cfg: &Config, cmd: ScratchCmd) -> Result<()> {
    match cmd {
        ScratchCmd::Write(args) => write_cmd(cfg, args),
        ScratchCmd::List(args) => list_cmd(cfg, args),
        ScratchCmd::Read(args) => read_cmd(cfg, args),
        ScratchCmd::Update(args) => update_cmd(cfg, args),
        ScratchCmd::Discard(args) => discard_cmd(cfg, args),
        ScratchCmd::Promote(args) => promote_cmd(cfg, args),
        ScratchCmd::Clean(args) => clean_cmd(cfg, args),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_ttl(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    if let Some(days) = s.strip_suffix('d') {
        let n: i64 = days.parse().map_err(|_| anyhow::anyhow!("invalid TTL: {s}"))?;
        return Ok(chrono::Duration::days(n));
    }
    if let Some(hours) = s.strip_suffix('h') {
        let n: i64 = hours.parse().map_err(|_| anyhow::anyhow!("invalid TTL: {s}"))?;
        return Ok(chrono::Duration::hours(n));
    }
    bail!("TTL must be in the form `24h` or `7d`")
}

fn parse_statuses(s: &str) -> Vec<ScratchStatus> {
    s.split(',')
        .filter_map(|t| match t.trim() {
            "draft" => Some(ScratchStatus::Draft),
            "promoted" => Some(ScratchStatus::Promoted),
            "discarded" => Some(ScratchStatus::Discarded),
            _ => None,
        })
        .collect()
}

fn parse_kind(s: &str) -> Result<LedgerKind> {
    match s {
        "decision" => Ok(LedgerKind::Decision),
        "assumption" => Ok(LedgerKind::Assumption),
        "constraint" => Ok(LedgerKind::Constraint),
        "rationale" => Ok(LedgerKind::Rationale),
        "hazard" => Ok(LedgerKind::Hazard),
        "tradeoff" => Ok(LedgerKind::Tradeoff),
        "invariant" => Ok(LedgerKind::Invariant),
        "ownership" => Ok(LedgerKind::Ownership),
        "proof" => Ok(LedgerKind::Proof),
        other => bail!("unknown ledger kind: '{}'. Valid: decision, assumption, constraint, rationale, hazard, tradeoff, invariant, ownership, proof", other),
    }
}

/// Derive a first-line summary from scratch content as a fallback.
fn first_line(content: &str) -> String {
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(content)
        .chars()
        .take(140)
        .collect()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn write_cmd(cfg: &Config, args: WriteArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgScratchStore { repo: &engine.repo };

    let mut entry = ScratchEntry::new(&args.content, &cfg.agent_id);
    entry.tags = args.tags;

    if let Some(ref wf) = args.workflow {
        entry.workflow = Some(wf.clone());
    }

    if let Some(ref ttl) = args.ttl {
        let dur = parse_ttl(ttl)?;
        entry.expires_at = Some(chrono::Utc::now() + dur);
    }

    // Resolve --symbol to symbol_id.
    if let Some(ref qname) = args.symbol {
        let index = AsgIndexStore { repo: &engine.repo };
        let sym = index
            .get_symbol_by_qname(&engine.ref_name, qname)?
            .ok_or_else(|| anyhow::anyhow!("symbol not found: {qname}"))?;
        entry.symbol_id = Some(sym.symbol_id);
    }

    let stored = store.write_entry(&engine.ref_name, &entry, &cfg.agent_id)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&stored)?);
    } else {
        println!("scratch_id: {}", stored.scratch_id);
        println!("status:     draft");
        if let Some(ref wf) = stored.workflow {
            println!("workflow:   {wf}");
        }
        if let Some(ref sym) = stored.symbol_id {
            println!("symbol_id:  {sym}");
        }
    }
    Ok(())
}

fn list_cmd(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgScratchStore { repo: &engine.repo };

    let filter = if args.all {
        ScratchFilter::all()
    } else {
        let status = args
            .status
            .as_deref()
            .and_then(|s| match s {
                "draft" => Some(ScratchStatus::Draft),
                "promoted" => Some(ScratchStatus::Promoted),
                "discarded" => Some(ScratchStatus::Discarded),
                _ => None,
            })
            .or(Some(ScratchStatus::Draft)); // default: drafts only

        ScratchFilter {
            symbol_id: None, // we filter by qname below if provided
            workflow: args.workflow.clone(),
            session: args.session.clone(),
            status,
            exclude_expired: true,
        }
    };

    // Resolve --symbol qname → symbol_id for filtering.
    let sym_id_filter: Option<String> = if let Some(ref qname) = args.symbol {
        let index = AsgIndexStore { repo: &engine.repo };
        let sym = index
            .get_symbol_by_qname(&engine.ref_name, qname)?
            .ok_or_else(|| anyhow::anyhow!("symbol not found: {qname}"))?;
        Some(sym.symbol_id)
    } else {
        None
    };

    let mut filter = filter;
    filter.symbol_id = sym_id_filter;

    let entries = store.list_entries(&engine.ref_name, &filter)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No scratch entries found.");
        return Ok(());
    }

    println!(
        "{:<36}  {:<10}  {:<22}  {}",
        "scratch_id", "status", "workflow", "content (first 60 chars)"
    );
    println!("{}", "-".repeat(120));
    for e in &entries {
        let snippet: String = e.content.chars().take(60).collect();
        let snippet = snippet.replace('\n', " ");
        println!(
            "{:<36}  {:<10}  {:<22}  {}",
            e.scratch_id,
            format!("{:?}", e.status).to_lowercase(),
            e.workflow.as_deref().unwrap_or("—"),
            snippet,
        );
    }
    println!("\n{} entry/entries.", entries.len());
    Ok(())
}

fn read_cmd(cfg: &Config, args: ReadArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgScratchStore { repo: &engine.repo };
    let entry = store.read_entry(&engine.ref_name, &args.scratch_id)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
        return Ok(());
    }

    println!("scratch_id:  {}", entry.scratch_id);
    println!("status:      {:?}", entry.status);
    println!("session:     {}", entry.session);
    if let Some(ref wf) = entry.workflow {
        println!("workflow:    {wf}");
    }
    if let Some(ref sid) = entry.symbol_id {
        println!("symbol_id:  {sid}");
    }
    if let Some(ref pt) = entry.promoted_to {
        println!("promoted_to: {pt}");
    }
    if !entry.tags.is_empty() {
        println!("tags:        {}", entry.tags.join(", "));
    }
    println!("created_at:  {}", entry.created_at);
    println!("updated_at:  {}", entry.updated_at);
    if let Some(exp) = entry.expires_at {
        println!("expires_at:  {}", exp);
    }
    println!("\n{}", entry.content);
    Ok(())
}

fn update_cmd(cfg: &Config, args: UpdateArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgScratchStore { repo: &engine.repo };
    let updated = store.update_entry(
        &engine.ref_name,
        &args.scratch_id,
        &args.content,
        &cfg.agent_id,
    )?;
    println!("Updated {}", updated.scratch_id);
    Ok(())
}

fn discard_cmd(cfg: &Config, args: DiscardArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgScratchStore { repo: &engine.repo };
    store.discard_entry(&engine.ref_name, &args.scratch_id, &cfg.agent_id)?;
    println!("Discarded {}", args.scratch_id);
    Ok(())
}

fn promote_cmd(cfg: &Config, args: PromoteArgs) -> Result<()> {
    // Policy + audit wired through open_engine_public.
    let engine = open_engine_public(cfg)?;
    let scratch_store = AsgScratchStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };

    // 1. Read the scratch entry.
    let entry = scratch_store.read_entry(&engine.ref_name, &args.scratch_id)?;

    // 2. Resolve symbol_id: --symbol flag wins, then fall back to entry.symbol_id.
    let symbol_id = if let Some(ref qname) = args.symbol {
        let index = AsgIndexStore { repo: &engine.repo };
        let sym = index
            .get_symbol_by_qname(&engine.ref_name, qname)?
            .ok_or_else(|| anyhow::anyhow!("symbol not found: {qname}"))?;
        sym.symbol_id
    } else if let Some(ref sid) = entry.symbol_id {
        sid.clone()
    } else {
        bail!(
            "no symbol attached to scratch entry and --symbol was not provided. \
             Use --symbol <qname> to attach the ledger entry to a symbol."
        );
    };

    // 3. Build summary.
    let summary = args
        .summary
        .unwrap_or_else(|| first_line(&entry.content));

    // 4. Build LedgerEntry.
    let kind = parse_kind(&args.kind)?;
    let author_id = args
        .author
        .unwrap_or_else(|| cfg.agent_id.clone());
    let author = Author {
        kind: AuthorKind::Agent,
        id: author_id.clone(),
    };
    let mut ledger_entry = LedgerEntry::new(&symbol_id, kind, &summary, author);
    ledger_entry.body = Some(entry.content.clone());

    // 5. Append via engine (policy + audit).
    ledger_store.append_entry(&engine.ref_name, &ledger_entry, &author_id)?;

    // 6. Mark scratch as promoted.
    let promoted = scratch_store.mark_promoted(
        &engine.ref_name,
        &entry.scratch_id,
        &ledger_entry.entry_id,
        &author_id,
    )?;

    println!("scratch_id:  {}", promoted.scratch_id);
    println!("promoted_to: {}", ledger_entry.entry_id);
    println!("symbol_id:   {symbol_id}");
    Ok(())
}

fn clean_cmd(cfg: &Config, args: CleanArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgScratchStore { repo: &engine.repo };

    let dur = parse_ttl(&args.older_than)?;
    let statuses = parse_statuses(&args.status);

    let filter = CleanFilter {
        older_than: Some(dur),
        statuses,
    };

    let count = store.clean_entries(&engine.ref_name, &filter, args.dry_run)?;

    if args.dry_run {
        println!(
            "dry-run: {} entry/entries would be deleted. Run without --dry-run to apply.",
            count
        );
    } else {
        println!("Deleted {} entry/entries.", count);
    }
    Ok(())
}
