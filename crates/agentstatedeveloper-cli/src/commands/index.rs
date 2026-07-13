//! `asd index <path>` — walk a directory for source files we have
//! adapters for, parse them, and write Symbol + EffectDecl records into
//! the ASG.
//!
//! A full debug log is always written to `.asd/index.log` in the current
//! directory. `--verbose` tees that same output to stderr in real time.
//! Skipped files are capped at 100 lines on stderr but fully recorded in
//! the log file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_adapters::default_adapters;
use agentstatedeveloper_core::search_fts::SearchFtsDb;
use agentstatedeveloper_core::{
    AsgFeedbackStore, EffectDecl, Engine, FeedbackStore, LedgerEntry, collect_source_files, paths,
    run_index, sync_to_dir,
};

use crate::commands::init::find_project_root;
use crate::config::Config;

const SKIPPED_DISPLAY_LIMIT: usize = 100;

#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Directory (or file) to index. Recursively walks for known source
    /// extensions (`.py`, `.ts`, `.tsx`, `.rs`, `.go`, `.java`, `.cs`, `.rb`, `.kt`, `.swift`).
    /// Defaults to the current directory — `asd index` / `asd reindex`
    /// (no args) both mean "index whatever I'm sitting in."
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Tee the full index log to stderr in real time.
    #[arg(short, long)]
    pub verbose: bool,
}

/// Always-on log that writes every line to `.asd/index.log` and
/// optionally mirrors it to stderr when verbose is set.
struct IndexLog {
    file: std::fs::File,
    verbose: bool,
}

impl IndexLog {
    fn open(verbose: bool) -> Option<Self> {
        let dir = PathBuf::from(".asd");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::File::create(dir.join("index.log"))
            .ok()
            .map(|f| Self { file: f, verbose })
    }

    fn line(&mut self, msg: &str) {
        let _ = writeln!(self.file, "{}", msg);
        if self.verbose {
            eprintln!("{}", msg);
        }
    }
}

