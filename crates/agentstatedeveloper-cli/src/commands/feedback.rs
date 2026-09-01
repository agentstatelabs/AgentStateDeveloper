//! `asd feedback` — record and list search-quality verdicts.
//!
//! Subcommands:
//!   mark    — attach a verdict (useful/noisy/missing/wrong_layer) to a (query, symbol) pair
//!   list    — display all recorded verdicts, optionally filtered to one symbol
//!   expire   — lapse a verdict so it stops influencing search ranking
//!   withdraw — retract a verdict that should not have been recorded
//!   purge    — hard-delete a verdict from both stores (escape hatch)

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use uuid::Uuid;

use agentstatedeveloper_core::{
    AsgFeedbackStore, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, FeedbackEntry,
    FeedbackStore, FeedbackVerdict, IndexStore, LedgerEntry, LedgerKind, LedgerStore, RoleTag,
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum FeedbackCmd {
    /// Record a verdict for a (query, symbol) result.
    Mark(MarkArgs),
    /// List recorded feedback verdicts.
    List(ListArgs),
    /// Lapse a verdict so it stops influencing search ranking.
    Expire(ExpireArgs),
    /// Retract a verdict that should never have been recorded.
    Withdraw(WithdrawArgs),
    /// Hard-delete a verdict from both stores. Prefer `withdraw`.
    Purge(PurgeArgs),
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
    /// Plan E t-009: when --verdict already_covered, the qname of the
    /// symbol whose behavior covers this one. Auto-writes a paired
    /// Mapping ledger entry (kind=mapping, body={from, to}) so the
    /// coverage link is durable, not just a per-query verdict.
    #[arg(long)]
    pub covered_by: Option<String>,

    /// Plan J t-014: optional verdict expiry in days from now. After
    /// `now + N days` the entry no longer influences ranking (still
    /// visible via `asd feedback list` / `export`). Useful for
    /// false-positive marks that should auto-decay — code shifts,
    /// what was wrong last quarter may be right next. Omit to keep
    /// the verdict permanent (current default).
    #[arg(long)]
    pub ttl_days: Option<i64>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter to a specific symbol qname. Omit to list all.
    pub qname: Option<String>,
    /// Emit JSON output.
    #[arg(long)]
    pub json: bool,
}

/// Selectors are mutually exclusive and one is required — expiring
/// everything because a flag was forgotten is not a recoverable mistake.
#[derive(Debug, Args)]
pub struct ExpireArgs {
    /// Entry id to expire (from `asd feedback list`).
    pub entry_id: Option<String>,

    /// Expire every verdict recorded against this symbol qname.
    #[arg(long, conflicts_with = "entry_id")]
    pub symbol: Option<String>,

    /// Expire every verdict recorded for this query (matched as the stored
    /// normalized form: lowercased and trimmed).
    #[arg(long, conflicts_with_all = ["entry_id", "symbol"])]
    pub query: Option<String>,

    /// Show what would be expired without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct WithdrawArgs {
    /// Entry id to withdraw (from `asd feedback list`).
    pub entry_id: String,

    /// Who is retracting it. Recorded on the entry.
    #[arg(long, default_value = "asd-cli")]
    pub by: String,

    /// Why. Free text, stored with the withdrawal.
    #[arg(long)]
    pub reason: Option<String>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {
    /// Entry id to delete permanently (from `asd feedback list`).
    pub entry_id: String,

    /// Required. Without it the command explains and refuses.
    #[arg(long)]
    pub yes: bool,

    /// Emit machine-readable JSON.
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
        FeedbackCmd::Expire(args) => run_expire(cfg, args),
        FeedbackCmd::Withdraw(args) => run_withdraw(cfg, args),
        FeedbackCmd::Purge(args) => run_purge(cfg, args),
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
        (
            format!("__file_scope__{}", Uuid::new_v4().simple()),
            glob.clone(),
        )
    } else {
        let index_store = AsgIndexStore::from_engine(&engine);
        let symbol = match index_store.get_symbol_by_qname(&engine.ref_name, &args.qname)? {
            Some(s) => s,
            None => bail!("symbol not found: {}", args.qname),
        };
        (symbol.symbol_id, args.qname.clone())
    };
    let now = chrono::Utc::now();
    let expires_at = args.ttl_days.map(|days| now + chrono::Duration::days(days));
    let entry = FeedbackEntry {
        entry_id: format!("fb_{}", Uuid::new_v4().simple()),
        symbol_id,
        symbol_qname: symbol_qname.clone(),
        query: args.query.to_lowercase().trim().to_string(),
        verdict,
        note: args.note.clone(),
        author: args.author.clone(),
        created_at: now,
        file_scope: args.file_scope.clone(),
        expires_at,
        // A freshly recorded verdict is never withdrawn.
        withdrawn_at: None,
        withdrawn_by: None,
        withdrawn_reason: None,
    };
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    feedback_store.record(&engine.ref_name, &entry, &args.author)?;

    // Plan E t-009: auto-write paired ledger entries that make the
    // verdict's intent durable. AlreadyCovered + --covered-by → a
    // Mapping entry; DiagnosticOnly → a Classification (Ownership with
    // role=diagnostic-test). Skipped for file-scope verdicts (no
    // concrete symbol to anchor the ledger entry on).
    let mut paired_msg = String::new();
    if args.file_scope.is_none() {
        let author_kind = if args.author == "asd-cli" {
            AuthorKind::Human
        } else {
            AuthorKind::Agent
        };
        let author_struct = Author {
            kind: author_kind,
            id: args.author.clone(),
        };
        let ledger_store = AsgLedgerStore::from_engine(&engine);

        if matches!(verdict, FeedbackVerdict::AlreadyCovered) {
            let cover = args.covered_by.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--covered-by <qname> is required when --verdict already_covered")
            })?;
            let body = serde_json::json!({
                "from_qname": &args.qname,
                "to_qname": cover,
                "source": "feedback-pair",
            })
            .to_string();
            let mut led = LedgerEntry::new(
                &entry.symbol_id,
                LedgerKind::Mapping,
                format!("covered by {cover}"),
                author_struct.clone(),
            );
            led.body = Some(body);
            led.tags.push("plan-e:t-009".into());
            ledger_store.append_entry(&engine.ref_name, &led, &args.author)?;
            paired_msg = format!(" + Mapping ledger entry → {cover}");
        } else if matches!(verdict, FeedbackVerdict::DiagnosticOnly) {
            let mut led = LedgerEntry::new(
                &entry.symbol_id,
                LedgerKind::Ownership,
                format!("diagnostic-only: {}", args.query),
                author_struct.clone(),
            );
            led.role = Some(RoleTag::DiagnosticTest.as_str().to_string());
            led.tags.push("plan-e:t-009".into());
            ledger_store.append_entry(&engine.ref_name, &led, &args.author)?;
            paired_msg = " + Classification ledger entry (role=diagnostic-test)".to_string();
        }
    }

    if args.file_scope.is_some() {
        println!(
            "recorded {} for files matching {:?} ({})",
            args.verdict, symbol_qname, entry.entry_id
        );
    } else {
        println!(
            "recorded {} for {} ({}){}",
            args.verdict, args.qname, entry.entry_id, paired_msg
        );
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
    let author_kind = if args.author == "asd-cli" {
        AuthorKind::Human
    } else {
        AuthorKind::Agent
    };
    let mut entry = LedgerEntry::new(
        &symbol.symbol_id,
        LedgerKind::Ownership,
        &args.concept,
        Author {
            kind: author_kind,
            id: args.author.clone(),
        },
    );
    entry.tags = vec!["promote-as-truth".to_string()];
    let ledger_store = AsgLedgerStore::from_engine(&engine);
    ledger_store.append_entry(&engine.ref_name, &entry, &args.author)?;
    println!(
        "promoted {} as source-of-truth for \"{}\" ({})",
        args.qname, args.concept, entry.entry_id
    );
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
    let useful = entries
        .iter()
        .filter(|e| matches!(e.verdict, FeedbackVerdict::Useful))
        .count();
    let noisy = entries
        .iter()
        .filter(|e| matches!(e.verdict, FeedbackVerdict::Noisy))
        .count();
    let missing = entries
        .iter()
        .filter(|e| matches!(e.verdict, FeedbackVerdict::Missing))
        .count();
    let wl = entries
        .iter()
        .filter(|e| matches!(e.verdict, FeedbackVerdict::WrongLayer))
        .count();
    (useful, noisy, missing, wl)
}

