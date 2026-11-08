//! `asd gc` — preview, and on request perform, a garbage-collection sweep of
//! the ASG store behind this repository.
//!
//! Every `asd index` commits a fresh state tree. Content addressing shares the
//! unchanged parts, but the interior nodes on each changed path are new, so the
//! store grows with every run. Until AgentStateGraph v1.2.2 each of those
//! checkpoints also pinned its snapshot as a milestone, which kept nearly all of
//! it reachable: measured on one store, 7.9 GB whose live state was 3.4% of its
//! objects. `asd gc` is how that space comes back.
//!
//! **By default it only previews.** Nothing is deleted without `--sweep`, and
//! the preview reports what a sweep under the same policy would keep and
//! reclaim, less any objects written within the grace period (`--grace-secs`),
//! which a sweep spares. When mutating, the steps run in this order:
//!
//! 1. Distil any not-yet-extracted history. A sweep refuses while any commit is
//!    undistilled, so its signal is never lost with its state.
//! 2. `--unpin-legacy`: stop pinning milestones distilled before v1.2.2. One
//!    time, deletes nothing; without it a sweep of an older store reclaims little.
//! 3. `--sweep`: delete every object outside the retention policy's keep-set.
//!    AgentStateGraph holds the store's write lock from computing that keep-set
//!    to the last delete, so another ASD process committing meanwhile — a git
//!    hook, `asd-serve`, an MCP server — waits or fails rather than losing data.
//!    Objects written within the last `--grace-secs` (10 minutes by default)
//!    are spared too, so a write whose objects are stored but not yet committed
//!    survives (AgentStateGraph v1.3.0 and later).
//! 4. `--vacuum`: shrink the file. A sweep frees pages inside the database;
//!    only a vacuum returns them to the OS.
//!
//! Mutation is deliberately CLI-only: `asd-serve` exposes the preview, never
//! the sweep. Run this on the machine that owns the database.

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::Engine;
use agentstatedeveloper_core::gc::{GC_CHECKPOINT_EVERY, GC_GRACE_SECS, GC_KEEP_RECENT, gc_policy};

use crate::config::Config;

/// Batch size for the history extractor — matches `asd index`.
const HISTORY_EXTRACT_BATCH: usize = 5_000;

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Delete every object outside the retention policy's keep-set. Without
    /// this flag `asd gc` only reports what a sweep would reclaim.
    #[arg(long)]
    pub sweep: bool,

    /// Compact the database file afterwards, returning freed pages to the OS.
    /// Needs free disk roughly equal to the database's size while it runs. On
    /// its own (without `--sweep`) it compacts without deleting anything.
    #[arg(long)]
    pub vacuum: bool,

    /// Stop pinning the snapshots of milestones distilled before
    /// AgentStateGraph v1.2.2, which pinned every checkpoint. One-time and
    /// deletes nothing itself; milestones stay on the timeline. Checkpoints
    /// deliberately tagged to pin their state are left alone.
    #[arg(long)]
    pub unpin_legacy: bool,

    /// Keep the state of the N most recent commits in full.
    #[arg(long, default_value_t = GC_KEEP_RECENT)]
    pub keep_recent: usize,

    /// Also keep every Kth older commit's state as a sparse checkpoint. The
    /// default, 0, keeps none: ASD rebuilds derived state from source at the
    /// git revision a milestone records rather than restoring a retained
    /// snapshot, and at one pin per hundred commits a busy store would keep
    /// thousands of full snapshots.
    #[arg(long, default_value_t = GC_CHECKPOINT_EVERY)]
    pub checkpoint_every: usize,

    /// Spare objects written in the last N seconds, so a write another ASD
    /// process (a git hook, asd-serve, an MCP server) has in flight isn't
    /// swept out from under it. A sweep straight after writes therefore
    /// deletes less than the preview counts. 0 spares nothing: use it only
    /// when nothing else writes to this store.
    #[arg(long, default_value_t = GC_GRACE_SECS)]
    pub grace_secs: u64,
}

