//! On-disk sidecar for the "git roundtrip" promise.
//!
//! ASD's live state lives in a SQLite-backed ASG repository. The sidecar
//! mirrors the current-state subset of that data to a `.asd/v1/` tree
//! inside the project root so it travels with `git clone` and can hydrate
//! a fresh machine without a network registry.
//!
//! Two entry points:
//!   - [`sync_to_dir`]: ASG -> disk. Walks the `/asd/v1/` ASG tree and
//!     emits one JSON file per effect/ledger entry/symbol under
//!     `<dir>/.asd/v1/`, plus a plaintext `meta/schema-version`.
//!   - [`hydrate_from_dir`]: disk -> ASG. Reads the sidecar back and
//!     writes via the existing `AsgIndexStore`, `AsgEffectStore`,
//!     `AsgLedgerStore` traits, producing equivalent state to what was
//!     synced.
//!
//! ## What the sidecar does NOT carry
//!
//! Per DESIGN.md's three-tier split: the sidecar is current-state only.
//! ASG commit metadata (per-edit intent/confidence/authority) lives in
//! the full-fidelity ASG and is lost on hydrate — that tier is what an
//! opt-in ASG registry would restore. Speculative branches, transitive
//! caches, traces, and the semantic index are also excluded: they're
//! either regenerable (`asd index`, `asd verify-effects`) or registry-
//! only. Effect `verification` fields rehydrate as-is; no re-verification
//! runs during hydrate.
//!
//! ## Orphan handling
//!
//! Sync writes files whose keys are present in ASG and leaves other files
//! untouched. If a symbol is removed from the index, its sidecar files
//! become orphans on disk. M10 accepts this; `asd sync --prune` is a
//! follow-up. (See DEFERRED.md § Miscellaneous.)
//!
//! ## Filename safety
//!
//! Python qnames (`payments.charge_card`) and TypeScript qnames
//! (`driver.main`) are filesystem-safe on macOS and Linux: no slashes,
//! no colons. On Windows there could be conflicts with reserved device
//! names (`COM1`, `LPT1`, `NUL`, `CON`, etc.). M10 ignores that; a
//! follow-up should sanitize or hash Windows-reserved filenames.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use serde_json::Value;

use crate::error::{AsdError, Result};
use crate::paths;
use crate::repair::drop_orphaned_edge_refs;
use crate::schema::{ASD_SCHEMA_VERSION, EffectDecl, LedgerEntry, Rebind, Symbol};
use crate::search_fts::SearchFtsDb;

/// Relative path (from project root) to the sidecar root.
const SIDECAR_REL_ROOT: &str = ".asd/v1";

/// Observable lifecycle state of the on-disk sidecar.
///
/// Agents can use this to distinguish a deliberate reset from an indexing
/// failure, and to know whether `asd hydrate` still needs to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidecarState {
    /// No `.asd/v1/` directory exists — the project has never been synced.
    Missing,
    /// `.asd/v1/` exists with sidecar files but has not yet been hydrated into ASG.
    Present,
    /// `.asd/v1/` was successfully hydrated into ASG (`meta/hydrated-at` is present).
    Hydrated,
    /// The sidecar was deliberately reset (`meta/fresh-reset` sentinel is present).
    FreshReset,
}

/// A warning when the project under `root` runs git hooks from before Plan
/// B, which reload the whole ledger from `.asd/v1/` with `asd hydrate` on
/// every checkout and merge. On a store that holds more than the sidecar —
/// anything since the last `asd sync` — that rewrote every entry each time,
/// and before v1.4.7 it re-filed moved entries under their old symbols.
/// `asd init` installs the current hooks, which import committed
/// conclusions instead.
pub fn outdated_hooks(root: &Path) -> Option<String> {
    let hooks = root.join(".asd/hooks");
    let stale: Vec<&str> = ["post-checkout", "post-merge"]
        .into_iter()
        .filter(|name| {
            fs::read_to_string(hooks.join(name)).is_ok_and(|script| {
                script
                    .lines()
                    .any(|l| !l.trim_start().starts_with('#') && l.contains("asd hydrate"))
            })
        })
        .collect();
    (!stale.is_empty()).then(|| {
        format!(
            "this project's {} hook{} still run{} `asd hydrate`, reloading the whole ledger \
             from .asd/v1 on every checkout — run `asd init` to install the current hooks \
             (they import committed conclusions instead), and commit .asd/hooks",
            stale.join(" and "),
            if stale.len() == 1 { "" } else { "s" },
            if stale.len() == 1 { "s" } else { "" },
        )
    })
}

/// Inspect the on-disk sidecar under `dir` and return its lifecycle state.
///
/// Checks in priority order: `FreshReset` > `Hydrated` > `Present` > `Missing`.
pub fn sidecar_lifecycle_state(dir: &Path) -> SidecarState {
    let root = dir.join(SIDECAR_REL_ROOT);
    if !root.exists() {
        return SidecarState::Missing;
    }
    let meta = root.join("meta");
    if meta.join("fresh-reset").exists() {
        return SidecarState::FreshReset;
    }
    if meta.join("hydrated-at").exists() {
        return SidecarState::Hydrated;
    }
    SidecarState::Present
}

