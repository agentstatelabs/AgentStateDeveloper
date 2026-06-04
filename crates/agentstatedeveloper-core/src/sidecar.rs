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
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::paths;
use crate::repair::drop_orphaned_edge_refs;
use crate::schema::{ASD_SCHEMA_VERSION, EffectDecl, LedgerEntry, Rebind, Symbol};

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
}

/// Result of [`hydrate_from_dir`]. `missing_schema_version` is true when
/// the sidecar exists but has no `meta/schema-version` file; hydrate
/// still proceeds but callers should surface the mismatch.
#[derive(Debug, Clone)]
pub struct HydrateSummary {
    pub effects_loaded: usize,
    pub ledger_entries_loaded: usize,
    pub symbols_loaded: usize,
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
    let ledger_prefix = format!("{}/ledger", paths::ASD_ROOT);
    if let Ok(serde_json::Value::Object(by_symbol)) = repo.get_tree(ref_name, &ledger_prefix) {
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
    })
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

    let ledger_store = AsgLedgerStore::new(repo);

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
    // Ledger entries — individual writes are fine here; ledger entries are
    // rare and the nested per-symbol path structure makes bulk writes complex.
    // Parse failures skip the file rather than aborting the hydrate.
    // -----------------------------------------------------------------------
    let mut ledger_entries_loaded = 0usize;
    if ledger_dir.is_dir() {
        for sym_entry in fs::read_dir(&ledger_dir)? {
            let sym_entry = sym_entry?;
            let sym_path = sym_entry.path();
            if !sym_path.is_dir() {
                continue;
            }
            for file_entry in fs::read_dir(&sym_path)? {
                let file_entry = file_entry?;
                let file_path = file_entry.path();
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
                        blobs_rejected += 1;
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
                        blobs_rejected += 1;
                        continue;
                    }
                };
                ledger_store.append_entry(ref_name, &entry, agent_id)?;
                ledger_entries_loaded += 1;
            }
        }
    }

    // Rebind records: restore to ASG repo for provenance, then defensively
    // re-parent any ledger entries still stored under old symbol_ids.
    // Sort by `at` timestamp (ascending) so chained rebinds (A→B→C) replay
    // in commit order.
    let mut rebinds_replayed = 0usize;
    if rebinds_dir.is_dir() {
        let mut rebinds: Vec<Rebind> = Vec::new();
        for entry in fs::read_dir(&rebinds_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let rebind: Rebind = serde_json::from_str(&text)?;
            rebinds.push(rebind);
        }
        // Sort by timestamp so chained rebinds apply in correct order.
        rebinds.sort_by_key(|r| r.at);

        for rebind in &rebinds {
            // Restore the rebind record itself for provenance.
            let rebind_path = paths::rebind_path(&rebind.from_symbol_id);
            let val = serde_json::to_value(rebind)?;
            let opts = CommitOptions::new(
                agent_id,
                IntentCategory::Refine,
                format!(
                    "hydrate rebind {} → {}",
                    rebind.from_symbol_id, rebind.to_symbol_id
                ),
            );
            repo.set_json(ref_name, &rebind_path, &val, opts)
                .map_err(|e| AsdError::Other(e.to_string()))?;

            // Defensively re-parent any entries still under from_symbol_id.
            // This handles the case where the sidecar was last synced before
            // the rebind occurred.
            let stale_entries = ledger_store
                .list_entries_with_superseded(ref_name, &rebind.from_symbol_id)
                .unwrap_or_default();
            for mut entry in stale_entries {
                entry.symbol_id = rebind.to_symbol_id.clone();
                let new_path = paths::ledger_entry_path(&rebind.to_symbol_id, &entry.entry_id);
                let entry_val = serde_json::to_value(&entry)?;
                let opts = CommitOptions::new(
                    agent_id,
                    IntentCategory::Refine,
                    format!("reparent entry {} during rebind replay", entry.entry_id),
                );
                if repo
                    .set_json(ref_name, &new_path, &entry_val, opts)
                    .map_err(|e| AsdError::Other(e.to_string()))
                    .is_ok()
                {
                    // Update the reverse index to point to the new symbol_id.
                    let idx_path = paths::ledger_entry_index_path(&entry.entry_id);
                    let idx_val = serde_json::Value::String(rebind.to_symbol_id.clone());
                    let idx_opts = CommitOptions::new(
                        agent_id,
                        IntentCategory::Refine,
                        format!(
                            "ledger-idx reparent {} → {}",
                            entry.entry_id, rebind.to_symbol_id
                        ),
                    );
                    let _ = repo.set_json(ref_name, &idx_path, &idx_val, idx_opts);

                    let old_path =
                        paths::ledger_entry_path(&rebind.from_symbol_id, &entry.entry_id);
                    let opts = CommitOptions::new(
                        agent_id,
                        IntentCategory::Refine,
                        format!("remove stale entry {} after rebind replay", entry.entry_id),
                    );
                    let _ = repo.delete(ref_name, &old_path, opts);
                }
            }
            rebinds_replayed += 1;
        }
    }

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
    let _ = write_text_atomic(
        &meta_dir.join("hydrated-at"),
        &format!("{}\n", chrono::Utc::now().to_rfc3339()),
    );
    let _ = fs::remove_file(meta_dir.join("fresh-reset"));

    Ok(HydrateSummary {
        effects_loaded,
        ledger_entries_loaded,
        symbols_loaded,
        rebinds_replayed,
        missing_schema_version,
        symbols_skipped,
        blobs_rejected,
        refs_dropped,
    })
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
