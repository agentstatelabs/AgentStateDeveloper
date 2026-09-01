//! Plan B t-004: shared conclusion-export pipeline used by both the CLI
//! (`asd conclusions export`) and MCP (`conclusions_export` tool).
//!
//! Walks every indexed symbol, collects ledger entries, buckets them by
//! `ConclusionClass`, and writes one compact, byte-stable JSONL file per
//! class. The output is intentionally minimal — see DESIGN.md "Plan B" for
//! the field layout and rationale.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::paths::ASD_ROOT;
use crate::schema::{AuthorKind, ConclusionClass, LedgerKind};
use crate::sidecar_config::{ShardBy, SidecarConfig, package_key_for_filename};
use crate::thinking::DEFAULT_CONFIDENCE_FLOOR;

/// Compact JSONL record. `preserve_order` is on for serde_json in this
/// workspace so field order is stable across runs. Plan B t-005 requires
/// Deserialize too so import can round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRecord {
    pub id: String,
    pub kind: String,
    pub qname: String,
    pub file: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Plan T t-007: `LedgerEntry.confidence` (0.0–1.0). Load-bearing
    /// for the Plan G thinking class (Hypothesis confidence gates both
    /// the export floor and read-side `gather_prior_thinking`), but
    /// carried for every kind so no class drops it on round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    pub author: String,
    pub created_at: String,
}

/// Walk the index, bucket every ledger entry by `ConclusionClass`. Each
/// bucket is sorted by `id` alone — entry_ids are deterministic hashes
/// (blake3-derived for Plan G thinking, content-derived for others), so
/// id-sort gives:
///
/// 1. **Byte-identical output across runs** when the entry set is
///    unchanged (re-export is a no-op).
/// 2. **Conflict-resistant git merges**: hash-distributed positions
///    mean two developers' concurrent same-day inserts land in
///    different file regions and don't textually collide. Plan K
///    t-001.
///
/// The previous sort key was `(created_at, id)`. That gave the same
/// byte-stability but clustered same-day inserts together, raising the
/// odds of merge conflicts when two devs added entries on the same day.
/// id-only loses the time-ordered visual diff but gains real
/// conflict-resistance — the Plan K principle (sidecar = judgment;
/// conflicts must be meaningful) prefers the latter.
///
/// Upgrade note: re-exporting an existing project at >=1.0.36 will
/// produce a one-time mass reorder of every `.asd/conclusions/*.jsonl`
/// file. Commit it once, then steady state.
/// What a gather produced, plus what it deliberately withheld.
pub struct Gathered {
    pub buckets: BTreeMap<&'static str, Vec<ExportRecord>>,
    /// Entry ids excluded on purpose — superseded, or a hypothesis below the
    /// confidence floor. Everything else missing from `buckets` is missing
    /// because this clone could not produce it, not because it decided not to.
    pub retired: std::collections::HashSet<String>,
}

/// Back-compat wrapper: the buckets alone.
pub fn gather_buckets(
    engine: &Engine,
) -> std::io::Result<BTreeMap<&'static str, Vec<ExportRecord>>> {
    Ok(gather(engine)?.buckets)
}

pub fn gather(engine: &Engine) -> std::io::Result<Gathered> {
    let index = AsgIndexStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    let mut buckets: BTreeMap<&'static str, Vec<ExportRecord>> = BTreeMap::new();
    for class in ConclusionClass::all() {
        buckets.insert(class.filename_stem(), Vec::new());
    }
    // Ids this clone excluded ON PURPOSE. Distinct from ids it simply could
    // not produce (an orphaned symbol, or a record another branch owns) —
    // see `merge_preserving`.
    let mut retired: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Drive the walk from the LEDGER tree, not the symbol index. Nearly
    // every symbol in a large repo carries zero conclusions, and the
    // per-symbol `list_entries` falls through to an authoritative git
    // `get_json` on a cache-miss (count == 0) — so iterating all 97k+
    // symbols meant ~97k git probes to collect a few hundred entries.
    // The ledger tree has one child per symbol that ACTUALLY has an
    // entry, so this is O(symbols_with_conclusions), matching
    // `detect_orphaned_entries`. Resolve qname/file via the cached
    // symbol_id → Symbol map (one SQLite-backed build, no per-symbol
    // git reads).
    let id_map = index.build_id_map(engine);
    let ledger_prefix = format!("{}/ledger", ASD_ROOT);
    let ledger_tree = engine
        .repo
        .get_tree(&ref_name, &ledger_prefix)
        .unwrap_or(serde_json::Value::Null);
    let by_symbol = match ledger_tree {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };

    // Sort symbol_ids for deterministic traversal (final output is
    // id-sorted below, but a stable walk keeps behavior reproducible).
    let mut symbol_ids: Vec<String> = by_symbol.keys().cloned().collect();
    symbol_ids.sort();

    for symbol_id in symbol_ids {
        let sym = match id_map.get(&symbol_id) {
            Some(s) => s,
            // Orphaned ledger entry: symbol no longer in the index.
            // Matches the prior behavior, which skipped any qname that
            // didn't resolve to a symbol.
            None => continue,
        };
        let per_symbol = match by_symbol.get(&symbol_id) {
            Some(serde_json::Value::Object(m)) => m,
            _ => continue,
        };
        // Parse entries and apply the same superseded-entry filter that
        // `LedgerStore::list_entries` applies (drop any entry that a
        // later entry supersedes).
        let mut entries: Vec<crate::schema::LedgerEntry> = per_symbol
            .values()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        let superseded: std::collections::HashSet<String> = entries
            .iter()
            .flat_map(|e| e.supersedes.iter().cloned())
            .collect();
        // A superseded entry is RETIRED ON PURPOSE — record that, so the
        // additive merge below does not resurrect it from the committed file.
        for e in entries.iter().filter(|e| superseded.contains(&e.entry_id)) {
            retired.insert(e.entry_id.clone());
        }
        entries.retain(|e| !superseded.contains(&e.entry_id));
        for entry in entries {
            // Plan K t-003: confidence-floor filter at sync time.
            // Hypotheses below DEFAULT_CONFIDENCE_FLOOR (0.3) are
            // speculative scratch — still useful locally via
            // `asd think list`, but not durable enough to ship as
            // inherited team judgment. Same threshold as the
            // read-side `gather_prior_thinking` so a new dev sees
            // exactly what the committed sidecar would carry. Only
            // the Hypothesis kind has a confidence semantic worth
            // gating on; Decision/Constraint/etc. carry the same
            // field but the meaning is different (recorded judgment
            // regardless of certainty) so they always ship.
            if entry.kind == LedgerKind::Hypothesis
                && entry.confidence.unwrap_or(0.0) < DEFAULT_CONFIDENCE_FLOOR
            {
                // Also deliberate: a hypothesis that fell below the floor is
                // meant to stop shipping, so it must not be resurrected.
                retired.insert(entry.entry_id.clone());
                continue;
            }
            let class = entry.kind.conclusion_class();
            let stem = class.filename_stem();
            let record = ExportRecord {
                id: entry.entry_id,
                kind: entry.kind.as_str().to_string(),
                qname: sym.qname.clone(),
                file: sym.file.clone(),
                summary: entry.summary,
                body: entry.body,
                confidence: entry.confidence,
                role: entry.role,
                command: entry.command,
                tags: entry.tags,
                evidence: entry
                    .evidence
                    .into_iter()
                    .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
                    .collect(),
                supersedes: entry.supersedes,
                author: format!("{}:{}", author_kind_str(entry.author.kind), entry.author.id),
                created_at: entry.created_at.to_rfc3339(),
            };
            if let Some(bucket) = buckets.get_mut(stem) {
                bucket.push(record);
            }
        }
    }

    // Plan K t-001: sort by id alone (deterministic hash). See the
    // `gather_buckets` docstring for the rationale.
    for bucket in buckets.values_mut() {
        bucket.sort_by(|a, b| a.id.cmp(&b.id));
    }
    Ok(Gathered { buckets, retired })
}