/// Write the `meta/fresh-reset` sentinel so agents know a deliberate reset occurred.
/// Call this when `asd init --reset` or an equivalent wipe operation runs.
pub fn mark_fresh_reset(dir: &Path) -> Result<()> {
    let sentinel = dir.join(SIDECAR_REL_ROOT).join("meta").join("fresh-reset");
    if let Some(parent) = sentinel.parent() {
        fs::create_dir_all(parent)?;
    }
    write_text_atomic(&sentinel, &format!("{}\n", chrono::Utc::now().to_rfc3339()))
}

/// Result of [`sync_to_dir`]. Counts what was written; the schema
/// version is always stamped.
#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub effects_written: usize,
    pub ledger_entries_written: usize,
    pub symbols_written: usize,
    pub rebinds_synced: usize,
    pub schema_version: String,
    /// Files removed by `--prune` (0 when prune was not requested).
    pub pruned: usize,
    /// Ledger files removed because the store now files their entry under
    /// another symbol. Always done, `--prune` or not.
    pub moved_entries_removed: usize,
}

/// Result of [`hydrate_from_dir`]. `missing_schema_version` is true when
/// the sidecar exists but has no `meta/schema-version` file; hydrate
/// still proceeds but callers should surface the mismatch.
#[derive(Debug, Clone)]
pub struct HydrateSummary {
    pub effects_loaded: usize,
    pub ledger_entries_loaded: usize,
    pub symbols_loaded: usize,
    /// Sidecar ledger copies not written: the store already files the entry
    /// (under that symbol at the same or a newer revision, or under another
    /// symbol it was moved to), or another copy of it was taken.
    pub ledger_entries_skipped: usize,
    pub rebinds_replayed: usize,
    pub missing_schema_version: bool,
    /// Symbols whose sidecar file was newer than the existing ASG entry and
    /// overwrote it, OR whose existing ASG entry was already up-to-date
    /// (i.e., no net change). Currently counts collisions detected (both
    /// kept-new and kept-old paths).
    pub symbols_skipped: usize,
    /// JSON parse failures across all sidecar file types (symbols, effects,
    /// ledger). Malformed files are logged to stderr and skipped rather than
    /// aborting the hydrate.
    pub blobs_rejected: usize,
    /// Orphaned callee/caller refs dropped from the call graph after hydrate
    /// to ensure referential integrity.
    pub refs_dropped: usize,
}

