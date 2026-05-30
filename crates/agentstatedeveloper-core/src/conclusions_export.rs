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
use crate::schema::{AuthorKind, ConclusionClass};

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
/// bucket is sorted by (created_at, id) so re-export is byte-identical
/// when no new entries are added.
pub fn gather_buckets(
    engine: &Engine,
) -> std::io::Result<BTreeMap<&'static str, Vec<ExportRecord>>> {
    let index = AsgIndexStore::from_engine(engine);
    let ledger = AsgLedgerStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    let prefix = format!("{}/index/by-qname", ASD_ROOT);
    let tree = engine
        .repo
        .get_tree(&ref_name, &prefix)
        .unwrap_or(serde_json::Value::Null);
    let mut qnames: Vec<String> = match tree {
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        _ => Vec::new(),
    };
    qnames.sort();

    let mut buckets: BTreeMap<&'static str, Vec<ExportRecord>> = BTreeMap::new();
    for class in ConclusionClass::all() {
        buckets.insert(class.filename_stem(), Vec::new());
    }

    for qn in qnames {
        let sym = match index.get_symbol_by_qname(&ref_name, &qn) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let entries = ledger
            .list_entries(&ref_name, &sym.symbol_id)
            .unwrap_or_default();
        for entry in entries {
            let class = entry.kind.conclusion_class();
            let stem = class.filename_stem();
            let record = ExportRecord {
                id: entry.entry_id,
                kind: entry.kind.as_str().to_string(),
                qname: sym.qname.clone(),
                file: sym.file.clone(),
                summary: entry.summary,
                body: entry.body,
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

    for bucket in buckets.values_mut() {
        bucket.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    Ok(buckets)
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

/// One-shot: gather + write all six files under `out_dir`. Returns per-class
/// (stem, entry_count, byte_count).
pub fn export_all(
    engine: &Engine,
    out_dir: &Path,
) -> std::io::Result<Vec<(&'static str, usize, u64)>> {
    std::fs::create_dir_all(out_dir)?;
    let buckets = gather_buckets(engine)?;
    let mut out = Vec::with_capacity(6);
    for class in ConclusionClass::all() {
        let stem = class.filename_stem();
        let path = out_dir.join(format!("{stem}.jsonl"));
        let entries = buckets.get(stem).cloned().unwrap_or_default();
        let bytes = write_jsonl(&path, &entries)?;
        out.push((stem, entries.len(), bytes));
    }
    Ok(out)
}

fn author_kind_str(k: AuthorKind) -> &'static str {
    match k {
        AuthorKind::Agent => "agent",
        AuthorKind::Human => "human",
    }
}

// -- Import (Plan B t-005) ---------------------------------------------------

use crate::schema::{Author, Evidence, LedgerEntry, LedgerKind};
use chrono::{DateTime, Utc};

/// Outcome of importing one JSONL file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportFileResult {
    pub class: &'static str,
    pub file: String,
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
pub fn import_all(
    engine: &Engine,
    in_dir: &Path,
    agent_id: &str,
) -> std::io::Result<Vec<ImportFileResult>> {
    let mut out = Vec::with_capacity(6);
    for class in ConclusionClass::all() {
        let stem = class.filename_stem();
        let path = in_dir.join(format!("{stem}.jsonl"));
        out.push(import_one(engine, &path, stem, agent_id)?);
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
        confidence: None,
        evidence,
        supersedes: rec.supersedes,
        created_at,
        tags: rec.tags,
        matched_policy: None,
        role: rec.role,
        command: rec.command,
    })
}

fn parse_kind(s: &str) -> Option<LedgerKind> {
    match s {
        "decision" => Some(LedgerKind::Decision),
        "assumption" => Some(LedgerKind::Assumption),
        "constraint" => Some(LedgerKind::Constraint),
        "rationale" => Some(LedgerKind::Rationale),
        "hazard" => Some(LedgerKind::Hazard),
        "tradeoff" => Some(LedgerKind::Tradeoff),
        "invariant" => Some(LedgerKind::Invariant),
        "ownership" => Some(LedgerKind::Ownership),
        "proof" => Some(LedgerKind::Proof),
        "validation_scenario" => Some(LedgerKind::ValidationScenario),
        "known_bug" => Some(LedgerKind::KnownBug),
        "concept" => Some(LedgerKind::Concept),
        "mapping" => Some(LedgerKind::Mapping),
        "follow_up" => Some(LedgerKind::FollowUp),
        _ => None,
    }
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
}
