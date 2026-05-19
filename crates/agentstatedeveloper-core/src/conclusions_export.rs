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

use serde::Serialize;

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::paths::ASD_ROOT;
use crate::schema::{AuthorKind, ConclusionClass};

/// Compact JSONL record. `preserve_order` is on for serde_json in this
/// workspace so field order is stable across runs.
#[derive(Debug, Clone, Serialize)]
pub struct ExportRecord {
    pub id: String,
    pub kind: &'static str,
    pub qname: String,
    pub file: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
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
        let entries = ledger.list_entries(&ref_name, &sym.symbol_id).unwrap_or_default();
        for entry in entries {
            let class = entry.kind.conclusion_class();
            let stem = class.filename_stem();
            let record = ExportRecord {
                id: entry.entry_id,
                kind: entry.kind.as_str(),
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
        let line = serde_json::to_string(rec).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_jsonl_is_byte_stable_across_runs() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("decisions.jsonl");
        let recs = vec![
            ExportRecord {
                id: "led_1".into(),
                kind: "decision",
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
            },
        ];
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
            kind: "decision",
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
}