/// Mirror live ASG state into the `.asd/v1/` sidecar under `dir`.
///
/// `dir` is the project root; `.asd/v1/` is appended internally.
/// Pre-existing files whose keys aren't in ASG are left alone (orphan
/// handling — see module docs). Overwrites are done atomically enough
/// for the single-writer solo-dev case: write then rename.
///
/// **Scratch entries are excluded by design**: this function only walks
/// the `effects`, `ledger`, `symbols`, and `rebinds` prefixes; the
/// `/asd/v1/scratch/` tree is never read or written.
pub fn sync_to_dir(repo: &Repository, ref_name: &str, dir: &Path) -> Result<SyncSummary> {
    let root = dir.join(SIDECAR_REL_ROOT);
    let effects_dir = root.join("effects");
    let ledger_dir = root.join("ledger");
    let symbols_dir = root.join("symbols");
    let rebinds_dir = root.join("rebinds");
    let meta_dir = root.join("meta");

    fs::create_dir_all(&effects_dir)?;
    fs::create_dir_all(&ledger_dir)?;
    fs::create_dir_all(&symbols_dir)?;
    fs::create_dir_all(&rebinds_dir)?;
    fs::create_dir_all(&meta_dir)?;

    // Effects: one file per EffectDecl at /asd/v1/effects/<symbol_id>.
    let mut effects_written = 0usize;
    let effects_prefix = format!("{}/effects", paths::ASD_ROOT);
    if let Ok(serde_json::Value::Object(map)) = repo.get_tree(ref_name, &effects_prefix) {
        // Sort for deterministic disk order.
        let sorted: BTreeMap<_, _> = map.into_iter().collect();
        for (symbol_id, value) in sorted {
            // Parse to validate shape; the EffectDecl carries symbol_id
            // inside, so rehydration doesn't need the filename.
            let decl: EffectDecl = serde_json::from_value(value)?;
            let out = effects_dir.join(format!("{symbol_id}.json"));
            write_json_atomic(&out, &decl)?;
            effects_written += 1;
        }
    }

    // Ledger: two-level tree, /asd/v1/ledger/<symbol_id>/<entry_id>.
    let mut ledger_entries_written = 0usize;
    let mut moved_entries_removed = 0usize;
    let ledger_prefix = format!("{}/ledger", paths::ASD_ROOT);
    if let Ok(serde_json::Value::Object(by_symbol)) = repo.get_tree(ref_name, &ledger_prefix) {
        moved_entries_removed = remove_moved_entries(&ledger_dir, &by_symbol)?;
        let sorted_syms: BTreeMap<_, _> = by_symbol.into_iter().collect();
        for (symbol_id, bucket) in sorted_syms {
            let serde_json::Value::Object(entries) = bucket else {
                continue;
            };
            if entries.is_empty() {
                continue;
            }
            let sym_dir = ledger_dir.join(&symbol_id);
            fs::create_dir_all(&sym_dir)?;
            let sorted_entries: BTreeMap<_, _> = entries.into_iter().collect();
            for (entry_id, entry_val) in sorted_entries {
                let entry: LedgerEntry = serde_json::from_value(entry_val)?;
                let out = sym_dir.join(format!("{entry_id}.json"));
                write_json_atomic(&out, &entry)?;
                ledger_entries_written += 1;
            }
        }
    }

    // Symbols: mirror the qname index. We duplicate the Symbol payload
    // intentionally — hydrate needs both the by-qname pointer and the
    // payload, and reading from one sidecar source is simpler than
    // splitting across /code/ and /index/by-qname/.
    let mut symbols_written = 0usize;
    let qname_prefix = format!("{}/index/by-qname", paths::ASD_ROOT);
    if let Ok(serde_json::Value::Object(map)) = repo.get_tree(ref_name, &qname_prefix) {
        let sorted: BTreeMap<_, _> = map.into_iter().collect();
        for (qname, value) in sorted {
            let sym: Symbol = serde_json::from_value(value)?;
            let out = symbols_dir.join(format!("{qname}.json"));
            write_json_atomic(&out, &sym)?;
            symbols_written += 1;
        }
    }

    // Rebind records: one file per record at /asd/v1/rebinds/<from_symbol_id>.
    let mut rebinds_synced = 0usize;
    let rebinds_prefix = format!("{}/rebinds", paths::ASD_ROOT);
    if let Ok(serde_json::Value::Object(map)) = repo.get_tree(ref_name, &rebinds_prefix) {
        let sorted: BTreeMap<_, _> = map.into_iter().collect();
        for (from_symbol_id, value) in sorted {
            let rebind: Rebind = serde_json::from_value(value)?;
            let out = rebinds_dir.join(format!("{from_symbol_id}.json"));
            write_json_atomic(&out, &rebind)?;
            rebinds_synced += 1;
        }
    }

    // Schema version: plain text, single line, for easy git diffing.
    let sv_path = meta_dir.join("schema-version");
    write_text_atomic(&sv_path, &format!("{ASD_SCHEMA_VERSION}\n"))?;

    Ok(SyncSummary {
        effects_written,
        ledger_entries_written,
        symbols_written,
        rebinds_synced,
        schema_version: ASD_SCHEMA_VERSION.to_string(),
        pruned: 0,
        moved_entries_removed,
    })
}