pub fn run(cfg: &Config, args: IndexArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let adapters = default_adapters();

    let mut log = IndexLog::open(args.verbose);
    let log_path = PathBuf::from(".asd/index.log");

    // Pre-scan so we know total counts before processing starts.
    let collected = collect_source_files(&args.path, &adapters)?;
    let total = collected.recognized.len();
    let skipped = collected.skipped;

    let header = format!(
        "Indexing {} file{} under {} …",
        total,
        if total == 1 { "" } else { "s" },
        args.path.display()
    );

    // Always print the header to stderr regardless of verbose.
    eprintln!("{}", header);
    if let Some(l) = &mut log {
        l.line(&header);
    }

    if total == 0 {
        let msg = format!(
            "asd index: no recognized source files found under {}",
            args.path.display()
        );
        eprintln!("{}", msg);
        if let Some(l) = &mut log {
            l.line(&msg);
        }
        log_skipped(&skipped, &mut log, true);
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "files": 0,
                "skipped": skipped.len(),
                "symbols": 0,
                "effects": 0,
                "edges": 0,
                "intra_module_edges": 0,
                "cross_module_edges": 0,
                "transitive_updates": 0,
                "orphaned_tagged": 0,
            }))?
        );
        return Ok(());
    }

    // Wrap log in Arc<Mutex> so the progress closure can borrow it.
    let log = Arc::new(Mutex::new(log));
    let log_clone = Arc::clone(&log);
    let width = total.to_string().len();

    let log_clone2 = Arc::clone(&log);

    let progress: &dyn Fn(&Path, usize, usize) = &|file: &Path, idx: usize, total: usize| {
        let msg = format!("  [{idx:>width$}/{total}] {}", file.display());
        if let Ok(mut guard) = log_clone.lock() {
            if let Some(l) = guard.as_mut() {
                l.line(&msg);
            }
        }
    };

    // Phase messages always go to stderr (not just verbose) — they mark the
    // boundary between file parsing and post-processing so the user knows
    // the tool is still working on a large repo.
    let on_phase: &dyn Fn(&str) = &|msg: &str| {
        eprintln!("{}", msg);
        if let Ok(mut guard) = log_clone2.lock() {
            if let Some(l) = guard.as_mut() {
                l.line(msg);
            }
        }
    };

    let summary = run_index(
        &engine.repo,
        &engine.ref_name,
        &args.path,
        &cfg.agent_id,
        &adapters,
        Some(engine.audit.as_ref()),
        Some(progress),
        Some(on_phase),
        Some(&cfg.db_path),
    )?;

    // Unwrap Arc — run_index is done, no other holders.
    let mut log = Arc::try_unwrap(log)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .flatten();

    // Auto-sync sidecar so ledger/effects survive future DB rebuilds without
    // needing a manual `asd sync` call.
    let project_root = find_project_root(&cfg.db_path);
    match sync_to_dir(&engine.repo, &engine.ref_name, &project_root) {
        Ok(s) => {
            let msg = format!(
                "Sidecar synced: {} symbol{}, {} ledger entr{}, {} effect{}.",
                s.symbols_written,
                if s.symbols_written == 1 { "" } else { "s" },
                s.ledger_entries_written,
                if s.ledger_entries_written == 1 {
                    "y"
                } else {
                    "ies"
                },
                s.effects_written,
                if s.effects_written == 1 { "" } else { "s" },
            );
            if let Some(l) = &mut log {
                l.line(&msg);
            }
        }
        Err(e) => {
            let msg = format!("asd: sidecar sync skipped: {e}");
            eprintln!("{msg}");
            if let Some(l) = &mut log {
                l.line(&msg);
            }
        }
    }

    // --- SQLite cache reconciliation ---
    // Pull authoritative git entries into SQLite so subsequent hot-path reads
    // hit the fast path immediately after `asd index`.
    if let Ok(fts) = SearchFtsDb::open(&cfg.db_path) {
        // Feedback
        let fb_store = AsgFeedbackStore::new(&engine.repo);
        if let Ok(all_fb) = fb_store.list_all(&engine.ref_name) {
            if !all_fb.is_empty() {
                if let Err(e) = fts.sync_feedback_entries(&all_fb) {
                    eprintln!("asd: feedback cache sync warning: {e}");
                }
            }
        }

        // Ledger — walk the full tree and bulk-insert into SQLite.
        let ledger_prefix = format!("{}/ledger", paths::ASD_ROOT);
        if let Ok(serde_json::Value::Object(by_symbol)) =
            engine.repo.get_tree(&engine.ref_name, &ledger_prefix)
        {
            let mut ledger_pairs: Vec<(String, LedgerEntry)> = Vec::new();
            for (sym_id, per_symbol) in &by_symbol {
                if let serde_json::Value::Object(entries_map) = per_symbol {
                    for ev in entries_map.values() {
                        if let Ok(e) = serde_json::from_value::<LedgerEntry>(ev.clone()) {
                            ledger_pairs.push((sym_id.clone(), e));
                        }
                    }
                }
            }
            if !ledger_pairs.is_empty() {
                if let Err(e) = fts.sync_ledger_entries(&ledger_pairs, &engine.ref_name) {
                    eprintln!("asd: ledger cache sync warning: {e}");
                }
            }
        }

        // Effects — walk the effects tree and bulk-insert into SQLite.
        let effects_prefix = format!("{}/effects", paths::ASD_ROOT);
        if let Ok(serde_json::Value::Object(by_symbol)) =
            engine.repo.get_tree(&engine.ref_name, &effects_prefix)
        {
            let mut effects_pairs: Vec<(String, EffectDecl)> = Vec::new();
            for (sym_id, val) in &by_symbol {
                if let Ok(decl) = serde_json::from_value::<EffectDecl>(val.clone()) {
                    effects_pairs.push((sym_id.clone(), decl));
                }
            }
            if !effects_pairs.is_empty() {
                if let Err(e) = fts.sync_effects(&effects_pairs, &engine.ref_name) {
                    eprintln!("asd: effects cache sync warning: {e}");
                }
            }
        }
    }

    // Log all skipped files (no cap in log; capped on stderr).
    log_skipped(&skipped, &mut log, args.verbose);

    // Done summary — always to stderr.
    let skipped_hint = if !skipped.is_empty() && !args.verbose {
        format!(
            " ({} file{} skipped — see {})",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            log_path.display()
        )
    } else {
        String::new()
    };
    let doc_hint = if summary.doc_files > 0 {
        format!(
            ", {} doc chunk{} from {} file{}",
            summary.docs_indexed,
            if summary.docs_indexed == 1 { "" } else { "s" },
            summary.doc_files,
            if summary.doc_files == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let done_msg = format!(
        "Done. {} symbol{}, {} effect{}{}.{}",
        summary.symbols,
        if summary.symbols == 1 { "" } else { "s" },
        summary.effects,
        if summary.effects == 1 { "" } else { "s" },
        doc_hint,
        skipped_hint,
    );
    eprintln!("{}", done_msg);
    if let Some(l) = &mut log {
        l.line(&done_msg);
    }

    // t-002: surface cross-service endpoints detected this run.
    if summary.service_endpoints > 0 {
        let ep_msg = format!(
            "{} cross-service endpoint{} detected (see `asd endpoints`).",
            summary.service_endpoints,
            if summary.service_endpoints == 1 {
                ""
            } else {
                "s"
            }
        );
        eprintln!("{}", ep_msg);
        if let Some(l) = &mut log {
            l.line(&ep_msg);
        }
    }

    // t-002 slice 4: surface intra-process data-flow edges (arg→param).
    if summary.dataflow_edges > 0 {
        let df_msg = format!(
            "{} data-flow edge{} (arg→param) detected.",
            summary.dataflow_edges,
            if summary.dataflow_edges == 1 { "" } else { "s" }
        );
        eprintln!("{}", df_msg);
        if let Some(l) = &mut log {
            l.line(&df_msg);
        }
    }

    // Plan L t-005: surface dynamic-dispatch warnings so agents know
    // which call paths the static walker couldn't resolve.
    if summary.dynamic_dispatch_sites > 0 {
        let warn_header = format!(
            "Note: {} dynamic-dispatch site{} detected (calls the static walker can't resolve into edges).",
            summary.dynamic_dispatch_sites,
            if summary.dynamic_dispatch_sites == 1 {
                ""
            } else {
                "s"
            }
        );
        eprintln!("{}", warn_header);
        if let Some(l) = &mut log {
            l.line(&warn_header);
        }
        for h in &summary.dynamic_dispatch_samples {
            let line = format!("  {}:{} [{}] {}", h.file, h.line, h.pattern, h.snippet);
            eprintln!("{}", line);
            if let Some(l) = &mut log {
                l.line(&line);
            }
        }
        if summary.dynamic_dispatch_sites > summary.dynamic_dispatch_samples.len() {
            let more = format!(
                "  …and {} more",
                summary.dynamic_dispatch_sites - summary.dynamic_dispatch_samples.len()
            );
            eprintln!("{}", more);
            if let Some(l) = &mut log {
                l.line(&more);
            }
        }
    }

    // Plan L t-006: surface dropped (unresolved) call edges.
    if summary.dropped_call_edges > 0 {
        let warn_header = format!(
            "Note: {} call site{} couldn't be resolved to a workspace symbol (stdlib / third-party / dynamic).",
            summary.dropped_call_edges,
            if summary.dropped_call_edges == 1 {
                ""
            } else {
                "s"
            }
        );
        eprintln!("{}", warn_header);
        if let Some(l) = &mut log {
            l.line(&warn_header);
        }
        for h in &summary.sample_unresolved {
            let line = format!("  {}:{} {}", h.file, h.line, h.callee_text);
            eprintln!("{}", line);
            if let Some(l) = &mut log {
                l.line(&line);
            }
        }
        if summary.dropped_call_edges > summary.sample_unresolved.len() {
            let more = format!(
                "  …and {} more",
                summary.dropped_call_edges - summary.sample_unresolved.len()
            );
            eprintln!("{}", more);
            if let Some(l) = &mut log {
                l.line(&more);
            }
        }
    }

    let log_note = format!("Full log: {}", log_path.display());
    if let Some(l) = &mut log {
        l.line(&log_note);
    }
    // Only print log path hint to stderr when not verbose (verbose already showed everything).
    if !args.verbose {
        eprintln!("{}", log_note);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "files": summary.files,
            "skipped": summary.skipped,
            "symbols": summary.symbols,
            "effects": summary.effects,
            "edges": summary.edges,
            "intra_module_edges": summary.intra_module_edges,
            "cross_module_edges": summary.cross_module_edges,
            "transitive_updates": summary.transitive_updates,
            "orphaned_tagged": summary.orphaned_tagged,
            "disambiguated": summary.disambiguated,
            "doc_files": summary.doc_files,
            "docs_indexed": summary.docs_indexed,
            "cross_file_collisions": summary.top_collisions.iter().map(|(q, f1, f2)| {
                serde_json::json!({ "qname": q, "first": f1, "second": f2 })
            }).collect::<Vec<_>>(),
            "dynamic_dispatch_sites": summary.dynamic_dispatch_sites,
            "dynamic_dispatch_samples": summary.dynamic_dispatch_samples.iter().map(|h| {
                serde_json::json!({
                    "file": h.file, "line": h.line, "pattern": h.pattern, "snippet": h.snippet,
                })
            }).collect::<Vec<_>>(),
            "dropped_call_edges": summary.dropped_call_edges,
            "sample_unresolved": summary.sample_unresolved.iter().map(|c| {
                serde_json::json!({
                    "file": c.file, "line": c.line, "callee": c.callee_text,
                })
            }).collect::<Vec<_>>(),
            // Plan T: surface cache-sync failures (previously stderr-only
            // eprintln warnings) so agents parsing this JSON can tell a warm
            // DB from one that will pay cold git-walk reads until self-heal.
            "caches_synced": summary.caches_synced,
            "cache_sync_warning": summary.cache_sync_warning,
        }))?
    );

    auto_maintain_registry(&cfg.db_path);

    Ok(())
}