fn run_export(cfg: &Config, args: ExportArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    let entries = feedback_store.list_all(&engine.ref_name)?;

    if args.summary {
        let (useful, noisy, missing, wl) = verdict_breakdown(&entries);
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "total": entries.len(),
                "by_verdict": {
                    "useful": useful,
                    "noisy": noisy,
                    "missing": missing,
                    "wrong_layer": wl,
                },
                "db": cfg.db_path.display().to_string(),
            }))?
        );
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&entries)?;
    let (useful, noisy, missing, wl) = verdict_breakdown(&entries);
    match args.output {
        Some(ref path) => {
            std::fs::write(path, &json)?;
            eprintln!(
                "asd: exported {} feedback entries to {} (useful={}, noisy={}, missing={}, wrong_layer={})",
                entries.len(),
                path,
                useful,
                noisy,
                missing,
                wl
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
        let u = to_import
            .iter()
            .filter(|e| matches!(e.verdict, FeedbackVerdict::Useful))
            .count();
        let n = to_import
            .iter()
            .filter(|e| matches!(e.verdict, FeedbackVerdict::Noisy))
            .count();
        let m = to_import
            .iter()
            .filter(|e| matches!(e.verdict, FeedbackVerdict::Missing))
            .count();
        let w = to_import
            .iter()
            .filter(|e| matches!(e.verdict, FeedbackVerdict::WrongLayer))
            .count();
        (u, n, m, w)
    };

    if args.dry_run {
        eprintln!(
            "asd: [dry-run] would import {} entries, skip {} duplicates (useful={}, noisy={}, missing={}, wrong_layer={})",
            to_import.len(),
            skipped,
            useful,
            noisy,
            missing,
            wl
        );
        return Ok(());
    }

    for entry in &to_import {
        feedback_store.record(&engine.ref_name, entry, &args.author)?;
    }
    eprintln!(
        "asd: imported {} entries, skipped {} duplicates (useful={}, noisy={}, missing={}, wrong_layer={})",
        to_import.len(),
        skipped,
        useful,
        noisy,
        missing,
        wl
    );
    Ok(())
}

/// `asd feedback expire` — lapse a verdict so search ranking stops seeing it.
///
/// Feedback is not inert metadata: `apply_feedback_adjustments` folds these
/// verdicts into ranking, so a mistaken `noisy` suppresses a good symbol on
/// every future query. Until this existed there was no way to take one back —
/// feedback was the only write-only surface in ASD, while ledger has
/// withdraw/supersede and scratch has discard.
///
/// Implemented as a re-record with `expires_at = now` rather than a delete.
/// That reuses the expiry field and the lapsed-entry filtering Plan J t-014
/// already added, and — the part that matters — it goes through
/// `FeedbackStore::record`, which writes the authoritative ASG tree AND the
/// `asd_feedback` SQLite cache that `list_all` reads first. A bespoke delete
/// touching only one of those leaves the entry live in the other; that trap
/// is what motivated this command.
///
/// The entry stays listed, marked expired. A retracted verdict still explains
/// why a past search ranked the way it did, so hiding it would make old
/// results inexplicable.
fn run_expire(cfg: &Config, args: ExpireArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let feedback_store = AsgFeedbackStore::from_engine(&engine);
    let now = chrono::Utc::now();

    let all = feedback_store.list_all(&engine.ref_name)?;

    // Match the stored normalization so `--query "Drift Playhead"` finds the
    // entry recorded as `drift playhead`.
    let wanted_query = args
        .query
        .as_ref()
        .map(|q| q.to_lowercase().trim().to_string());

    let selected: Vec<&FeedbackEntry> = match (&args.entry_id, &args.symbol, &wanted_query) {
        (Some(id), _, _) => all.iter().filter(|e| &e.entry_id == id).collect(),
        (_, Some(qname), _) => all.iter().filter(|e| &e.symbol_qname == qname).collect(),
        (_, _, Some(q)) => all.iter().filter(|e| &e.query == q).collect(),
        _ => bail!("give an entry id, or one of --symbol <qname> / --query <text>"),
    };

    if selected.is_empty() {
        bail!(
            "no feedback matched; `asd feedback list` shows {} recorded entr{}",
            all.len(),
            if all.len() == 1 { "y" } else { "ies" }
        );
    }

    // Idempotent: re-expiring an already-lapsed entry would rewrite its
    // expires_at to a later timestamp, which is the opposite of what the
    // caller wants.
    let (already, to_expire): (Vec<_>, Vec<_>) = selected
        .into_iter()
        .partition(|e| e.expires_at.is_some_and(|t| t <= now));

    if args.dry_run {
        let out = serde_json::json!({
            "dry_run": true,
            "would_expire": to_expire.iter().map(|e| summarize(e)).collect::<Vec<_>>(),
            "already_expired": already.iter().map(|e| summarize(e)).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let mut expired = Vec::new();
    for entry in &to_expire {
        let mut lapsed = (*entry).clone();
        lapsed.expires_at = Some(now);
        feedback_store.record(&engine.ref_name, &lapsed, &lapsed.author)?;
        expired.push(summarize(&lapsed));
    }

    if args.json {
        let out = serde_json::json!({
            "expired": expired,
            "already_expired": already.iter().map(|e| summarize(e)).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    for e in &to_expire {
        println!(
            "expired {}  {:?}  {}  query={:?}",
            e.entry_id, e.verdict, e.symbol_qname, e.query
        );
    }
    for e in &already {
        println!("already expired {}  ({})", e.entry_id, e.symbol_qname);
    }
    if !expired.is_empty() {
        println!(
            "\n{} verdict{} will no longer influence ranking. They remain in \
             `asd feedback list`, marked expired.",
            expired.len(),
            if expired.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Compact identity for an entry in command output.
fn summarize(e: &FeedbackEntry) -> serde_json::Value {
    serde_json::json!({
        "entry_id": e.entry_id,
        "symbol_qname": e.symbol_qname,
        "query": e.query,
        "verdict": format!("{:?}", e.verdict),
        "expires_at": e.expires_at,
    })
}

/// `asd feedback withdraw` — retract a verdict that should not have been
/// recorded.
///
/// Distinct from `expire` on purpose. Expiry says *this was right, it is no
/// longer relevant*; withdrawal says *this was wrong*. Both stop the verdict
/// influencing ranking, but only withdrawal records who retracted it and why,
/// and only withdrawal is not revivable by future-dating an expiry. See
/// DESIGN.md, "Feedback withdrawal — tombstone shape".
///
/// The entry stays in `asd feedback list`, marked withdrawn — it still
/// explains why a past search ranked as it did.
fn run_withdraw(cfg: &Config, args: WithdrawArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgFeedbackStore::from_engine(&engine);

    let Some(entry) = store.withdraw(
        &engine.ref_name,
        &args.entry_id,
        &args.by,
        args.reason.as_deref(),
    )?
    else {
        let n = store.list_all(&engine.ref_name)?.len();
        bail!(
            "no feedback entry {}; `asd feedback list` shows {} recorded entr{}",
            args.entry_id,
            n,
            if n == 1 { "y" } else { "ies" }
        );
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
        return Ok(());
    }
    println!(
        "withdrawn {}  {:?}  {}  query={:?}",
        entry.entry_id, entry.verdict, entry.symbol_qname, entry.query
    );
    if let Some(ref r) = entry.withdrawn_reason {
        println!("    reason: {r}");
    }
    println!(
        "\nIt no longer influences ranking. It remains in `asd feedback list`, \
         marked withdrawn — it still explains a past ranking."
    );
    Ok(())
}

/// `asd feedback purge` — permanently delete a verdict from both stores.
///
/// The escape hatch a tombstone cannot serve: test data written by mistake, or
/// a secret pasted into a `--note`. For anything else `withdraw` is correct —
/// it retracts the verdict while keeping the record, which still explains why
/// a past search ranked as it did.
///
/// This rewrites history in a store that is otherwise append-only, so it is
/// gated behind `--yes` and says what it is about to destroy first.
fn run_purge(cfg: &Config, args: PurgeArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let store = AsgFeedbackStore::from_engine(&engine);

    // Show the entry before destroying it — an id alone is not enough for
    // someone to confirm they are deleting what they think they are.
    let Some(target) = store
        .list_all(&engine.ref_name)?
        .into_iter()
        .find(|e| e.entry_id == args.entry_id)
    else {
        bail!("no feedback entry {}", args.entry_id);
    };

    if !args.yes {
        eprintln!(
            "about to PERMANENTLY delete:\n  \
             {}  {:?}  {}  query={:?}",
            target.entry_id, target.verdict, target.symbol_qname, target.query
        );
        if let Some(ref n) = target.note {
            eprintln!("  note: {n}");
        }
        bail!(
            "refusing without --yes.\n\
             This rewrites history in an append-only store. If the verdict was \
             merely wrong, `asd feedback withdraw {}` retracts it while keeping \
             the record — almost always what you want. Purge is for data that \
             must not exist: test entries, or a secret pasted into a note.",
            args.entry_id
        );
    }

    let purged = store
        .purge(&engine.ref_name, &args.entry_id)?
        .ok_or_else(|| anyhow::anyhow!("no feedback entry {}", args.entry_id))?;

    if args.json {
        println!("{}", serde_json::json!({ "purged": true, "entry": purged }));
        return Ok(());
    }
    println!(
        "purged {}  {:?}  {}  query={:?}",
        purged.entry_id, purged.verdict, purged.symbol_qname, purged.query
    );
    println!("Gone from both the store and the search cache.");
    Ok(())
}
