//! `asd map` core — initial-read project summary.
//!
//! Walks the indexed project, identifies package boundaries from directory
//! structure, and classifies test files into `fast-test` vs `diagnostic-test`
//! per file-name and body heuristics. Results land as `Ownership` ledger
//! entries so the next session inherits the project mental model without
//! re-deriving it. Idempotent: entry IDs are deterministic hashes of
//! (symbol_id + role), so re-running overwrites prior tags.
//!
//! Extracted from the CLI so both `asd map` and the `map` MCP tool share one
//! implementation: [`run_map`] returns the summary payload and (unless
//! `dry_run`) writes the ledger entries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::json;

use crate::error::Result;

use crate::{
    ASD_PATH_PREFIX, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore,
    LedgerEntry, LedgerKind, LedgerStore, RoleTag, Symbol,
};

/// Build the initial-read project map and (unless `dry_run`) write the
/// Ownership ledger entries. Returns the JSON summary payload. `agent_id`
/// attributes the writes; `db_parent` is used to pick up the active CTX task
/// id for provenance tagging.
pub fn run_map(
    engine: &Engine,
    agent_id: &str,
    db_parent: Option<&Path>,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let index = AsgIndexStore::from_engine(engine);
    let ledger = AsgLedgerStore::from_engine(engine);
    let ref_name = engine.ref_name.clone();

    // Collect every indexed symbol.
    let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
    let tree = engine
        .repo
        .get_tree(&ref_name, &prefix)
        .unwrap_or(serde_json::Value::Null);
    let qnames: Vec<String> = match tree {
        serde_json::Value::Object(m) => m.keys().cloned().collect(),
        _ => Vec::new(),
    };

    let mut all_symbols: Vec<Symbol> = Vec::new();
    for qn in qnames {
        if let Ok(Some(sym)) = index.get_symbol_by_qname(&ref_name, &qn) {
            all_symbols.push(sym);
        }
    }

    // Group by package — the directory of the file. For each package, pick a
    // "front-door" symbol (the one with the shortest qname).
    let mut package_front_doors: BTreeMap<String, Symbol> = BTreeMap::new();
    for sym in &all_symbols {
        if !is_package_member(&sym.file) {
            continue;
        }
        let pkg = package_dir(&sym.file);
        package_front_doors
            .entry(pkg)
            .and_modify(|existing| {
                if sym.qname.len() < existing.qname.len() {
                    *existing = sym.clone();
                }
            })
            .or_insert_with(|| sym.clone());
    }

    // Classify test files (one symbol per file is enough for the role tag).
    let mut test_file_roles: BTreeMap<String, (Symbol, RoleTag)> = BTreeMap::new();
    for sym in &all_symbols {
        if let Some(role) = test_file_role(&sym.file) {
            test_file_roles
                .entry(sym.file.clone())
                .and_modify(|(s, _)| {
                    if sym.qname.len() < s.qname.len() {
                        *s = sym.clone();
                    }
                })
                .or_insert_with(|| (sym.clone(), role));
        }
    }

    // Build the summary payload + write entries (unless dry-run).
    let mut written_pkg = 0usize;
    let mut written_test = 0usize;
    let mut packages_out: Vec<serde_json::Value> = Vec::new();
    let mut test_files_out: Vec<serde_json::Value> = Vec::new();
    let author = Author {
        kind: AuthorKind::Agent,
        id: agent_id.to_string(),
    };

    // Pick up the active CTX task id once, pass to every write so the
    // provenance tag lands consistently.
    let ctx_task_id = read_active_ctx_task_id(db_parent);

    for (pkg, sym) in &package_front_doors {
        packages_out.push(json!({
            "package": pkg,
            "front_door_qname": sym.qname,
            "file": sym.file,
            "role": RoleTag::PackageBoundary.as_str(),
        }));
        if !dry_run {
            write_map_entry(
                &ledger,
                &ref_name,
                sym,
                RoleTag::PackageBoundary,
                &format!("package boundary: {pkg}"),
                &author,
                ctx_task_id.as_deref(),
            )?;
            written_pkg += 1;
        }
    }

    for (file, (sym, role)) in &test_file_roles {
        test_files_out.push(json!({
            "file": file,
            "qname": sym.qname,
            "role": role.as_str(),
        }));
        if !dry_run {
            let summary = match role {
                RoleTag::FastTest => format!("fast test: {file}"),
                RoleTag::DiagnosticTest => format!("diagnostic test (env-gate before CI): {file}"),
                _ => format!("test: {file}"),
            };
            write_map_entry(
                &ledger,
                &ref_name,
                sym,
                *role,
                &summary,
                &author,
                ctx_task_id.as_deref(),
            )?;
            written_test += 1;
        }
    }

    let pkg_set: BTreeSet<&String> = package_front_doors.keys().collect();

    Ok(json!({
        "intent": "asd-map",
        "dry_run": dry_run,
        "indexed_symbols": all_symbols.len(),
        "packages": packages_out,
        "test_files": test_files_out,
        "summary": {
            "package_count": pkg_set.len(),
            "test_file_count": test_file_roles.len(),
            "entries_written": written_pkg + written_test,
        },
    }))
}