/// Write skipped files to the log and conditionally to stderr.
/// Log receives all files; stderr is capped at SKIPPED_DISPLAY_LIMIT.
fn log_skipped(skipped: &[PathBuf], log: &mut Option<IndexLog>, show_on_stderr: bool) {
    if skipped.is_empty() {
        return;
    }

    let header = format!(
        "  {} file{} skipped (no adapter):",
        skipped.len(),
        if skipped.len() == 1 { "" } else { "s" }
    );

    // Log file always gets the header and all entries.
    if let Some(l) = log.as_mut() {
        l.line(&header);
        for f in skipped {
            l.line(&format!("  [skip] {}", f.display()));
        }
    }

    if !show_on_stderr {
        return;
    }

    // stderr: header + up to SKIPPED_DISPLAY_LIMIT entries.
    eprintln!("{}", header);
    let display = skipped.len().min(SKIPPED_DISPLAY_LIMIT);
    for f in &skipped[..display] {
        eprintln!("  [skip] {}", f.display());
    }
    if skipped.len() > SKIPPED_DISPLAY_LIMIT {
        eprintln!(
            "  … and {} more — see .asd/index.log for the full list",
            skipped.len() - SKIPPED_DISPLAY_LIMIT
        );
    }
}

/// Best-effort registry maintenance after a successful index: register this
/// repo in the shared registry (`~/.config/asd/repos.toml`) and opportunistically
/// self-heal by dropping entries whose db has vanished. Prints one-line notices
/// for a new registration and for any pruned stale entries. All errors are
/// swallowed — this must never fail an index. Skipped entirely for ephemeral
/// paths and when `ASD_NO_AUTO_REGISTER` is set; self-heal alone can be disabled
/// with `ASD_NO_AUTO_PRUNE`.
fn auto_maintain_registry(db_path: &Path) {
    use agentstatedeveloper_core::registry::Registry;

    let abs = match db_path.canonicalize().or_else(|_| {
        if db_path.is_absolute() {
            Ok(db_path.to_path_buf())
        } else {
            std::env::current_dir().map(|c| c.join(db_path))
        }
    }) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Never auto-register ephemeral or explicitly-opted-out repos into the
    // shared registry. This is the main source of registry clutter: every
    // `asd index` on a temp db (integration tests, throwaway checkouts) would
    // otherwise leave a dead entry behind once the temp dir is cleaned. Set
    // `ASD_NO_AUTO_REGISTER` to opt out regardless of path. Existing dead
    // entries are cleaned by `asd repo prune`.
    if std::env::var_os("ASD_NO_AUTO_REGISTER").is_some() || is_ephemeral_path(&abs) {
        return;
    }

    let default_name = abs
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        });
    let Some(name) = default_name.filter(|s| !s.is_empty() && s != "default") else {
        return;
    };

    let mut reg = match Registry::load() {
        Ok(r) => r,
        Err(_) => return,
    };

    // Opportunistic self-heal: drop entries whose db has vanished (dead
    // temp/test/deleted repos). Keeps the shared registry from rotting without
    // a scheduler — the standalone-asd hygiene story. Opt out with
    // ASD_NO_AUTO_PRUNE.
    let pruned = if std::env::var_os("ASD_NO_AUTO_PRUNE").is_some() {
        Vec::new()
    } else {
        reg.prune_missing()
    };
    let mut changed = !pruned.is_empty();
    let mut registered_new = false;

    match reg.get(&name).map(|e| e.path.clone()) {
        // Same repo already registered at this exact path — nothing to add.
        Some(existing) if existing == abs => {}
        // Registered under this name but the path drifted — update it.
        Some(_) => {
            if reg.register(&name, &abs).is_ok() {
                changed = true;
            }
        }
        // Registered under a *different* name — the user named it on purpose;
        // leave it.
        None if reg.list().iter().any(|e| e.path == abs) => {}
        None => {
            if reg.register(&name, &abs).is_ok() {
                changed = true;
                registered_new = true;
            }
        }
    }

    if changed && reg.save().is_ok() {
        if registered_new {
            eprintln!("Registered as '{name}' — use `asd repo use {name}` to make it active.");
        }
        if !pruned.is_empty() {
            eprintln!(
                "Pruned {} stale repo registration(s) whose db no longer exists.",
                pruned.len()
            );
        }
    }
}

/// True if `path` lives under a temporary directory — where integration tests
/// and throwaway checkouts create `.asd-state.db` files. Such repos must never
/// land in the shared registry (they'd become dead entries the moment the temp
/// dir is cleaned). `abs` is already canonicalized by the caller, so `/var`
/// symlinks resolve to `/private/var` on macOS; both spellings are covered.
fn is_ephemeral_path(path: &Path) -> bool {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    let td = std::env::temp_dir();
    if let Ok(c) = td.canonicalize() {
        roots.push(c);
    }
    roots.push(td);
    for r in ["/tmp", "/private/tmp", "/var/folders", "/private/var/folders"] {
        roots.push(std::path::PathBuf::from(r));
    }
    roots.iter().any(|root| path.starts_with(root))
}