/// Remove sidecar ledger files for entries the store now files under a
/// different symbol — moved by a line shift, a file move or a rebind. Left
/// in place, they put each moved entry back under its old symbol on the next
/// `asd hydrate`. Files for entries the store does not hold at all are left
/// for `--prune`. Returns the number of files removed.
fn remove_moved_entries(
    ledger_dir: &Path,
    by_symbol: &serde_json::Map<String, Value>,
) -> Result<usize> {
    if !ledger_dir.is_dir() {
        return Ok(0);
    }
    let stored = crate::ledger_dupes::locations(by_symbol);
    let mut removed = 0usize;
    for sym_entry in fs::read_dir(ledger_dir)? {
        let sym_dir = sym_entry?.path();
        if !sym_dir.is_dir() {
            continue;
        }
        let Some(symbol_id) = sym_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        for file_entry in fs::read_dir(&sym_dir)? {
            let path = file_entry?.path();
            if !is_json_file(&path) {
                continue;
            }
            let Some(entry_id) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            if stored
                .get(entry_id)
                .is_some_and(|symbols| !symbols.contains(&symbol_id))
            {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
        if fs::read_dir(&sym_dir)?.next().is_none() {
            fs::remove_dir(&sym_dir)?;
        }
    }
    Ok(removed)
}

/// Remove orphaned `.asd/v1/` sidecar files — files whose keys no longer
/// exist in the live ASG index. Returns the number of files/dirs removed.
///
/// Orphans accumulate when symbols are renamed or deleted. Run via
/// `asd sync --prune` (also invoked by the pre-commit hook).
pub fn prune_sidecar(repo: &Repository, ref_name: &str, dir: &Path) -> Result<usize> {
    let root = dir.join(SIDECAR_REL_ROOT);
    if !root.exists() {
        return Ok(0);
    }

    let mut pruned = 0usize;

    // Build live key sets from ASG.
    let live_symbol_ids: std::collections::HashSet<String> = {
        let prefix = format!("{}/effects", paths::ASD_ROOT);
        match repo.get_tree(ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().map(|(k, _)| k).collect(),
            _ => std::collections::HashSet::new(),
        }
    };

    let live_qnames: std::collections::HashSet<String> = {
        let prefix = format!("{}/index/by-qname", paths::ASD_ROOT);
        match repo.get_tree(ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().map(|(k, _)| k).collect(),
            _ => std::collections::HashSet::new(),
        }
    };

    let live_ledger_symbol_ids: std::collections::HashSet<String> = {
        let prefix = format!("{}/ledger", paths::ASD_ROOT);
        match repo.get_tree(ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().map(|(k, _)| k).collect(),
            _ => std::collections::HashSet::new(),
        }
    };

    let live_rebind_ids: std::collections::HashSet<String> = {
        let prefix = format!("{}/rebinds", paths::ASD_ROOT);
        match repo.get_tree(ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().map(|(k, _)| k).collect(),
            _ => std::collections::HashSet::new(),
        }
    };

    // Prune effects/<symbol_id>.json
    pruned += prune_flat_dir(&root.join("effects"), &live_symbol_ids)?;

    // Prune symbols/<qname>.json
    pruned += prune_flat_dir(&root.join("symbols"), &live_qnames)?;

    // Prune rebinds/<from_symbol_id>.json
    pruned += prune_flat_dir(&root.join("rebinds"), &live_rebind_ids)?;

    // Prune ledger/<symbol_id>/ directories — remove the whole dir if the
    // symbol is gone, otherwise remove individual entry files that are no
    // longer in ASG.
    let ledger_dir = root.join("ledger");
    if ledger_dir.is_dir() {
        for entry in fs::read_dir(&ledger_dir)? {
            let entry = entry?;
            let sym_dir = entry.path();
            if !sym_dir.is_dir() {
                continue;
            }
            let sym_id = sym_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            if !live_ledger_symbol_ids.contains(&sym_id) {
                // Entire symbol gone — remove the directory tree.
                fs::remove_dir_all(&sym_dir)?;
                pruned += 1;
            } else {
                // Symbol still live — check individual entry files against ASG.
                let live_entries: std::collections::HashSet<String> = {
                    let prefix = format!("{}/ledger/{}", paths::ASD_ROOT, sym_id);
                    match repo.get_tree(ref_name, &prefix) {
                        Ok(serde_json::Value::Object(m)) => m.into_iter().map(|(k, _)| k).collect(),
                        _ => std::collections::HashSet::new(),
                    }
                };
                for file_entry in fs::read_dir(&sym_dir)? {
                    let file_entry = file_entry?;
                    let file_path = file_entry.path();
                    if !is_json_file(&file_path) {
                        continue;
                    }
                    let stem = file_path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    if !live_entries.contains(&stem) {
                        fs::remove_file(&file_path)?;
                        pruned += 1;
                    }
                }
                // Remove the now-empty symbol dir if all entries were pruned.
                if fs::read_dir(&sym_dir)?.next().is_none() {
                    fs::remove_dir(&sym_dir)?;
                }
            }
        }
    }

    Ok(pruned)
}

/// Remove `.json` files from `dir` whose stem is not in `live_keys`.
fn prune_flat_dir(dir: &Path, live_keys: &std::collections::HashSet<String>) -> Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !is_json_file(&path) {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if !live_keys.contains(&stem) {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Read a `.asd/v1/` sidecar under `dir` and write its contents back
/// into the ASG repo via the existing stores.
///
/// Idempotent at the per-file level: rewriting the same JSON payload
/// produces equivalent ASG state (content-addressed storage dedups).
/// ASG commit history is NOT restored — see module docs.
pub fn hydrate_from_dir(
    repo: &Repository,
    ref_name: &str,
    dir: &Path,
    agent_id: &str,
) -> Result<HydrateSummary> {
    hydrate_from_dir_with_cache(repo, None, ref_name, dir, agent_id)
}

/// [`hydrate_from_dir`], also writing the ledger entries it loads into the
/// ledger cache, which answers `list_entries`.
pub fn hydrate_from_dir_with_cache(
    repo: &Repository,
    fts: Option<&SearchFtsDb>,
    ref_name: &str,
    dir: &Path,
    agent_id: &str,
) -> Result<HydrateSummary> {
    let root = dir.join(SIDECAR_REL_ROOT);
    if !root.exists() {
        return Err(AsdError::Other(format!(
            "no sidecar found at {} — did you mean to run `asd sync` first?",
            root.display()
        )));
    }

    let effects_dir = root.join("effects");
    let ledger_dir = root.join("ledger");
    let symbols_dir = root.join("symbols");
    let rebinds_dir = root.join("rebinds");
    let meta_dir = root.join("meta");

    // -----------------------------------------------------------------------
    // Symbols — bulk load: read all sidecar files into memory maps, then
    // write each subtree in one spec_set_json call. O(N) objects vs the
    // O(N²) that individual put_symbol calls produce.
    //
    // Validation: parse failures increment `blobs_rejected` and skip the
    // file. Collisions (same qname OR same content fingerprint already in ASG)
    // increment `symbols_skipped` and retain the live record.
    //
    // The secondary fingerprint check (symbol_fp) handles qname format changes:
    // if the live index was built with a different qname scheme than the sidecar
    // (e.g., after the 0.9.8 Sources-anchor fix), the same code unit exists
    // under two different qnames.  Importing the stale-qname copy would inflate
    // the symbol count and introduce mixed-format call edges.  Skipping by fp
    // keeps only the freshly-indexed, correctly-qnamed symbol.
    // -----------------------------------------------------------------------
    let mut symbols_loaded = 0usize;
    let mut symbols_skipped = 0usize;
    let mut blobs_rejected = 0usize;
    if symbols_dir.is_dir() {
        // Seed from existing state so partial hydrates merge cleanly.
        let mut by_qname: serde_json::Map<String, Value> = repo
            .get_tree(ref_name, "/asd/v1/index/by-qname")
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();

        // Secondary dedup: set of symbol_fp values already present in the live
        // index under *any* qname.  Built once before the import loop so that
        // sidecar symbols whose code hasn't changed are skipped even when their
        // qname differs from the live record (e.g., after a qname format change).
        let live_fps: std::collections::HashSet<String> = by_qname
            .values()
            .filter_map(|v| v.get("symbol_fp")?.as_str().map(|s| s.to_string()))
            .collect();

        // code tree: lang → { "clean_file/symbol_fp" → Symbol }
        let mut by_code: BTreeMap<String, serde_json::Map<String, Value>> = {
            let existing = repo
                .get_tree(ref_name, "/asd/v1/code")
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            existing
                .into_iter()
                .filter_map(|(lang, subtree)| subtree.as_object().cloned().map(|m| (lang, m)))
                .collect()
        };

        for entry in fs::read_dir(&symbols_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("asd hydrate: skipping unreadable {}: {}", path.display(), e);
                    blobs_rejected += 1;
                    continue;
                }
            };
            let sym: Symbol = match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "asd hydrate: skipping malformed symbol {}: {}",
                        path.display(),
                        e
                    );
                    blobs_rejected += 1;
                    continue;
                }
            };

            // Primary collision check: same qname already present with matching
            // fingerprint → no-op.  Different fingerprint → sidecar wins.
            if let Some(existing_val) = by_qname.get(&sym.qname) {
                if let Ok(existing_sym) = serde_json::from_value::<Symbol>(existing_val.clone()) {
                    if existing_sym.symbol_fp == sym.symbol_fp {
                        symbols_skipped += 1;
                        continue; // identical content under same qname — skip
                    }
                    // Different fingerprint → sidecar wins; fall through to insert.
                }
            }

            // Secondary collision check: same symbol_fp already present under
            // a *different* qname in the live index.  This catches stale-qname
            // sidecar entries after a qname format change (e.g., 0.9.8 Sources-
            // anchor) — the symbol was re-indexed with the new qname, so the
            // sidecar copy is a renamed duplicate that must not be imported.
            if live_fps.contains(&sym.symbol_fp) {
                symbols_skipped += 1;
                continue;
            }

            let sym_val = serde_json::to_value(&sym)?;
            let code_key = format!("{}/{}", paths::clean(&sym.file), sym.symbol_fp);
            by_qname.insert(sym.qname.clone(), sym_val.clone());
            by_code
                .entry(sym.language.clone())
                .or_default()
                .insert(code_key, sym_val);
            symbols_loaded += 1;
        }

        if symbols_loaded > 0 {
            let code_tree: serde_json::Map<String, Value> = by_code
                .into_iter()
                .map(|(lang, subtree)| (lang, Value::Object(subtree)))
                .collect();
            let spec = repo
                .speculate(ref_name, Some("asd-hydrate-symbols".into()))
                .map_err(|e| AsdError::Other(e.to_string()))?;
            repo.spec_set_json(spec, "/asd/v1/index/by-qname", &Value::Object(by_qname))
                .map_err(|e| AsdError::Other(e.to_string()))?;
            if !code_tree.is_empty() {
                repo.spec_set_json(spec, "/asd/v1/code", &Value::Object(code_tree))
                    .map_err(|e| AsdError::Other(e.to_string()))?;
            }
            let opts = CommitOptions::new(
                agent_id,
                IntentCategory::Checkpoint,
                format!("asd hydrate: {} symbols", symbols_loaded),
            );
            repo.commit_speculation(spec, opts)
                .map_err(|e| AsdError::Other(e.to_string()))?;
        }
    }

    // -----------------------------------------------------------------------
    // Effects — same bulk approach, with parse-failure tolerance.
    // -----------------------------------------------------------------------
    let mut effects_loaded = 0usize;
    if effects_dir.is_dir() {
        let mut by_effects: serde_json::Map<String, Value> = repo
            .get_tree(ref_name, "/asd/v1/effects")
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();

        for entry in fs::read_dir(&effects_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("asd hydrate: skipping unreadable {}: {}", path.display(), e);
                    blobs_rejected += 1;
                    continue;
                }
            };
            let decl: EffectDecl = match serde_json::from_str(&text) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!(
                        "asd hydrate: skipping malformed effect {}: {}",
                        path.display(),
                        e
                    );
                    blobs_rejected += 1;
                    continue;
                }
            };
            let val = serde_json::to_value(&decl)?;
            by_effects.insert(decl.symbol_id.clone(), val);
            effects_loaded += 1;
        }

        if effects_loaded > 0 {
            let spec = repo
                .speculate(ref_name, Some("asd-hydrate-effects".into()))
                .map_err(|e| AsdError::Other(e.to_string()))?;
            repo.spec_set_json(spec, "/asd/v1/effects", &Value::Object(by_effects))
                .map_err(|e| AsdError::Other(e.to_string()))?;
            let opts = CommitOptions::new(
                agent_id,
                IntentCategory::Checkpoint,
                format!("asd hydrate: {} effects", effects_loaded),
            );
            repo.commit_speculation(spec, opts)
                .map_err(|e| AsdError::Other(e.to_string()))?;
        }
    }

    // -----------------------------------------------------------------------
    // Ledger entries and rebind records — staged on one speculation and
    // committed once (see `hydrate_ledger`).
    // -----------------------------------------------------------------------
    let ledger = hydrate_ledger(
        repo,
        ref_name,
        &ledger_dir,
        &rebinds_dir,
        agent_id,
        &mut blobs_rejected,
    )?;
    if let Some(fts) = fts {
        for entry in &ledger.changed {
            fts.upsert_ledger_entry(entry, ref_name)
                .map_err(|e| AsdError::Other(format!("ledger cache: {e}")))?;
        }
    }
    let ledger_entries_loaded = ledger.loaded;
    let ledger_entries_skipped = ledger.skipped;
    let rebinds_replayed = ledger.rebinds_replayed;

    let missing_schema_version = !meta_dir.join("schema-version").is_file();

    // -----------------------------------------------------------------------
    // Post-hydrate integrity pass: drop any callee/caller refs whose target
    // symbol_id isn't present in the (now fully hydrated) index.  This
    // catches stale edges that the sidecar carried from a previous bad merge.
    // -----------------------------------------------------------------------
    let refs_dropped = drop_orphaned_edge_refs(repo, ref_name, agent_id).unwrap_or_else(|e| {
        eprintln!("asd hydrate: edge-ref cleanup failed: {}", e);
        0
    });

    if refs_dropped > 0 {
        eprintln!(
            "asd hydrate: dropped {} orphaned call-graph ref(s) — run `asd repair` for details",
            refs_dropped
        );
    }

    // Stamp meta/hydrated-at so sidecar_lifecycle_state() can return Hydrated.
    // Also clear any stale fresh-reset sentinel — the hydrate supersedes it.
    let meta_dir = root.join("meta");
    // Best-effort: if the hydrated-at stamp fails to write, lifecycle
    // state falls back to "Stale" on the next status check — annoying
    // but not corruption. We've already done the real work above; don't
    // surface a hydrate failure for a metadata side effect.
    let _ = write_text_atomic(
        &meta_dir.join("hydrated-at"),
        &format!("{}\n", chrono::Utc::now().to_rfc3339()),
    );
    // Idempotent cleanup: the sentinel may not exist (most hydrates
    // are non-reset), and a removal error here can't undo the hydrate.
    let _ = fs::remove_file(meta_dir.join("fresh-reset"));

    Ok(HydrateSummary {
        effects_loaded,
        ledger_entries_loaded,
        symbols_loaded,
        ledger_entries_skipped,
        rebinds_replayed,
        missing_schema_version,
        symbols_skipped,
        blobs_rejected,
        refs_dropped,
    })
}

