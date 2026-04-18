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

use agentstategraph::Repository;

use crate::effects::{AsgEffectStore, EffectStore};
use crate::error::{AsdError, Result};
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::paths;
use crate::schema::{ASD_SCHEMA_VERSION, EffectDecl, LedgerEntry, Symbol};

/// Relative path (from project root) to the sidecar root.
const SIDECAR_REL_ROOT: &str = ".asd/v1";

/// Result of [`sync_to_dir`]. Counts what was written; the schema
/// version is always stamped.
#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub effects_written: usize,
    pub ledger_entries_written: usize,
    pub symbols_written: usize,
    pub schema_version: String,
}

/// Result of [`hydrate_from_dir`]. `missing_schema_version` is true when
/// the sidecar exists but has no `meta/schema-version` file; hydrate
/// still proceeds but callers should surface the mismatch.
#[derive(Debug, Clone)]
pub struct HydrateSummary {
    pub effects_loaded: usize,
    pub ledger_entries_loaded: usize,
    pub symbols_loaded: usize,
    pub missing_schema_version: bool,
}

/// Mirror live ASG state into the `.asd/v1/` sidecar under `dir`.
///
/// `dir` is the project root; `.asd/v1/` is appended internally.
/// Pre-existing files whose keys aren't in ASG are left alone (orphan
/// handling — see module docs). Overwrites are done atomically enough
/// for the single-writer solo-dev case: write then rename.
pub fn sync_to_dir(
    repo: &Repository,
    ref_name: &str,
    dir: &Path,
) -> Result<SyncSummary> {
    let root = dir.join(SIDECAR_REL_ROOT);
    let effects_dir = root.join("effects");
    let ledger_dir = root.join("ledger");
    let symbols_dir = root.join("symbols");
    let meta_dir = root.join("meta");

    fs::create_dir_all(&effects_dir)?;
    fs::create_dir_all(&ledger_dir)?;
    fs::create_dir_all(&symbols_dir)?;
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

    // Schema version: plain text, single line, for easy git diffing.
    let sv_path = meta_dir.join("schema-version");
    write_text_atomic(&sv_path, &format!("{ASD_SCHEMA_VERSION}\n"))?;

    Ok(SyncSummary {
        effects_written,
        ledger_entries_written,
        symbols_written,
        schema_version: ASD_SCHEMA_VERSION.to_string(),
    })
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
    let meta_dir = root.join("meta");

    let index_store = AsgIndexStore { repo };
    let effect_store = AsgEffectStore { repo };
    let ledger_store = AsgLedgerStore { repo };

    // Symbols first — their qname index is what the CLI and other
    // consumers look up by.
    let mut symbols_loaded = 0usize;
    if symbols_dir.is_dir() {
        for entry in fs::read_dir(&symbols_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let sym: Symbol = serde_json::from_str(&text)?;
            index_store.put_symbol(ref_name, &sym, agent_id)?;
            symbols_loaded += 1;
        }
    }

    // Effects.
    let mut effects_loaded = 0usize;
    if effects_dir.is_dir() {
        for entry in fs::read_dir(&effects_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let decl: EffectDecl = serde_json::from_str(&text)?;
            effect_store.put_effects(ref_name, &decl.symbol_id, &decl, agent_id)?;
            effects_loaded += 1;
        }
    }

    // Ledger entries.
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
                let text = fs::read_to_string(&file_path)?;
                let e: LedgerEntry = serde_json::from_str(&text)?;
                ledger_store.append_entry(ref_name, &e, agent_id)?;
                ledger_entries_loaded += 1;
            }
        }
    }

    let missing_schema_version = !meta_dir.join("schema-version").is_file();

    Ok(HydrateSummary {
        effects_loaded,
        ledger_entries_loaded,
        symbols_loaded,
        missing_schema_version,
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