fn write_map_entry(
    ledger: &AsgLedgerStore,
    ref_name: &str,
    sym: &Symbol,
    role: RoleTag,
    summary: &str,
    author: &Author,
    ctx_task_id: Option<&str>,
) -> Result<()> {
    let mut entry = LedgerEntry::new(
        &sym.symbol_id,
        LedgerKind::Ownership,
        summary,
        author.clone(),
    );
    // Deterministic entry id so re-running `asd map` overwrites instead of
    // appending duplicates.
    entry.entry_id = deterministic_entry_id(&sym.symbol_id, role.as_str());
    entry.role = Some(role.as_str().to_string());
    entry.tags.push("plan-c:asd-map".to_string());
    // Auto-tag with the active CTX task id so future scope-by-task filtering /
    // audits can attribute the entries to the gesture that wrote them.
    if let Some(id) = ctx_task_id {
        let tag = format!("ctx:task:{id}");
        if !entry.tags.iter().any(|t| t == &tag) {
            entry.tags.push(tag);
        }
    }
    ledger.append_entry(ref_name, &entry, &author.id)?;
    Ok(())
}

/// Read the active CTX task id from `CTX_ACTIVE_TASK` env var (JSON:
/// `{"task_id": "..."}`) with a fallback to `.asd/cache/active-task.json`.
fn read_active_ctx_task_id(db_parent: Option<&Path>) -> Option<String> {
    let raw = std::env::var("CTX_ACTIVE_TASK").ok().or_else(|| {
        let p = db_parent?
            .join(".asd")
            .join("cache")
            .join("active-task.json");
        std::fs::read_to_string(p).ok()
    })?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("task_id")?.as_str().map(String::from)
}

fn deterministic_entry_id(sym_id: &str, role: &str) -> String {
    let key = format!("asd-map:{sym_id}:{role}");
    let h = blake3::hash(key.as_bytes()).to_hex();
    let short: String = h.chars().take(24).collect();
    format!("led_map_{short}")
}

/// True when this file lives inside a recognizable language package. Used to
/// scope which files seed a package-boundary tag.
fn is_package_member(file: &str) -> bool {
    // Conservative: any file with at least one path separator (root files skip).
    file.contains('/')
}

/// Directory portion of `file`, used as the package key. Keeps the leaf
/// directory (not the package NAME) because that round-trips to file globs.
fn package_dir(file: &str) -> String {
    match file.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => String::new(),
    }
}

/// Body markers that flip a test file from `fast-test` → `diagnostic-test`
/// when present: tests that touch the real filesystem, render full songs,
/// batch-run, or take long are diagnostic by nature.
const DIAGNOSTIC_BODY_MARKERS: &[&str] = &[
    "FileManager.default.fileExists",
    "FileManager.fileExists",
    "renderFullSong",
    ".trace(",
    "batchRender",
    "durationSeconds",
    "/Users/",  // hard-coded paths to user dirs
    "tmp_path", // pytest real-fs fixtures
    "tempfile.NamedTemporaryFile",
    "subprocess.run",
];