/// Fold the records already committed at `path` back into a freshly gathered
/// bucket, so the sidecar is additive rather than a snapshot of one clone.
///
/// Why this exists
/// ---------------
/// The export writes from THIS clone's database. Two things routinely make
/// that a subset of what the committed file holds, and in both cases writing
/// the bare export deletes records nobody meant to delete:
///
/// * **Orphaned symbols.** A ledger entry anchored to a `:line`-disambiguated
///   qname (`Foo:237`) stops resolving the moment the code shifts, and
///   `gather` skips it. That is how `led_9a6e25e92e8b…` vanished from main —
///   the symbol became `ApiError:269` and the conclusion had nowhere to hang.
/// * **A branch that predates someone else's merge.** Records another branch
///   contributed were never in this clone's database at all.
///
/// What it must NOT do is resurrect a record this clone retired ON PURPOSE —
/// superseded by a later entry, or a hypothesis that fell below the
/// confidence floor. Hence `retired`: a committed record is preserved only
/// when the local store expressed no such intent about it.
pub fn merge_preserving(
    fresh: &[ExportRecord],
    path: &Path,
    retired: &std::collections::HashSet<String>,
) -> std::io::Result<Vec<ExportRecord>> {
    let mut by_id: BTreeMap<String, ExportRecord> = BTreeMap::new();
    for rec in read_jsonl(path).unwrap_or_default() {
        if retired.contains(&rec.id) {
            continue; // deliberately withdrawn — let it go
        }
        by_id.insert(rec.id.clone(), rec);
    }
    // Fresh wins on conflict: this clone's view of a record it CAN see is
    // authoritative over whatever was committed earlier.
    for rec in fresh {
        by_id.insert(rec.id.clone(), rec.clone());
    }
    let mut out: Vec<ExportRecord> = by_id.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Write one JSONL file. Returns the byte count written.
pub fn write_jsonl(path: &Path, records: &[ExportRecord]) -> std::io::Result<u64> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = String::new();
    for rec in records {
        let line = serde_json::to_string(rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        buf.push_str(&line);
        buf.push('\n');
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(buf.as_bytes())?;
    Ok(buf.len() as u64)
}

/// Read a conclusions JSONL file into records. A missing file is treated as
/// empty (an add/add merge can hand us a nonexistent ancestor). Blank lines are
/// skipped; a malformed line is a hard error so the driver falls back to a plain
/// git conflict rather than silently dropping judgment.
pub fn read_jsonl(path: &Path) -> std::io::Result<Vec<ExportRecord>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: ExportRecord = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "conclusions merge: unparseable line in {}: {e}",
                    path.display()
                ),
            )
        })?;
        out.push(rec);
    }
    Ok(out)
}

/// Git merge driver for `.asd/conclusions/*.jsonl`.
///
/// Unions the records of `ours` and `theirs` keyed by `id` and rewrites `ours`
/// in canonical export form (sorted by `id`, one compact record per line) —
/// byte-identical to what [`write_jsonl`] would produce for that set, so the
/// merge result is conflict-free and stable, and the next `export` is a no-op.
///
/// Union semantics (append-and-supersede sidecar): an entry survives iff it is
/// present on either side, so entries superseded away on both sides stay gone,
/// while an entry kept on one side is preserved. `base` is intentionally not
/// consulted for inclusion; the ledger reconciles authoritatively on the next
/// `import`, which is idempotent and keyed by `id`. On the (hash-keyed, so
/// expected-impossible) event of one `id` carrying two different payloads, the
/// lexicographically-greater serialization wins so the output is independent of
/// which side git labeled `ours`.
///
/// Returns the number of records written.
pub fn merge_jsonl(ours: &Path, theirs: &Path) -> std::io::Result<usize> {
    let mut by_id: std::collections::BTreeMap<String, (String, ExportRecord)> =
        std::collections::BTreeMap::new();
    for path in [ours, theirs] {
        for rec in read_jsonl(path)? {
            let ser = serde_json::to_string(&rec)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            match by_id.entry(rec.id.clone()) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert((ser, rec));
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    if ser > o.get().0 {
                        o.insert((ser, rec));
                    }
                }
            }
        }
    }
    // BTreeMap iteration is ordered by `id`, matching `gather_buckets`' sort.
    let records: Vec<ExportRecord> = by_id.into_values().map(|(_, rec)| rec).collect();
    write_jsonl(ours, &records)?;
    Ok(records.len())
}

/// One-shot: gather + write all six files under `out_dir`. Returns per-class
/// (stem, entry_count, byte_count).
///
/// Plan K t-007: honors `.asd/config.toml` for sharding. Config is
/// resolved from `out_dir`'s parent (the project root); missing
/// config = default Class sharding. For per-Package mode, writes
/// `<out_dir>/<stem>/<package-key>.jsonl` and returns one row per
/// non-empty (class, package) pair.
pub fn export_all(
    engine: &Engine,
    out_dir: &Path,
) -> std::io::Result<Vec<(&'static str, usize, u64)>> {
    std::fs::create_dir_all(out_dir)?;
    // out_dir is typically `<root>/.asd/conclusions/`; the config
    // lives at `<root>/.asd/config.toml`. Walk up two parents to
    // find the project root.
    let project_root = out_dir
        .parent() // .asd/
        .and_then(|p| p.parent()) // <root>/
        .unwrap_or(out_dir);
    let cfg = SidecarConfig::load_from_project(project_root);

    match cfg.shard_by {
        ShardBy::Class => export_class_layout(engine, out_dir),
        ShardBy::Package => export_package_layout(engine, out_dir),
    }
}

/// Default layout: one file per ConclusionClass under `out_dir`.
fn export_class_layout(
    engine: &Engine,
    out_dir: &Path,
) -> std::io::Result<Vec<(&'static str, usize, u64)>> {
    let Gathered { buckets, retired } = gather(engine)?;
    let mut out = Vec::with_capacity(6);
    for class in ConclusionClass::all() {
        let stem = class.filename_stem();
        let path = out_dir.join(format!("{stem}.jsonl"));
        let fresh = buckets.get(stem).cloned().unwrap_or_default();
        // Additive: keep what is already committed unless this clone
        // deliberately retired it. See `merge_preserving`.
        let entries = merge_preserving(&fresh, &path, &retired)?;
        let bytes = write_jsonl(&path, &entries)?;
        out.push((stem, entries.len(), bytes));
    }
    Ok(out)
}

/// Per-Package layout: `<out_dir>/<stem>/<package-key>.jsonl`,
/// one file per (class, package_dir) pair. Empty buckets produce
/// no file. Plan K t-007.
fn export_package_layout(
    engine: &Engine,
    out_dir: &Path,
) -> std::io::Result<Vec<(&'static str, usize, u64)>> {
    let Gathered { buckets, retired } = gather(engine)?;
    let mut out = Vec::new();
    for class in ConclusionClass::all() {
        let stem = class.filename_stem();
        let entries = buckets.get(stem).cloned().unwrap_or_default();
        if entries.is_empty() {
            continue;
        }
        // Re-bucket by package_dir(file). The same package always
        // maps to the same shard filename → diffs and merges stay
        // local to whichever package's entry changed.
        let mut by_pkg: BTreeMap<String, Vec<ExportRecord>> = BTreeMap::new();
        for rec in entries {
            let pkg = package_dir(&rec.file);
            by_pkg.entry(pkg).or_default().push(rec);
        }
        let class_dir = out_dir.join(stem);
        std::fs::create_dir_all(&class_dir)?;
        for (pkg, recs) in by_pkg {
            let fname = format!("{}.jsonl", package_key_for_filename(&pkg));
            let path = class_dir.join(fname);
            // Same additive rule, per shard — the shard file is the unit on
            // disk, so that is the unit to preserve against.
            let merged = merge_preserving(&recs, &path, &retired)?;
            let bytes = write_jsonl(&path, &merged)?;
            out.push((stem, merged.len(), bytes));
        }
    }
    Ok(out)
}

/// Directory portion of a file path — `src/pkg/foo.py` → `src/pkg`.
/// Plan K t-007 mirror of the helper in commands/map.rs so the
/// shard layout is computed from the same key.
fn package_dir(file: &str) -> String {
    match file.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => String::new(),
    }
}

// -- Budget enforcement (Plan K t-008) -------------------------------------

/// One shard file's size measurement against the per-shard budget.
#[derive(Debug, Clone)]
pub struct ShardSize {
    /// Repo-relative path to the shard, e.g. `decisions.jsonl` or
    /// `decisions/crates--core.jsonl`.
    pub path: String,
    pub bytes: u64,
    /// True when `bytes > BudgetConfig::per_shard_bytes`.
    pub over_per_shard: bool,
}

/// Result of `check_budget`. `ok` is true iff neither the total
/// nor any individual shard exceeds its configured cap.
#[derive(Debug, Clone)]
pub struct BudgetReport {
    pub ok: bool,
    pub total_bytes: u64,
    pub total_budget: u64,
    pub per_shard_budget: u64,
    pub shards: Vec<ShardSize>,
    /// True when `total_bytes > total_budget`.
    pub over_total: bool,
}