pub fn run(cfg: &Config, args: GcArgs) -> Result<()> {
    let engine = Engine::open_sqlite_with_gc_grace(
        &cfg.db_path,
        std::time::Duration::from_secs(args.grace_secs),
    )?;
    let repo = &engine.repo;

    let distilled = repo
        .extract_history(HISTORY_EXTRACT_BATCH)
        .context("could not distil history before GC")?
        .commits_processed;

    let unpinned = if args.unpin_legacy {
        Some(
            repo.history_unpin_legacy_milestones()
                .context("could not unpin legacy milestones")?,
        )
    } else {
        None
    };

    let policy = gc_policy(args.keep_recent, args.checkpoint_every);

    let result = if args.sweep {
        repo.gc_sweep(policy, true, args.vacuum).context(
            "sweep failed — if the database is locked, another ASD process \
             (a git hook, asd-serve, an MCP server) is writing; retry when it finishes",
        )?
    } else if args.vacuum {
        json!({ "vacuum": repo.gc_vacuum().context("vacuum failed")? })
    } else {
        repo.gc_sweep(policy, false, false)?
    };

    let out = json!({
        "db": cfg.db_path.display().to_string(),
        "distilled_now": distilled,
        "unpinned_legacy_milestones": unpinned,
        "gc_grace_secs": args.grace_secs,
        "result": result,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    eprintln!("{}", summary(&args, &result));

    if result["refused"] == true {
        // Distillation ran first, so this means a commit landed between it and
        // the sweep taking its lock. Correct to refuse; say so and fail loudly.
        bail!(
            "sweep refused: {}",
            result["reason"].as_str().unwrap_or("unsafe")
        );
    }
    Ok(())
}

/// One human-readable line for stderr; stdout stays machine-readable JSON.
fn summary(args: &GcArgs, result: &Value) -> String {
    let n = |v: &Value| v.as_i64().unwrap_or(0);
    if result["dry_run"] == true {
        let w = &result["would_reclaim"];
        return format!(
            "preview: a sweep would delete {} of {} objects ({:.1}%), keeping {} live{}.{} \
             Nothing was deleted — re-run with --sweep to reclaim.",
            n(&w["reclaimable_objects"]),
            n(&w["total_objects"]),
            w["reclaimable_pct"].as_f64().unwrap_or(0.0),
            n(&w["live_objects"]),
            if result["safe"] == true {
                ""
            } else {
                " (a sweep would currently be refused: history is not fully distilled)"
            },
            match grace(args.grace_secs) {
                Some(g) => format!(" It spares objects written in the last {g}."),
                None => String::new(),
            },
        );
    }
    if result["mutated"] == true {
        let mut line = format!(
            "swept: deleted {} of {} objects, {} remain.",
            n(&result["objects_deleted"]),
            n(&result["objects_before"]),
            n(&result["objects_after"]),
        );
        if let Some(g) = grace(args.grace_secs) {
            line.push_str(&format!(" Objects written in the last {g} were spared."));
        }
        if let Some(v) = result.get("vacuum") {
            line.push_str(&format!(
                " vacuum reclaimed {} bytes on disk.",
                n(&v["bytes_reclaimed"])
            ));
        } else if !args.vacuum {
            line.push_str(" The file only shrinks after --vacuum.");
        }
        return line;
    }
    if let Some(v) = result.get("vacuum") {
        return format!(
            "vacuumed: reclaimed {} bytes on disk.",
            n(&v["bytes_reclaimed"])
        );
    }
    "no change.".to_string()
}

/// The grace period in words, or `None` when there is none.
fn grace(secs: u64) -> Option<String> {
    match secs {
        0 => None,
        1 => Some("second".to_string()),
        60 => Some("minute".to_string()),
        s if s % 60 == 0 => Some(format!("{} minutes", s / 60)),
        s => Some(format!("{s} seconds")),
    }
}
