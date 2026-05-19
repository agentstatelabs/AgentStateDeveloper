//! `asd references <name>` — exact-symbol reference search with rg parity.
//!
//! ASD's index stores one row per symbol definition; it does not index
//! per-occurrence call sites or text references. For literal "find every line
//! that mentions this identifier" queries (the rg use case) we shell out to
//! `rg --json --fixed-strings --word-regexp <name>`. We then merge the
//! definition record(s) from the index so the caller gets both:
//!
//!   - `definitions`: canonical Symbol records (with effects/ledger context)
//!   - `occurrences`: every literal text occurrence in the worktree
//!
//! This is intentionally precision-over-recall: no tokenization, no CamelCase
//! splitting, no BM25 reranking. If `rg` is not on PATH, returns an error
//! pointing the user at installation — we do not silently degrade to a slower
//! Rust walker (Plan B may add a native fallback).
//!
//! See Plan A, t-004.

use std::path::PathBuf;
use std::process::Command as Proc;

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Symbol,
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct ReferencesArgs {
    /// Symbol name to find references for. Pass a qname (e.g. `pkg.mod.Type`)
    /// for exact definition lookup, or a bare identifier (e.g. `MasterBusParams`)
    /// to match any symbol whose qname ends with `.<name>` plus all text
    /// occurrences.
    pub name: String,

    /// Project root to search. Defaults to the current directory.
    /// `rg` respects `.gitignore` from this directory downward.
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// Cap the number of occurrences returned. 0 = unlimited.
    #[arg(long, default_value = "500")]
    pub limit: usize,

    /// Restrict the rg scan to files matching this glob (passed through
    /// as `rg --glob`). Repeatable.
    #[arg(long = "glob")]
    pub globs: Vec<String>,
}

pub fn run(cfg: &Config, args: ReferencesArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index = AsgIndexStore::from_engine(&engine);

    let definitions = lookup_definitions(&engine, &index, &args.name)?;

    let search_root = args
        .path
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let rg_result = rg_occurrences(&search_root, &args.name, &args.globs, args.limit);

    let (occurrences, scan_status) = match rg_result {
        Ok(occ) => (occ, "ok"),
        Err(e) => (Vec::new(), Box::leak(e.to_string().into_boxed_str()) as &str),
    };

    let out = json!({
        "name": args.name,
        "search_root": search_root.display().to_string(),
        "definitions": definitions,
        "occurrences": occurrences,
        "occurrence_count": occurrences.len(),
        "limit": args.limit,
        "scan": {
            "tool": "rg",
            "status": scan_status,
            "flags": ["--fixed-strings", "--word-regexp"],
        },
        "confidence": "exact-literal text match via rg + index definition lookup; no fuzzy/BM25 ranking",
    });

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Resolve the user's `name` into one or more canonical Symbol records.
/// - If `name` contains `.`, treat as a qname for direct lookup.
/// - Otherwise scan the qname index and collect symbols whose qname equals
///   `name` or ends with `.name` (case-sensitive — matches rg's default).
fn lookup_definitions(
    engine: &Engine,
    index: &AsgIndexStore,
    name: &str,
) -> Result<Vec<Symbol>> {
    if name.contains('.') {
        return Ok(index
            .get_symbol_by_qname(&engine.ref_name, name)?
            .into_iter()
            .collect());
    }

    let prefix = format!("{}/index/by-qname", agentstatedeveloper_core::paths::ASD_ROOT);
    let tree = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let qnames: Vec<String> = match tree {
        Value::Object(map) => map.keys().cloned().collect(),
        _ => return Ok(Vec::new()),
    };

    let needle_dot = format!(".{name}");
    let mut out = Vec::new();
    for qn in qnames {
        let matches = qn == name || qn.ends_with(&needle_dot);
        if !matches {
            continue;
        }
        if let Some(sym) = index.get_symbol_by_qname(&engine.ref_name, &qn)? {
            out.push(sym);
        }
    }
    out.sort_by(|a, b| a.qname.cmp(&b.qname));
    Ok(out)
}

/// Shell out to `rg --json --fixed-strings --word-regexp <name>` and parse
/// the JSON event stream into a flat occurrence list.
fn rg_occurrences(
    root: &std::path::Path,
    name: &str,
    globs: &[String],
    limit: usize,
) -> Result<Vec<Value>> {
    let mut cmd = Proc::new("rg");
    cmd.arg("--json")
        .arg("--fixed-strings")
        .arg("--word-regexp")
        .arg("--no-messages");
    for g in globs {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(name).arg(".");
    cmd.current_dir(root);

    let output = cmd
        .output()
        .with_context(|| "failed to spawn `rg`. Install ripgrep (https://github.com/BurntSushi/ripgrep) or skip this command.")?;

    // rg exits 1 when there are no matches — that is not a failure for us.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut occurrences = Vec::new();
    for line in stdout.lines() {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let data = match v.get("data") {
            Some(d) => d,
            None => continue,
        };
        let path = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let line_no = data.get("line_number").and_then(|n| n.as_u64()).unwrap_or(0);
        let text = data
            .get("lines")
            .and_then(|l| l.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string();
        let submatches: Vec<Value> = data
            .get("submatches")
            .and_then(|s| s.as_array())
            .map(|arr| arr.clone())
            .unwrap_or_default();
        let columns: Vec<u64> = submatches
            .iter()
            .filter_map(|sm| sm.get("start").and_then(|s| s.as_u64()))
            .collect();
        occurrences.push(json!({
            "file": path,
            "line": line_no,
            "columns": columns,
            "text": text,
        }));
        if limit > 0 && occurrences.len() >= limit {
            break;
        }
    }
    Ok(occurrences)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rg_scan_finds_literal_occurrences_in_self() {
        // Skip silently if rg isn't on PATH — we don't want CI to fail on
        // environments without it. Real coverage runs locally.
        if Proc::new("rg").arg("--version").output().is_err() {
            return;
        }
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest.parent().unwrap().parent().unwrap();
        let hits = rg_occurrences(workspace_root, "ReferencesArgs", &[], 50).unwrap();
        assert!(
            hits.iter()
                .any(|h| h.get("file").and_then(|f| f.as_str()).unwrap_or("").ends_with("references.rs")),
            "expected at least one ReferencesArgs hit in references.rs; got {hits:?}"
        );
    }

    #[test]
    fn rg_word_boundary_avoids_substrings() {
        if Proc::new("rg").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "foo\nfoobar\nfoo_bar\n").unwrap();
        let hits = rg_occurrences(tmp.path(), "foo", &[], 50).unwrap();
        // --word-regexp matches "foo" on its own line and "foo_bar" (underscore
        // is a word char), but NOT "foobar".
        let lines: Vec<u64> = hits
            .iter()
            .filter_map(|h| h.get("line").and_then(|l| l.as_u64()))
            .collect();
        assert!(lines.contains(&1));
        assert!(!lines.contains(&2), "foobar should not match word `foo`; got {hits:?}");
        assert!(lines.contains(&3));
    }
}