/// What [`hydrate_ledger`] did.
#[derive(Default)]
struct LedgerHydrate {
    loaded: usize,
    skipped: usize,
    rebinds_replayed: usize,
    /// Entries written, as stored — for the ledger cache.
    changed: Vec<LedgerEntry>,
}

/// One sidecar copy of a ledger entry, with when `asd sync` last wrote it.
struct SidecarCopy {
    symbol_id: String,
    entry: LedgerEntry,
    written: Option<std::time::SystemTime>,
}

/// Every ledger entry under `.asd/v1/ledger/`, grouped by entry id: a
/// sidecar written before v1.4.7 can hold one entry under several symbols.
fn read_sidecar_ledger(
    ledger_dir: &Path,
    blobs_rejected: &mut usize,
) -> Result<BTreeMap<String, Vec<SidecarCopy>>> {
    let mut copies: BTreeMap<String, Vec<SidecarCopy>> = BTreeMap::new();
    if !ledger_dir.is_dir() {
        return Ok(copies);
    }
    for sym_entry in fs::read_dir(ledger_dir)? {
        let sym_path = sym_entry?.path();
        if !sym_path.is_dir() {
            continue;
        }
        for file_entry in fs::read_dir(&sym_path)? {
            let file_path = file_entry?.path();
            if !is_json_file(&file_path) {
                continue;
            }
            let text = match fs::read_to_string(&file_path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!(
                        "asd hydrate: skipping unreadable {}: {}",
                        file_path.display(),
                        e
                    );
                    *blobs_rejected += 1;
                    continue;
                }
            };
            let entry: LedgerEntry = match serde_json::from_str(&text) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!(
                        "asd hydrate: skipping malformed ledger entry {}: {}",
                        file_path.display(),
                        e
                    );
                    *blobs_rejected += 1;
                    continue;
                }
            };
            let written = fs::metadata(&file_path).and_then(|m| m.modified()).ok();
            copies
                .entry(entry.entry_id.clone())
                .or_default()
                .push(SidecarCopy {
                    symbol_id: entry.symbol_id.clone(),
                    entry,
                    written,
                });
        }
    }
    Ok(copies)
}