/// Plan K t-008: walk `out_dir`, sum sizes per shard file
/// (including per-package subdirectory shards from t-007), and
/// compare against the budget config. Pure read of the
/// filesystem — does not re-export.
pub fn check_budget(
    out_dir: &Path,
    budget: &crate::sidecar_config::BudgetConfig,
) -> std::io::Result<BudgetReport> {
    let mut shards: Vec<ShardSize> = Vec::new();
    if out_dir.is_dir() {
        collect_jsonl_sizes(out_dir, out_dir, &mut shards)?;
    }
    let mut total_bytes = 0u64;
    for s in &mut shards {
        s.over_per_shard = s.bytes > budget.per_shard_bytes;
        total_bytes = total_bytes.saturating_add(s.bytes);
    }
    let over_total = total_bytes > budget.total_bytes;
    let ok = !over_total && shards.iter().all(|s| !s.over_per_shard);
    // Stable order for deterministic output.
    shards.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(BudgetReport {
        ok,
        total_bytes,
        total_budget: budget.total_bytes,
        per_shard_budget: budget.per_shard_bytes,
        shards,
        over_total,
    })
}

fn collect_jsonl_sizes(base: &Path, dir: &Path, out: &mut Vec<ShardSize>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_sizes(base, &path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(ShardSize {
                path: rel,
                bytes,
                over_per_shard: false,
            });
        }
    }
    Ok(())
}

fn author_kind_str(k: AuthorKind) -> &'static str {
    match k {
        AuthorKind::Agent => "agent",
        AuthorKind::Human => "human",
    }
}

// -- Import (Plan B t-005) ---------------------------------------------------

use crate::schema::{Author, Evidence, LedgerEntry};
use chrono::{DateTime, Utc};

/// Outcome of importing one JSONL file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportFileResult {
    pub class: &'static str,
    pub file: String,
    /// Plan T t-007: total non-empty lines read from the file. Always
    /// equals `imported + skipped_unknown_qname + skipped_parse_error`
    /// — surfaced so callers can spot silent drops (`read > imported`)
    /// without doing the arithmetic themselves.
    pub read: usize,
    pub imported: usize,
    pub skipped_unknown_qname: usize,
    pub skipped_parse_error: usize,
}

/// One-shot: read every `<class>.jsonl` under `in_dir` and upsert each
/// record into the local ledger keyed by entry_id. Idempotent — re-import
/// of unchanged JSONL is a no-op at the bytes level (set_json overwrites
/// with identical content).
///
/// Records whose `qname` is not in the current index are skipped with a
/// counter so callers can warn. Use after `git pull` / on a fresh clone.
///
/// Plan K t-007: reads either layout transparently. For each class:
///   1. If `<in_dir>/<stem>.jsonl` exists (class layout), import that.
///   2. If `<in_dir>/<stem>/` directory exists (package layout),
///      import every `*.jsonl` inside.
/// Both can coexist during a layout migration; the importer handles
/// the union without de-duplication beyond entry_id idempotency.
pub fn import_all(
    engine: &Engine,
    in_dir: &Path,
    agent_id: &str,
) -> std::io::Result<Vec<ImportFileResult>> {
    let mut out = Vec::new();
    for class in ConclusionClass::all() {
        let stem = class.filename_stem();
        // Class-layout file (default).
        let class_file = in_dir.join(format!("{stem}.jsonl"));
        if class_file.is_file() {
            out.push(import_one(engine, &class_file, stem, agent_id)?);
        }
        // Package-layout directory (opt-in via .asd/config.toml).
        // Read every *.jsonl in the per-class subdirectory.
        let pkg_dir = in_dir.join(stem);
        if pkg_dir.is_dir() {
            let mut shard_paths: Vec<std::path::PathBuf> = std::fs::read_dir(&pkg_dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect();
            // Sort for stable import order (helps test determinism).
            shard_paths.sort();
            for path in shard_paths {
                out.push(import_one(engine, &path, stem, agent_id)?);
            }
        }
        // If neither path exists, emit a zero-row result so callers
        // see the class was considered (matches prior behavior).
        if !class_file.is_file() && !pkg_dir.is_dir() {
            out.push(ImportFileResult {
                class: stem,
                file: class_file.to_string_lossy().into_owned(),
                read: 0,
                imported: 0,
                skipped_unknown_qname: 0,
                skipped_parse_error: 0,
            });
        }
    }
    Ok(out)
}

fn import_one(
    engine: &Engine,
    path: &Path,
    stem: &'static str,
    agent_id: &str,
) -> std::io::Result<ImportFileResult> {
    let mut result = ImportFileResult {
        class: stem,
        file: path.display().to_string(),
        read: 0,
        imported: 0,
        skipped_unknown_qname: 0,
        skipped_parse_error: 0,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e),
    };
    let index = AsgIndexStore::from_engine(engine);
    let ledger = AsgLedgerStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        result.read += 1;
        let rec: ExportRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                result.skipped_parse_error += 1;
                continue;
            }
        };
        let sym = match index.get_symbol_by_qname(&ref_name, &rec.qname) {
            Ok(Some(s)) => s,
            _ => {
                result.skipped_unknown_qname += 1;
                continue;
            }
        };
        let entry = match record_to_entry(rec, &sym.symbol_id) {
            Some(e) => e,
            None => {
                result.skipped_parse_error += 1;
                continue;
            }
        };
        if ledger.append_entry(&ref_name, &entry, agent_id).is_ok() {
            result.imported += 1;
        } else {
            result.skipped_parse_error += 1;
        }
    }
    Ok(result)
}

/// Rebuild a LedgerEntry from an ExportRecord. Returns None on unparseable
/// kind, author, or timestamp — caller bumps skipped_parse_error.
fn record_to_entry(rec: ExportRecord, symbol_id: &str) -> Option<LedgerEntry> {
    let kind = parse_kind(&rec.kind)?;
    let author = parse_author(&rec.author)?;
    let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&rec.created_at)
        .ok()?
        .with_timezone(&Utc);
    let evidence: Vec<Evidence> = rec
        .evidence
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    Some(LedgerEntry {
        entry_id: rec.id,
        symbol_id: symbol_id.to_string(),
        kind,
        summary: rec.summary,
        body: rec.body,
        author,
        confidence: rec.confidence,
        evidence,
        supersedes: rec.supersedes,
        created_at,
        tags: rec.tags,
        matched_policy: None,
        role: rec.role,
        command: rec.command,
    })
}

/// Parse the wire string back to a LedgerKind via serde — LedgerKind's
/// `#[serde(rename_all = "snake_case")]` names are exactly what
/// `LedgerKind::as_str()` emits on export, so this stays in lockstep
/// with the enum automatically.
///
/// Plan T t-007: the previous hand-written match predated Plan G and
/// silently rejected the four thinking kinds (hypothesis, mental_model,
/// failed_attempt, open_question) — export wrote thinking.jsonl but
/// import counted every row as a parse error, breaking the fresh-clone
/// restore promise. Serde-derived parsing eliminates that drift class:
/// any future LedgerKind variant round-trips without touching this file.
fn parse_kind(s: &str) -> Option<LedgerKind> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