/// Read the file's first ~64 KiB and check whether any diagnostic-body marker
/// appears. Errors and missing files return false (body-sniff is opt-in).
fn body_looks_diagnostic(file: &str) -> bool {
    let path = Path::new(file);
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let limit = bytes.len().min(64 * 1024);
    let head = match std::str::from_utf8(&bytes[..limit]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    DIAGNOSTIC_BODY_MARKERS.iter().any(|m| head.contains(m))
}

/// Classify a file as a test file and return its role tag, or None if the file
/// isn't a test. Combines filename heuristics with optional body-sniff.
fn test_file_role(file: &str) -> Option<RoleTag> {
    let lower = file.to_lowercase();
    let is_test = lower.contains("/test")
        || lower.contains("_test.")
        || lower.contains(".test.")
        || lower.contains("/spec/")
        || lower.ends_with("_spec.rb");
    if !is_test {
        return None;
    }
    let diagnostic_by_name = lower.contains("diagnostic")
        || lower.contains("integration")
        || lower.contains("e2e")
        || lower.contains("real_file")
        || lower.contains("slow");
    let diagnostic = diagnostic_by_name || body_looks_diagnostic(file);
    Some(if diagnostic {
        RoleTag::DiagnosticTest
    } else {
        RoleTag::FastTest
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_role_classifies_filenames() {
        assert_eq!(
            test_file_role("tests/fast_test.py"),
            Some(RoleTag::FastTest)
        );
        assert_eq!(
            test_file_role("tests/diagnostic_real_file_test.py"),
            Some(RoleTag::DiagnosticTest)
        );
        assert_eq!(
            test_file_role("tests/integration_test.py"),
            Some(RoleTag::DiagnosticTest)
        );
        assert_eq!(test_file_role("src/pkg/module.py"), None);
    }

    #[test]
    fn body_looks_diagnostic_matches_session_drift_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("foo_test.py");
        std::fs::write(
            &path,
            "import subprocess\nsubprocess.run(['swift', 'test'])\n",
        )
        .unwrap();
        assert!(body_looks_diagnostic(path.to_str().unwrap()));
    }

    #[test]
    fn body_looks_diagnostic_false_for_clean_unit_test() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("foo_test.py");
        std::fs::write(&path, "def test_addition():\n    assert 1 + 1 == 2\n").unwrap();
        assert!(!body_looks_diagnostic(path.to_str().unwrap()));
    }

    #[test]
    fn body_looks_diagnostic_false_for_missing_file() {
        assert!(!body_looks_diagnostic(
            "/nonexistent/path/that/cannot/exist.py"
        ));
    }

    #[test]
    fn read_active_ctx_task_id_extracts_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".asd/cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("active-task.json"),
            r#"{"task_id":"t-007","scope":["x/**"]}"#,
        )
        .unwrap();
        assert_eq!(
            read_active_ctx_task_id(Some(tmp.path())).as_deref(),
            Some("t-007")
        );
    }

    #[test]
    fn read_active_ctx_task_id_returns_none_when_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_active_ctx_task_id(Some(tmp.path())), None);
    }

    #[test]
    fn test_file_role_promotes_clean_test_to_diagnostic_on_body_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fast_test.py");
        std::fs::write(
            &path,
            "import subprocess\ndef test_ok():\n    subprocess.run(['ls'])\n",
        )
        .unwrap();
        assert_eq!(
            test_file_role(path.to_str().unwrap()),
            Some(RoleTag::DiagnosticTest)
        );
    }

    #[test]
    fn deterministic_entry_id_is_stable() {
        let a = deterministic_entry_id("sym_x", "fast-test");
        let b = deterministic_entry_id("sym_x", "fast-test");
        assert_eq!(a, b);
        assert!(a.starts_with("led_map_"));
    }

    #[test]
    fn deterministic_entry_id_changes_with_role() {
        let a = deterministic_entry_id("sym_x", "fast-test");
        let b = deterministic_entry_id("sym_x", "diagnostic-test");
        assert_ne!(a, b);
    }

    #[test]
    fn package_dir_returns_parent_directory() {
        assert_eq!(package_dir("src/pkg/mod.py"), "src/pkg");
        assert_eq!(package_dir("root.py"), "");
    }
}