/// Load the sidecar's ledger entries and rebind records into the store, in
/// one commit.
///
/// - An entry the store already files keeps the place the store gave it — a
///   moved entry stays moved. The sidecar's copy there replaces the stored
///   one only when it is a newer revision, the rule conclusions import uses.
///   Re-filing entries under the symbol the sidecar last saw them on is how
///   SessionDrift-ios came to store 272 entries twice.
/// - An entry new to the store is filed once, under the sidecar copy with
///   the best claim (see [`crate::ledger_dupes::claim`]; then the copy
///   `asd sync` wrote last).
/// - A rebind record the store lacks is restored, and entries still filed
///   under its old symbol move to the new one, as `asd ledger rebind` would.
///
/// Writing each entry with its own two commits stored fresh copies of the
/// ledger and entry-index maps every time: ~24,700 commits per hydrate of a
/// 12,000-entry ledger.
fn hydrate_ledger(
    repo: &Repository,
    ref_name: &str,
    ledger_dir: &Path,
    rebinds_dir: &Path,
    agent_id: &str,
    blobs_rejected: &mut usize,
) -> Result<LedgerHydrate> {
    let copies = read_sidecar_ledger(ledger_dir, blobs_rejected)?;
    let mut rebinds: Vec<Rebind> = Vec::new();
    if rebinds_dir.is_dir() {
        for entry in fs::read_dir(rebinds_dir)? {
            let path = entry?.path();
            if !is_json_file(&path) {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Rebind>(&t).map_err(|e| e.to_string()))
            {
                Ok(rebind) => rebinds.push(rebind),
                Err(e) => {
                    eprintln!("asd hydrate: skipping rebind {}: {}", path.display(), e);
                    *blobs_rejected += 1;
                }
            }
        }
    }
    // Chained rebinds (A→B→C) replay in the order they were made.
    rebinds.sort_by_key(|r| r.at);
    if copies.is_empty() && rebinds.is_empty() {
        return Ok(LedgerHydrate::default());
    }

    let (spec, fork) = crate::subtree::speculate_at_head(repo, ref_name, "asd-hydrate-ledger")?;
    let staged = (|| -> Result<LedgerHydrate> {
        let rebinds_root = format!("{}/rebinds", paths::ASD_ROOT);
        let mut ledger = crate::subtree::read_seed(repo, &fork, &paths::ledger_root())?;
        let mut index = crate::subtree::read_seed(repo, &fork, &paths::ledger_index_root())?;
        let mut records = crate::subtree::read_seed(repo, &fork, &rebinds_root)?;
        let live: std::collections::HashSet<String> =
            crate::subtree::read_seed(repo, &fork, &format!("{}/index/by-qname", paths::ASD_ROOT))?
                .values()
                .filter_map(|v| v.get("symbol_id")?.as_str().map(str::to_string))
                .collect();
        let stored = crate::ledger_dupes::locations(&ledger);
        let mut out = LedgerHydrate::default();
        let mut ledger_changed = false;

        let put = |ledger: &mut serde_json::Map<String, Value>,
                   symbol_id: &str,
                   entry: &LedgerEntry|
         -> Result<()> {
            let Value::Object(entries) = ledger
                .entry(symbol_id.to_string())
                .or_insert_with(|| Value::Object(Default::default()))
            else {
                return Err(AsdError::Other(format!(
                    "ledger node for {symbol_id} is not a map"
                )));
            };
            entries.insert(entry.entry_id.clone(), serde_json::to_value(entry)?);
            Ok(())
        };

        for (entry_id, mut copies) in copies {
            if let Some(symbols) = stored.get(&entry_id) {
                for copy in copies {
                    let current = symbols
                        .contains(&copy.symbol_id)
                        .then(|| ledger.get(&copy.symbol_id)?.get(&entry_id))
                        .flatten()
                        .and_then(|v| serde_json::from_value::<LedgerEntry>(v.clone()).ok());
                    match current {
                        Some(current)
                            if crate::conclusions_export::revised_at(&copy.entry)
                                > crate::conclusions_export::revised_at(&current) =>
                        {
                            put(&mut ledger, &copy.symbol_id, &copy.entry)?;
                            ledger_changed = true;
                            out.loaded += 1;
                            out.changed.push(copy.entry);
                        }
                        _ => out.skipped += 1,
                    }
                }
                continue;
            }
            copies.sort_by(|a, b| {
                crate::ledger_dupes::claim(&b.symbol_id, &b.entry, &live)
                    .cmp(&crate::ledger_dupes::claim(&a.symbol_id, &a.entry, &live))
                    .then(b.written.cmp(&a.written))
                    .then(a.symbol_id.cmp(&b.symbol_id))
            });
            let mut copies = copies.into_iter();
            let Some(keep) = copies.next() else {
                continue;
            };
            out.skipped += copies.len();
            put(&mut ledger, &keep.symbol_id, &keep.entry)?;
            index.insert(entry_id, Value::String(keep.symbol_id.clone()));
            ledger_changed = true;
            out.loaded += 1;
            out.changed.push(keep.entry);
        }

        let rebound_at = format!(
            "rebound-at:{}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
        );
        let mut records_changed = false;
        for rebind in &rebinds {
            let mut replayed = false;
            if !records.contains_key(&rebind.from_symbol_id) {
                records.insert(rebind.from_symbol_id.clone(), serde_json::to_value(rebind)?);
                records_changed = true;
                replayed = true;
            }
            // Entries still filed under the old symbol: the sidecar was
            // synced before the rebind was made.
            if rebind.from_symbol_id != rebind.to_symbol_id
                && let Some(Value::Object(entries)) = ledger.remove(&rebind.from_symbol_id)
            {
                for (entry_id, value) in entries {
                    let mut entry: LedgerEntry = serde_json::from_value(value).map_err(|e| {
                        AsdError::Other(format!(
                            "ledger entry {entry_id} under {}: {e}",
                            rebind.from_symbol_id
                        ))
                    })?;
                    entry.symbol_id = rebind.to_symbol_id.clone();
                    entry
                        .tags
                        .retain(|t| t != "orphaned" && !t.starts_with("orphaned-at:"));
                    entry.tags.push(rebound_at.clone());
                    put(&mut ledger, &rebind.to_symbol_id, &entry)?;
                    index.insert(entry_id, Value::String(rebind.to_symbol_id.clone()));
                    out.changed.push(entry);
                    ledger_changed = true;
                    replayed = true;
                }
            }
            out.rebinds_replayed += usize::from(replayed);
        }

        let write = |path: &str, map: serde_json::Map<String, Value>| {
            repo.spec_set_json(spec, path, &Value::Object(map))
                .map_err(|e| AsdError::Other(e.to_string()))
        };
        if ledger_changed {
            write(&paths::ledger_root(), ledger)?;
            write(&paths::ledger_index_root(), index)?;
        }
        if records_changed {
            write(&rebinds_root, records)?;
        }
        Ok(out)
    })();
    match staged {
        Ok(out) if out.changed.is_empty() && out.rebinds_replayed == 0 => {
            let _ = repo.discard_speculation(spec);
            Ok(out)
        }
        Ok(out) => {
            let opts = CommitOptions::new(
                agent_id,
                IntentCategory::Checkpoint,
                format!(
                    "asd hydrate: {} ledger entr{}, {} rebind(s) ({} sidecar cop{} already in the store)",
                    out.loaded,
                    if out.loaded == 1 { "y" } else { "ies" },
                    out.rebinds_replayed,
                    out.skipped,
                    if out.skipped == 1 { "y" } else { "ies" },
                ),
            );
            repo.commit_speculation(spec, opts)
                .map_err(|e| AsdError::Other(e.to_string()))?;
            Ok(out)
        }
        Err(e) => {
            let _ = repo.discard_speculation(spec);
            Err(e)
        }
    }
}

fn is_json_file(p: &Path) -> bool {
    p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("json")
}

/// Write JSON atomically: serialize to `<path>.tmp`, rename into place.
/// Good enough for the single-writer solo-dev case.
fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &bytes)
}

fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    write_bytes_atomic(path, text.as_bytes())
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path_for(path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".tmp");
    PathBuf::from(os)
}