fn parse_author(s: &str) -> Option<Author> {
    let (kind_str, id) = s.split_once(':')?;
    let kind = match kind_str {
        "agent" => AuthorKind::Agent,
        "human" => AuthorKind::Human,
        _ => return None,
    };
    Some(Author {
        kind,
        id: id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SymbolKind;
    use tempfile::tempdir;

    #[test]
    fn write_jsonl_is_byte_stable_across_runs() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("decisions.jsonl");
        let recs = vec![ExportRecord {
            id: "led_1".into(),
            kind: "decision".into(),
            qname: "App.A".into(),
            file: "a.rs".into(),
            summary: "first".into(),
            body: None,
            confidence: None,
            role: None,
            command: None,
            tags: vec![],
            evidence: vec![],
            supersedes: vec![],
            author: "agent:c".into(),
            created_at: "2026-05-19T20:00:00+00:00".into(),
        }];
        let n1 = write_jsonl(&path, &recs).unwrap();
        let bytes1 = std::fs::read(&path).unwrap();
        let n2 = write_jsonl(&path, &recs).unwrap();
        let bytes2 = std::fs::read(&path).unwrap();
        assert_eq!(n1, n2);
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn optional_fields_are_skipped_in_serialization() {
        let rec = ExportRecord {
            id: "led_abc".into(),
            kind: "decision".into(),
            qname: "App.Foo.bar".into(),
            file: "src/foo.rs".into(),
            summary: "be careful".into(),
            body: None,
            confidence: None,
            role: None,
            command: None,
            tags: vec![],
            evidence: vec![],
            supersedes: vec![],
            author: "agent:claude".into(),
            created_at: "2026-05-19T20:00:00+00:00".into(),
        };
        let line = serde_json::to_string(&rec).unwrap();
        assert!(!line.contains("\"body\""));
        assert!(!line.contains("\"role\""));
        assert!(!line.contains("\"command\""));
        assert!(!line.contains("\"tags\""));
        assert!(line.contains("\"id\":\"led_abc\""));
    }

    #[test]
    fn export_import_round_trips_an_entry() {
        use crate::engine::Engine;
        use crate::index::{AsgIndexStore, IndexStore};
        use crate::ledger::{AsgLedgerStore, LedgerStore};
        use crate::schema::{
            Author, AuthorKind, LedgerEntry, LedgerKind, Position, Symbol, SymbolKind,
        };

        let engine = Engine::open_in_memory().unwrap();
        let index = AsgIndexStore::from_engine(&engine);
        let ledger = AsgLedgerStore::from_engine(&engine);

        // Seed one symbol.
        let sym = Symbol {
            symbol_id: "sym_round_trip".into(),
            symbol_fp: "fp_rt".into(),
            qname: "App.Round.trip".into(),
            language: "rust".into(),
            kind: SymbolKind::Function,
            file: "src/rt.rs".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 10, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym, "test").unwrap();

        // Seed one ledger entry exercising the new fields.
        let mut entry = LedgerEntry::new(
            &sym.symbol_id,
            LedgerKind::FollowUp,
            "diagnostics still need migration",
            Author {
                kind: AuthorKind::Agent,
                id: "claude".into(),
            },
        );
        entry.role = Some("diagnostic-test".into());
        entry.command = Some("swift test --filter X".into());
        entry.tags = vec!["ctx:plan:p".into()];
        let original_id = entry.entry_id.clone();
        ledger
            .append_entry(&engine.ref_name, &entry, "test")
            .unwrap();

        // Export → import into a fresh engine that has the same symbol.
        let tmp = tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();

        let engine2 = Engine::open_in_memory().unwrap();
        let index2 = AsgIndexStore::from_engine(&engine2);
        index2.put_symbol(&engine2.ref_name, &sym, "test").unwrap();
        let results = import_all(&engine2, tmp.path(), "test").unwrap();

        let total_imported: usize = results.iter().map(|r| r.imported).sum();
        assert_eq!(total_imported, 1);

        let ledger2 = AsgLedgerStore::from_engine(&engine2);
        let entries = ledger2
            .list_entries(&engine2.ref_name, &sym.symbol_id)
            .unwrap();
        assert_eq!(entries.len(), 1);
        let imported = &entries[0];
        assert_eq!(imported.entry_id, original_id);
        assert_eq!(imported.kind, LedgerKind::FollowUp);
        assert_eq!(imported.role.as_deref(), Some("diagnostic-test"));
        assert_eq!(imported.command.as_deref(), Some("swift test --filter X"));
        assert_eq!(imported.tags, vec!["ctx:plan:p".to_string()]);
    }

    #[test]
    fn import_is_idempotent_when_run_twice() {
        use crate::engine::Engine;
        use crate::index::{AsgIndexStore, IndexStore};
        use crate::ledger::{AsgLedgerStore, LedgerStore};
        use crate::schema::{
            Author, AuthorKind, LedgerEntry, LedgerKind, Position, Symbol, SymbolKind,
        };

        let engine = Engine::open_in_memory().unwrap();
        let index = AsgIndexStore::from_engine(&engine);
        let ledger = AsgLedgerStore::from_engine(&engine);
        let sym = Symbol {
            symbol_id: "sym_idem".into(),
            symbol_fp: "fp_idem".into(),
            qname: "App.Idem".into(),
            language: "rust".into(),
            kind: SymbolKind::Function,
            file: "src/idem.rs".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 2, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym, "test").unwrap();
        let entry = LedgerEntry::new(
            &sym.symbol_id,
            LedgerKind::Decision,
            "x",
            Author {
                kind: AuthorKind::Agent,
                id: "c".into(),
            },
        );
        ledger
            .append_entry(&engine.ref_name, &entry, "test")
            .unwrap();

        let tmp = tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();

        let engine2 = Engine::open_in_memory().unwrap();
        AsgIndexStore::from_engine(&engine2)
            .put_symbol(&engine2.ref_name, &sym, "test")
            .unwrap();
        import_all(&engine2, tmp.path(), "test").unwrap();
        import_all(&engine2, tmp.path(), "test").unwrap();

        let entries = AsgLedgerStore::from_engine(&engine2)
            .list_entries(&engine2.ref_name, &sym.symbol_id)
            .unwrap();
        assert_eq!(entries.len(), 1, "second import must not duplicate");
    }

    // ---- Plan K t-001: sort-on-write byte-stability + conflict-resistance --

    /// Build a fresh engine with N decision entries inserted in the
    /// given order. Returns the (id, summary) pairs in the order they
    /// were inserted so the caller can reason about the input.
    fn engine_with_decisions_in_order(
        ids_and_summaries: &[(&str, &str)],
    ) -> (Engine, Vec<(String, String)>) {
        let engine = Engine::open_in_memory().unwrap();
        let sym = crate::schema::Symbol {
            symbol_id: "sym_order_test".into(),
            symbol_fp: "fp".into(),
            qname: "pkg.target".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/target.py".into(),
            start: crate::schema::Position { line: 1, col: 0 },
            end: crate::schema::Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        AsgIndexStore::from_engine(&engine)
            .put_symbol(&engine.ref_name, &sym, "test")
            .unwrap();
        let ledger = AsgLedgerStore::from_engine(&engine);
        let mut inserted = Vec::new();
        for (id, summary) in ids_and_summaries {
            let mut entry = LedgerEntry::new(
                &sym.symbol_id,
                LedgerKind::Decision,
                *summary,
                Author {
                    kind: AuthorKind::Agent,
                    id: "t".into(),
                },
            );
            entry.entry_id = (*id).to_string();
            ledger.append_entry(&engine.ref_name, &entry, "t").unwrap();
            inserted.push(((*id).to_string(), (*summary).to_string()));
        }
        (engine, inserted)
    }

    fn sha_of_file(p: &Path) -> [u8; 32] {
        let bytes = std::fs::read(p).expect("read jsonl");
        *blake3::hash(&bytes).as_bytes()
    }

    #[test]
    fn export_is_byte_identical_across_repeated_runs() {
        // Plan K t-001 byte-stability claim, scoped correctly: SAME
        // engine, two consecutive exports → identical bytes. (Across
        // independent engines the per-entry `created_at` timestamp
        // differs at append time, so byte-equality across engines is
        // expected to fail and not what id-sort can fix.)
        let entries = [
            ("led_aaa", "first decision"),
            ("led_zzz", "third decision"),
            ("led_mmm", "second decision"),
        ];
        let (engine, _) = engine_with_decisions_in_order(&entries);

        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        export_all(&engine, tmp1.path()).unwrap();
        export_all(&engine, tmp2.path()).unwrap();

        assert_eq!(
            sha_of_file(&tmp1.path().join("decisions.jsonl")),
            sha_of_file(&tmp2.path().join("decisions.jsonl")),
            "two consecutive exports from the same engine must hash-equal"
        );
    }

    #[test]
    fn export_byte_output_is_deterministic_under_id_sort() {
        // Stronger guarantee: the id-sort sorts the SAME entries into
        // the SAME order regardless of insertion order. Verify by
        // building two engines with identical (id, summary, …) inputs
        // but inserted in different orders, stripping the per-entry
        // created_at before comparing, then asserting both yield the
        // same line sequence.
        let order_a = [
            ("led_aaa", "first decision"),
            ("led_zzz", "third decision"),
            ("led_mmm", "second decision"),
        ];
        let order_b = [
            ("led_zzz", "third decision"),
            ("led_mmm", "second decision"),
            ("led_aaa", "first decision"),
        ];

        let (engine_a, _) = engine_with_decisions_in_order(&order_a);
        let (engine_b, _) = engine_with_decisions_in_order(&order_b);

        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        export_all(&engine_a, tmp_a.path()).unwrap();
        export_all(&engine_b, tmp_b.path()).unwrap();

        // Read line-by-line and pull out (id, summary) — drop the
        // engine-specific created_at — and assert the SEQUENCE
        // matches between the two engines.
        let extract = |path: &Path| -> Vec<(String, String)> {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|l| {
                    let v: serde_json::Value = serde_json::from_str(l).unwrap();
                    (
                        v["id"].as_str().unwrap_or("").to_string(),
                        v["summary"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        };
        let seq_a = extract(&tmp_a.path().join("decisions.jsonl"));
        let seq_b = extract(&tmp_b.path().join("decisions.jsonl"));
        assert_eq!(
            seq_a, seq_b,
            "id-sort must yield identical (id, summary) sequence regardless of insertion order"
        );
        // And the sequence must actually be id-sorted.
        let ids: Vec<&str> = seq_a.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["led_aaa", "led_mmm", "led_zzz"],
            "entries must come out in id-sorted order"
        );
    }

    /// Seed an engine with N indexed symbols spread across two
    /// packages, then return the engine. Used by Plan K t-007
    /// sharding tests.
    fn engine_with_multi_package_decisions() -> Engine {
        let engine = Engine::open_in_memory().unwrap();
        let mk_sym = |sid: &str, qname: &str, file: &str| crate::schema::Symbol {
            symbol_id: sid.into(),
            symbol_fp: "fp".into(),
            qname: qname.into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: file.into(),
            start: crate::schema::Position { line: 1, col: 0 },
            end: crate::schema::Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        let index = AsgIndexStore::from_engine(&engine);
        let sym_a = mk_sym("sym_a", "pkg.alpha.fn_a", "src/alpha/mod.py");
        let sym_b = mk_sym("sym_b", "pkg.beta.fn_b", "src/beta/mod.py");
        index.put_symbol(&engine.ref_name, &sym_a, "t").unwrap();
        index.put_symbol(&engine.ref_name, &sym_b, "t").unwrap();

        let append = |sid: &str, eid: &str, summary: &str| {
            let mut entry = LedgerEntry::new(
                sid,
                LedgerKind::Decision,
                summary,
                Author {
                    kind: AuthorKind::Agent,
                    id: "t".into(),
                },
            );
            entry.entry_id = eid.into();
            AsgLedgerStore::from_engine(&engine)
                .append_entry(&engine.ref_name, &entry, "t")
                .unwrap();
        };
        append("sym_a", "led_a1", "alpha decision 1");
        append("sym_a", "led_a2", "alpha decision 2");
        append("sym_b", "led_b1", "beta decision 1");
        engine
    }

    // ---- Plan K t-008: budget enforcement ------------------------------

    #[test]
    fn check_budget_reports_ok_when_well_under_limits() {
        use crate::sidecar_config::BudgetConfig;
        let tmp = tempdir().unwrap();
        // Write one small JSONL file.
        std::fs::write(tmp.path().join("decisions.jsonl"), b"{}\n{}\n").unwrap();
        let budget = BudgetConfig {
            total_bytes: 1024,
            per_shard_bytes: 1024,
        };
        let r = check_budget(tmp.path(), &budget).unwrap();
        assert!(r.ok);
        assert!(!r.over_total);
        assert_eq!(r.shards.len(), 1);
        assert!(!r.shards[0].over_per_shard);
    }

    #[test]
    fn check_budget_flags_over_total() {
        use crate::sidecar_config::BudgetConfig;
        let tmp = tempdir().unwrap();
        let blob = vec![b'x'; 600];
        std::fs::write(tmp.path().join("a.jsonl"), &blob).unwrap();
        std::fs::write(tmp.path().join("b.jsonl"), &blob).unwrap();
        let budget = BudgetConfig {
            total_bytes: 1000, // 1200 actual > 1000 budget
            per_shard_bytes: 1024,
        };
        let r = check_budget(tmp.path(), &budget).unwrap();
        assert!(!r.ok);
        assert!(r.over_total);
        // Neither individual shard violates per-shard.
        assert!(r.shards.iter().all(|s| !s.over_per_shard));
    }

    #[test]
    fn check_budget_flags_over_per_shard() {
        use crate::sidecar_config::BudgetConfig;
        let tmp = tempdir().unwrap();
        let big = vec![b'x'; 500];
        std::fs::write(tmp.path().join("big.jsonl"), &big).unwrap();
        let budget = BudgetConfig {
            total_bytes: 10_000,
            per_shard_bytes: 200, // 500 actual > 200 per-shard
        };
        let r = check_budget(tmp.path(), &budget).unwrap();
        assert!(!r.ok);
        assert!(!r.over_total);
        assert!(r.shards[0].over_per_shard);
    }

    #[test]
    fn check_budget_walks_into_package_subdirs() {
        // Plan K t-007 + t-008: when shard_by = "package" produces a
        // subdirectory layout, the budget walker must descend into
        // it. Sets up a per-class subdir + a file inside, asserts
        // the file is counted.
        use crate::sidecar_config::BudgetConfig;
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("decisions")).unwrap();
        std::fs::write(tmp.path().join("decisions/crates--core.jsonl"), b"row\n").unwrap();
        let r = check_budget(
            tmp.path(),
            &BudgetConfig {
                total_bytes: 1024,
                per_shard_bytes: 1024,
            },
        )
        .unwrap();
        assert_eq!(r.shards.len(), 1);
        assert!(
            r.shards[0].path.contains("decisions/crates--core.jsonl")
                || r.shards[0].path.contains("decisions\\crates--core.jsonl"),
            "shard path must include the subdir; got {:?}",
            r.shards[0].path
        );
    }

    #[test]
    fn check_budget_handles_missing_directory_gracefully() {
        use crate::sidecar_config::BudgetConfig;
        let tmp = tempdir().unwrap();
        // tmp/conclusions doesn't exist.
        let r = check_budget(&tmp.path().join("conclusions"), &BudgetConfig::default()).unwrap();
        assert!(r.ok, "empty/missing dir is trivially under budget");
        assert_eq!(r.total_bytes, 0);
        assert!(r.shards.is_empty());
    }

    #[test]
    fn package_layout_writes_per_package_shards() {
        // Plan K t-007 acceptance: with `shard_by = "package"`,
        // `export_all` writes `<out_dir>/<stem>/<pkg-key>.jsonl`
        // rather than `<out_dir>/<stem>.jsonl`.
        let tmp = tempdir().unwrap();
        let project_root = tmp.path();
        std::fs::create_dir_all(project_root.join(".asd")).unwrap();
        std::fs::write(
            project_root.join(".asd/config.toml"),
            "[sidecar]\nshard_by = \"package\"\n",
        )
        .unwrap();

        let engine = engine_with_multi_package_decisions();
        let conclusions_dir = project_root.join(".asd/conclusions");
        export_all(&engine, &conclusions_dir).unwrap();

        // Default layout files must NOT exist.
        assert!(
            !conclusions_dir.join("decisions.jsonl").is_file(),
            "package layout must not write a flat decisions.jsonl"
        );
        // Per-package files MUST exist.
        let alpha = conclusions_dir.join("decisions/src--alpha.jsonl");
        let beta = conclusions_dir.join("decisions/src--beta.jsonl");
        assert!(alpha.is_file(), "expected per-package shard at {alpha:?}");
        assert!(beta.is_file(), "expected per-package shard at {beta:?}");

        let alpha_body = std::fs::read_to_string(&alpha).unwrap();
        assert!(alpha_body.contains("led_a1") && alpha_body.contains("led_a2"));
        assert!(
            !alpha_body.contains("led_b1"),
            "beta entry must not leak into alpha shard; got:\n{alpha_body}"
        );
    }

    #[test]
    fn class_layout_is_default_without_config() {
        // No .asd/config.toml → default Class sharding, single flat
        // file per class. Locks the backward-compat promise.
        let tmp = tempdir().unwrap();
        let project_root = tmp.path();
        // intentionally no config.toml

        let engine = engine_with_multi_package_decisions();
        let conclusions_dir = project_root.join(".asd/conclusions");
        export_all(&engine, &conclusions_dir).unwrap();

        let flat = conclusions_dir.join("decisions.jsonl");
        assert!(
            flat.is_file(),
            "default layout must write flat decisions.jsonl"
        );
        let body = std::fs::read_to_string(&flat).unwrap();
        for id in ["led_a1", "led_a2", "led_b1"] {
            assert!(
                body.contains(id),
                "flat file must contain {id}; got:\n{body}"
            );
        }
        // The per-package subdir must NOT have been created.
        assert!(
            !conclusions_dir.join("decisions").is_dir(),
            "default layout must not create per-class subdirectory"
        );
    }

    #[test]
    fn import_reads_package_layout_transparently() {
        // Plan K t-007: round-trip through package layout — export
        // with sharding on, import into a fresh engine, verify all
        // entries land in the ledger.
        let tmp = tempdir().unwrap();
        let project_root = tmp.path();
        std::fs::create_dir_all(project_root.join(".asd")).unwrap();
        std::fs::write(
            project_root.join(".asd/config.toml"),
            "[sidecar]\nshard_by = \"package\"\n",
        )
        .unwrap();

        let engine = engine_with_multi_package_decisions();
        let conclusions_dir = project_root.join(".asd/conclusions");
        export_all(&engine, &conclusions_dir).unwrap();

        // Fresh engine; replay the same symbols so import has qnames
        // to match against.
        let engine2 = Engine::open_in_memory().unwrap();
        let index2 = AsgIndexStore::from_engine(&engine2);
        for (sid, qn, file) in [
            ("sym_a", "pkg.alpha.fn_a", "src/alpha/mod.py"),
            ("sym_b", "pkg.beta.fn_b", "src/beta/mod.py"),
        ] {
            let sym = crate::schema::Symbol {
                symbol_id: sid.into(),
                symbol_fp: "fp".into(),
                qname: qn.into(),
                language: "python".into(),
                kind: SymbolKind::Function,
                file: file.into(),
                start: crate::schema::Position { line: 1, col: 0 },
                end: crate::schema::Position { line: 5, col: 0 },
                signature: None,
                doc: None,
            };
            index2.put_symbol(&engine2.ref_name, &sym, "t").unwrap();
        }
        let results = import_all(&engine2, &conclusions_dir, "t").unwrap();
        let total: usize = results.iter().map(|r| r.imported).sum();
        assert_eq!(
            total, 3,
            "all 3 entries (2 alpha + 1 beta) must import; got results = {results:?}"
        );
        // Confirm by reading the ledger directly.
        let ledger2 = AsgLedgerStore::from_engine(&engine2);
        let a = ledger2.list_entries(&engine2.ref_name, "sym_a").unwrap();
        let b = ledger2.list_entries(&engine2.ref_name, "sym_b").unwrap();
        assert_eq!(a.len(), 2, "sym_a must have 2 imported entries");
        assert_eq!(b.len(), 1, "sym_b must have 1 imported entry");
    }

    /// Append a single thinking-class entry of a given kind to the
    /// given engine's `pkg.target` symbol. Used by the Plan K t-003
    /// confidence-floor tests.
    fn append_thinking_entry(
        engine: &Engine,
        kind: LedgerKind,
        id: &str,
        summary: &str,
        confidence: Option<f64>,
    ) {
        let mut entry = LedgerEntry::new(
            "sym_order_test",
            kind,
            summary,
            Author {
                kind: AuthorKind::Agent,
                id: "t".into(),
            },
        );
        entry.entry_id = id.to_string();
        entry.confidence = confidence;
        AsgLedgerStore::from_engine(engine)
            .append_entry(&engine.ref_name, &entry, "t")
            .unwrap();
    }

    #[test]
    fn export_drops_low_confidence_hypotheses() {
        // Plan K t-003 acceptance: a Hypothesis below
        // DEFAULT_CONFIDENCE_FLOOR (0.3) must NOT appear in the
        // exported sidecar, even though it's in the ledger.
        let (engine, _) = engine_with_decisions_in_order(&[]);
        // High-confidence: should ship.
        append_thinking_entry(
            &engine,
            LedgerKind::Hypothesis,
            "led_strong",
            "well-supported hypothesis",
            Some(0.7),
        );
        // Low-confidence: must be filtered out.
        append_thinking_entry(
            &engine,
            LedgerKind::Hypothesis,
            "led_weak",
            "speculative guess",
            Some(0.1),
        );
        // Hypothesis with no confidence: treated as 0.0 → dropped.
        append_thinking_entry(
            &engine,
            LedgerKind::Hypothesis,
            "led_unset",
            "no confidence assigned",
            None,
        );

        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("thinking.jsonl")).unwrap();

        assert!(
            body.contains("led_strong"),
            "high-confidence hypothesis must ship; got:\n{body}"
        );
        assert!(
            !body.contains("led_weak"),
            "low-confidence hypothesis must NOT ship; got:\n{body}"
        );
        assert!(
            !body.contains("led_unset"),
            "hypothesis with no confidence must NOT ship; got:\n{body}"
        );
    }

    #[test]
    fn confidence_floor_does_not_affect_non_hypothesis_kinds() {
        // Plan K t-003: the filter is Hypothesis-only. MentalModels,
        // FailedAttempts, and OpenQuestions don't carry a confidence
        // semantic in the same way — they're recorded judgment
        // regardless of certainty. Verify they ship even with
        // confidence below the floor (or unset).
        let (engine, _) = engine_with_decisions_in_order(&[]);
        append_thinking_entry(
            &engine,
            LedgerKind::MentalModel,
            "led_mm_low",
            "mental model with low conf marker",
            Some(0.1),
        );
        append_thinking_entry(
            &engine,
            LedgerKind::OpenQuestion,
            "led_oq_unset",
            "what does this mean?",
            None,
        );
        append_thinking_entry(
            &engine,
            LedgerKind::FailedAttempt,
            "led_fa_low",
            "tried X, failed because Y",
            Some(0.1),
        );

        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("thinking.jsonl")).unwrap();

        for must_appear in ["led_mm_low", "led_oq_unset", "led_fa_low"] {
            assert!(
                body.contains(must_appear),
                "non-Hypothesis kind {must_appear} must ship regardless of confidence; got:\n{body}"
            );
        }
    }

    #[test]
    fn confidence_floor_does_not_affect_decisions() {
        // A Decision is a recorded choice; even "I'm 10% sure about
        // this" is still a decision worth committing. The filter
        // must only gate Hypothesis.
        let (engine, _) = engine_with_decisions_in_order(&[]);
        append_thinking_entry(
            &engine,
            LedgerKind::Decision,
            "led_dec_low",
            "decided X with low certainty",
            Some(0.1),
        );

        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("decisions.jsonl")).unwrap();
        assert!(
            body.contains("led_dec_low"),
            "low-confidence Decision must still ship; got:\n{body}"
        );
    }

    #[test]
    fn exported_entries_are_self_describing() {
        // Plan K t-004: every emitted entry must carry qname, kind,
        // and summary inline (no symbol_id-only references). An agent
        // reading raw .asd/conclusions/*.jsonl without a hydrated
        // index must still be able to grep for a qname and find
        // every relevant entry.
        let entries = [("led_target_1", "first decision about pkg.target")];
        let (engine, _) = engine_with_decisions_in_order(&entries);
        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();

        let body = std::fs::read_to_string(tmp.path().join("decisions.jsonl")).unwrap();
        // Grep-by-qname must hit. (The grep test mirrors what an
        // agent reading the file cold would do.)
        assert!(
            body.contains("pkg.target"),
            "qname must appear in serialized line for grep-by-qname; got: {body}"
        );

        // Structural check: parse the line and confirm the
        // self-describing fields are present and non-empty.
        let v: serde_json::Value =
            serde_json::from_str(body.lines().next().expect("one line")).unwrap();
        for field in ["id", "kind", "qname", "summary"] {
            let s = v[field].as_str().unwrap_or("");
            assert!(
                !s.is_empty(),
                "field `{field}` must be present and non-empty for self-describing entries; got {v}"
            );
        }
        // No symbol_id leak — entries reference symbols by qname
        // (human-readable) only.
        assert!(
            v.get("symbol_id").is_none(),
            "ExportRecord must not leak opaque symbol_id; got {v}"
        );
    }

    #[test]
    fn export_sorts_by_id_not_by_created_at() {
        // Conflict-resistance check: with id-sort, two same-day
        // entries with very different ids end up far apart in the
        // file. Verify by inserting two entries and checking the
        // file lines them up in id order, not insertion order.
        let entries = [
            ("led_zzz", "zzz summary inserted first"),
            ("led_aaa", "aaa summary inserted second"),
        ];
        let (engine, _) = engine_with_decisions_in_order(&entries);
        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();

        let body = std::fs::read_to_string(tmp.path().join("decisions.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // led_aaa < led_zzz lexicographically; aaa must come first
        // even though it was inserted second.
        assert!(
            lines[0].contains("led_aaa"),
            "id-sort: led_aaa must precede led_zzz; got first line = {}",
            lines[0]
        );
        assert!(
            lines[1].contains("led_zzz"),
            "id-sort: led_zzz must follow led_aaa; got second line = {}",
            lines[1]
        );
    }

    // ---- Plan T t-007: lossless round-trip incl. thinking + confidence --

    #[test]
    fn parse_kind_accepts_every_ledger_kind_string() {
        // The old hand-written parse_kind predated Plan G and silently
        // rejected the four thinking kinds. Lock the invariant that
        // every LedgerKind round-trips as_str → parse_kind.
        let all = [
            LedgerKind::Decision,
            LedgerKind::Assumption,
            LedgerKind::Constraint,
            LedgerKind::Rationale,
            LedgerKind::Hazard,
            LedgerKind::Tradeoff,
            LedgerKind::Invariant,
            LedgerKind::Ownership,
            LedgerKind::Proof,
            LedgerKind::ValidationScenario,
            LedgerKind::KnownBug,
            LedgerKind::Concept,
            LedgerKind::Mapping,
            LedgerKind::FollowUp,
            LedgerKind::Hypothesis,
            LedgerKind::MentalModel,
            LedgerKind::FailedAttempt,
            LedgerKind::OpenQuestion,
        ];
        for kind in all {
            assert_eq!(
                parse_kind(kind.as_str()),
                Some(kind),
                "parse_kind must accept `{}`",
                kind.as_str()
            );
        }
        assert_eq!(parse_kind("no_such_kind"), None);
    }

    #[test]
    fn export_wipe_import_round_trips_all_classes_losslessly() {
        // Plan T t-007 acceptance: export → wipe ledger (fresh engine)
        // → import → field-level diff. Covers all four thinking kinds
        // with their body payloads (mental-model symbols[], failed-
        // attempt tried/because), confidence values, tags, and a
        // representative of every other conclusion class so no class
        // drops fields silently.
        use crate::schema::{Author, AuthorKind, LedgerEntry, LedgerKind};

        let (engine, _) = engine_with_decisions_in_order(&[]);
        let ledger = AsgLedgerStore::from_engine(&engine);

        let mk = |kind: LedgerKind, id: &str, summary: &str| -> LedgerEntry {
            let mut e = LedgerEntry::new(
                "sym_order_test",
                kind,
                summary,
                Author {
                    kind: AuthorKind::Agent,
                    id: "t".into(),
                },
            );
            e.entry_id = id.to_string();
            e
        };

        let mut originals: Vec<LedgerEntry> = Vec::new();

        // Thinking class: all four kinds.
        let mut hyp = mk(
            LedgerKind::Hypothesis,
            "led_rt_hyp",
            "suspect the cache is stale",
        );
        hyp.confidence = Some(0.85);
        hyp.body = Some("saw stale reads under load".into());
        hyp.tags = vec!["source:asd-think".into()];
        originals.push(hyp);

        let mut mm = mk(
            LedgerKind::MentalModel,
            "led_rt_mm",
            "pipeline flows input -> mix -> out",
        );
        mm.body = Some(r#"{"symbols":["pkg.target","pkg.other"],"name":"audio-pipeline"}"#.into());
        originals.push(mm);

        let mut fa = mk(
            LedgerKind::FailedAttempt,
            "led_rt_fa",
            "tried batching, failed on ordering",
        );
        fa.body = Some(r#"{"tried":"batch writes","because":"reorders events"}"#.into());
        fa.confidence = Some(0.4);
        originals.push(fa);

        let mut oq = mk(
            LedgerKind::OpenQuestion,
            "led_rt_oq",
            "what does constant 4096 mean?",
        );
        oq.body = Some("partial finding: it's a page size somewhere".into());
        originals.push(oq);

        // One representative per non-thinking class, with confidence
        // set where it previously vanished on import.
        let mut dec = mk(LedgerKind::Decision, "led_rt_dec", "chose sqlite");
        dec.confidence = Some(0.9);
        originals.push(dec);
        let mut own = mk(LedgerKind::Ownership, "led_rt_own", "owned by core team");
        own.role = Some("fixture-path".into());
        originals.push(own);
        let mut map = mk(LedgerKind::Mapping, "led_rt_map", "coverage moved");
        map.body = Some(r#"{"from_qname":"a","to_qname":"b"}"#.into());
        originals.push(map);
        originals.push(mk(LedgerKind::Hazard, "led_rt_haz", "races under load"));
        let mut val = mk(
            LedgerKind::ValidationScenario,
            "led_rt_val",
            "replay the file",
        );
        val.command = Some("cargo test -p core".into());
        originals.push(val);
        let mut fu = mk(LedgerKind::FollowUp, "led_rt_fu", "migrate diagnostics");
        fu.supersedes = vec!["led_rt_old".into()];
        originals.push(fu);

        for e in &originals {
            ledger.append_entry(&engine.ref_name, e, "t").unwrap();
        }

        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();

        // "Wipe the ledger": fresh engine, same symbol, empty ledger.
        let engine2 = Engine::open_in_memory().unwrap();
        let sym = crate::schema::Symbol {
            symbol_id: "sym_order_test".into(),
            symbol_fp: "fp".into(),
            qname: "pkg.target".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/target.py".into(),
            start: crate::schema::Position { line: 1, col: 0 },
            end: crate::schema::Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        AsgIndexStore::from_engine(&engine2)
            .put_symbol(&engine2.ref_name, &sym, "t")
            .unwrap();

        let results = import_all(&engine2, tmp.path(), "t").unwrap();

        // Counted, not silent: every class reports read == imported,
        // zero skips — including thinking.
        for r in &results {
            assert_eq!(
                r.read, r.imported,
                "class `{}` must import everything it reads; got {r:?}",
                r.class
            );
            assert_eq!(r.skipped_parse_error, 0, "class `{}`: {r:?}", r.class);
            assert_eq!(r.skipped_unknown_qname, 0, "class `{}`: {r:?}", r.class);
        }
        let thinking = results
            .iter()
            .find(|r| r.class == "thinking")
            .expect("thinking class must be reported");
        assert_eq!(
            thinking.imported, 4,
            "all four thinking kinds must import; got {thinking:?}"
        );
        let total: usize = results.iter().map(|r| r.imported).sum();
        assert_eq!(total, originals.len());

        // Field-level diff: every original entry must come back
        // byte-equal at the JSON level (entry_id, kind, summary, body,
        // confidence, tags, role, command, supersedes, evidence,
        // author, created_at).
        let imported = AsgLedgerStore::from_engine(&engine2)
            .list_entries(&engine2.ref_name, "sym_order_test")
            .unwrap();
        assert_eq!(imported.len(), originals.len());
        let by_id: std::collections::BTreeMap<String, &LedgerEntry> =
            imported.iter().map(|e| (e.entry_id.clone(), e)).collect();
        for orig in &originals {
            let got = by_id
                .get(&orig.entry_id)
                .unwrap_or_else(|| panic!("entry {} missing after import", orig.entry_id));
            assert_eq!(
                serde_json::to_value(orig).unwrap(),
                serde_json::to_value(got).unwrap(),
                "entry {} must round-trip losslessly",
                orig.entry_id
            );
        }
    }

    #[test]
    fn import_counts_thinking_and_unknown_kinds_as_reads() {
        // A future/unknown kind string must not import, but must show
        // up in `read` and `skipped_parse_error` so the drop is
        // visible. Hand-write a thinking.jsonl with one good and one
        // unknown-kind row.
        let (engine, _) = engine_with_decisions_in_order(&[]);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("thinking.jsonl"),
            concat!(
                r#"{"id":"led_ok","kind":"open_question","qname":"pkg.target","file":"src/target.py","summary":"q?","author":"agent:t","created_at":"2026-05-19T20:00:00+00:00"}"#,
                "\n",
                r#"{"id":"led_bad","kind":"from_the_future","qname":"pkg.target","file":"src/target.py","summary":"?","author":"agent:t","created_at":"2026-05-19T20:00:00+00:00"}"#,
                "\n",
            ),
        )
        .unwrap();

        let results = import_all(&engine, tmp.path(), "t").unwrap();
        let thinking = results
            .iter()
            .find(|r| r.class == "thinking")
            .expect("thinking reported");
        assert_eq!(thinking.read, 2);
        assert_eq!(thinking.imported, 1);
        assert_eq!(thinking.skipped_parse_error, 1);
        assert_eq!(
            thinking.read,
            thinking.imported + thinking.skipped_unknown_qname + thinking.skipped_parse_error,
            "read must equal imported + skips"
        );
    }

    #[test]
    fn confidence_survives_export_import_for_thinking_and_decisions() {
        // Focused regression for the confidence drop: export writes
        // `confidence`, import restores it (previously hardcoded None).
        let (engine, _) = engine_with_decisions_in_order(&[]);
        append_thinking_entry(
            &engine,
            LedgerKind::Hypothesis,
            "led_conf_hyp",
            "confident hypothesis",
            Some(0.75),
        );
        append_thinking_entry(
            &engine,
            LedgerKind::Decision,
            "led_conf_dec",
            "confident decision",
            Some(0.6),
        );

        let tmp = tempfile::tempdir().unwrap();
        export_all(&engine, tmp.path()).unwrap();

        // The serialized lines must carry the confidence field.
        let thinking_body = std::fs::read_to_string(tmp.path().join("thinking.jsonl")).unwrap();
        assert!(
            thinking_body.contains("\"confidence\":0.75"),
            "export must serialize confidence; got:\n{thinking_body}"
        );

        let engine2 = Engine::open_in_memory().unwrap();
        let sym = crate::schema::Symbol {
            symbol_id: "sym_order_test".into(),
            symbol_fp: "fp".into(),
            qname: "pkg.target".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "src/target.py".into(),
            start: crate::schema::Position { line: 1, col: 0 },
            end: crate::schema::Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        AsgIndexStore::from_engine(&engine2)
            .put_symbol(&engine2.ref_name, &sym, "t")
            .unwrap();
        import_all(&engine2, tmp.path(), "t").unwrap();

        let entries = AsgLedgerStore::from_engine(&engine2)
            .list_entries(&engine2.ref_name, "sym_order_test")
            .unwrap();
        let conf_of = |id: &str| {
            entries
                .iter()
                .find(|e| e.entry_id == id)
                .unwrap_or_else(|| panic!("{id} must import"))
                .confidence
        };
        assert_eq!(conf_of("led_conf_hyp"), Some(0.75));
        assert_eq!(conf_of("led_conf_dec"), Some(0.6));
    }

    fn rec(id: &str, summary: &str) -> ExportRecord {
        ExportRecord {
            id: id.into(),
            kind: "decision".into(),
            qname: "App.A".into(),
            file: "a.rs".into(),
            summary: summary.into(),
            body: None,
            confidence: None,
            role: None,
            command: None,
            tags: vec![],
            evidence: vec![],
            supersedes: vec![],
            author: "agent:c".into(),
            created_at: "2026-05-19T20:00:00+00:00".into(),
        }
    }

    #[test]
    fn merge_unions_dedups_and_sorts_by_id() {
        let tmp = tempdir().unwrap();
        let ours = tmp.path().join("ours.jsonl");
        let theirs = tmp.path().join("theirs.jsonl");
        // Out-of-order on purpose; `led_a` appears on both sides.
        write_jsonl(&ours, &[rec("led_b", "b"), rec("led_a", "a")]).unwrap();
        write_jsonl(&theirs, &[rec("led_a", "a"), rec("led_c", "c")]).unwrap();

        let n = merge_jsonl(&ours, &theirs).unwrap();
        assert_eq!(n, 3, "union of {{a,b}} and {{a,c}} is {{a,b,c}}");

        // Byte-identical to a fresh canonical export of the unioned set.
        let expected_path = tmp.path().join("expected.jsonl");
        write_jsonl(
            &expected_path,
            &[rec("led_a", "a"), rec("led_b", "b"), rec("led_c", "c")],
        )
        .unwrap();
        assert_eq!(
            std::fs::read(&ours).unwrap(),
            std::fs::read(&expected_path).unwrap(),
            "merge output must match canonical id-sorted export bytes"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let tmp = tempdir().unwrap();
        let ours = tmp.path().join("ours.jsonl");
        let theirs = tmp.path().join("theirs.jsonl");
        write_jsonl(&ours, &[rec("led_a", "a")]).unwrap();
        write_jsonl(&theirs, &[rec("led_b", "b")]).unwrap();
        merge_jsonl(&ours, &theirs).unwrap();
        let once = std::fs::read(&ours).unwrap();
        merge_jsonl(&ours, &theirs).unwrap();
        assert_eq!(once, std::fs::read(&ours).unwrap());
    }

    #[test]
    fn merge_treats_missing_side_as_empty() {
        let tmp = tempdir().unwrap();
        let ours = tmp.path().join("ours.jsonl");
        let missing = tmp.path().join("does_not_exist.jsonl");
        write_jsonl(&ours, &[rec("led_b", "b"), rec("led_a", "a")]).unwrap();
        let n = merge_jsonl(&ours, &missing).unwrap();
        assert_eq!(n, 2);
        // Still canonicalized (sorted) even with an absent counterpart.
        let expected = tmp.path().join("expected.jsonl");
        write_jsonl(&expected, &[rec("led_a", "a"), rec("led_b", "b")]).unwrap();
        assert_eq!(
            std::fs::read(&ours).unwrap(),
            std::fs::read(&expected).unwrap()
        );
    }
}

#[cfg(test)]
mod additive_export_tests {
    use super::*;
    use std::collections::HashSet;

    fn rec(id: &str, summary: &str) -> ExportRecord {
        ExportRecord {
            id: id.to_string(),
            kind: "decision".to_string(),
            qname: "pkg.thing".to_string(),
            file: "src/lib.rs".to_string(),
            summary: summary.to_string(),
            body: None,
            confidence: None,
            role: None,
            command: None,
            tags: Vec::new(),
            evidence: Vec::new(),
            supersedes: Vec::new(),
            author: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn committed(dir: &std::path::Path, recs: &[ExportRecord]) -> std::path::PathBuf {
        let p = dir.join("decisions.jsonl");
        write_jsonl(&p, recs).expect("seed committed file");
        p
    }

    #[test]
    fn preserves_a_record_this_clone_cannot_produce() {
        // The orphaned-symbol case: `led_…ApiError:237` existed in the ledger
        // but its `:line` qname stopped resolving, so `gather` skipped it and
        // the bare export deleted it from main. This is that regression.
        let d = tempfile::tempdir().unwrap();
        let path = committed(
            d.path(),
            &[rec("led_orphan", "from another era"), rec("led_a", "old")],
        );
        let fresh = vec![rec("led_a", "current")];

        let out = merge_preserving(&fresh, &path, &HashSet::new()).unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["led_a", "led_orphan"], "orphan was dropped");
        // Fresh wins for a record this clone CAN see.
        assert_eq!(out[0].summary, "current");
    }

    #[test]
    fn does_not_resurrect_a_superseded_record() {
        // The trap an unconditional union would fall into.
        let d = tempfile::tempdir().unwrap();
        let path = committed(
            d.path(),
            &[rec("led_old", "superseded"), rec("led_new", "current")],
        );
        let fresh = vec![rec("led_new", "current")];
        let retired: HashSet<String> = ["led_old".to_string()].into_iter().collect();

        let out = merge_preserving(&fresh, &path, &retired).unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["led_new"],
            "a deliberately retired record came back"
        );
    }

    #[test]
    fn does_not_resurrect_a_below_floor_hypothesis() {
        // Same rule, other deliberate exclusion: a hypothesis whose confidence
        // dropped under DEFAULT_CONFIDENCE_FLOOR must stop shipping.
        let d = tempfile::tempdir().unwrap();
        let path = committed(d.path(), &[rec("led_weak", "speculative")]);
        let retired: HashSet<String> = ["led_weak".to_string()].into_iter().collect();

        let out = merge_preserving(&[], &path, &retired).unwrap();
        assert!(
            out.is_empty(),
            "below-floor hypothesis resurrected: {out:?}"
        );
    }

    #[test]
    fn output_stays_id_sorted_and_deduped() {
        // gather_buckets and merge_jsonl both maintain an id-sort; a merge that
        // broke it would make every subsequent export a huge spurious diff.
        let d = tempfile::tempdir().unwrap();
        let path = committed(d.path(), &[rec("led_c", "c"), rec("led_a", "a")]);
        let fresh = vec![rec("led_b", "b"), rec("led_a", "a2")];

        let out = merge_preserving(&fresh, &path, &HashSet::new()).unwrap();
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["led_a", "led_b", "led_c"]);
        assert_eq!(out.len(), 3, "duplicate ids survived");
        assert_eq!(out[0].summary, "a2", "fresh should win on conflict");
    }

    #[test]
    fn a_missing_committed_file_is_not_an_error() {
        // First export in a fresh clone: nothing to preserve, and that is fine.
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("decisions.jsonl");
        let out = merge_preserving(&[rec("led_a", "a")], &path, &HashSet::new()).unwrap();
        assert_eq!(out.len(), 1);
    }
}
