//! `asd conclusions list [--class <class>] [--symbol <qname>]` — view ledger
//! entries bucketed by the six Plan B conclusion classes (decisions,
//! classifications, mappings, hazards, recipes, followups).
//!
//! This is the read surface for the conclusion layer. Write happens through
//! `asd ledger append` with the appropriate --kind (and optional --role /
//! --command flags). Export to JSONL is t-004; round-trip import is t-005.

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;

use agentstatedeveloper_core::{
    AsgIndexStore, ConclusionClass, Engine, IndexStore, LedgerKind,
    conclusions_export::{self},
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum ConclusionsCmd {
    /// List ledger entries grouped by conclusion class.
    List(ListArgs),
    /// Write all ledger conclusions to compact JSONL files under
    /// `.asd/conclusions/` (one file per class). Byte-stable when no new
    /// entries — safe to run from a pre-commit hook.
    Export(ExportArgs),
    /// Read `.asd/conclusions/*.jsonl` back into the local ledger.
    /// Idempotent — entries are keyed by entry_id. Use after `git pull`
    /// or on a fresh clone to populate ASG with the committed conclusions.
    Import(ImportArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Restrict to one conclusion class. Omit to list all six.
    #[arg(long, value_enum)]
    pub class: Option<CliConclusionClass>,

    /// Restrict to one symbol (qname). Omit to list across all symbols.
    #[arg(long)]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum CliConclusionClass {
    Decisions,
    Classifications,
    Mappings,
    Hazards,
    Recipes,
    #[value(name = "followups")]
    FollowUps,
}

impl From<CliConclusionClass> for ConclusionClass {
    fn from(c: CliConclusionClass) -> Self {
        match c {
            CliConclusionClass::Decisions => ConclusionClass::Decisions,
            CliConclusionClass::Classifications => ConclusionClass::Classifications,
            CliConclusionClass::Mappings => ConclusionClass::Mappings,
            CliConclusionClass::Hazards => ConclusionClass::Hazards,
            CliConclusionClass::Recipes => ConclusionClass::Recipes,
            CliConclusionClass::FollowUps => ConclusionClass::FollowUps,
        }
    }
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Output directory for the JSONL files. Defaults to `.asd/conclusions/`
    /// relative to the database parent directory.
    #[arg(long)]
    pub out: Option<std::path::PathBuf>,

    /// Emit a one-line summary instead of full per-class counts.
    #[arg(long)]
    pub quiet: bool,

    /// Plan K t-008: after export, verify total + per-shard sizes
    /// against `.asd/config.toml` budgets (defaults: 1 MiB total,
    /// 200 KiB per shard). Exits non-zero on violation.
    #[arg(long)]
    pub check_budget: bool,

    /// Pairs with `--check-budget`. Warn on violation instead of
    /// exiting non-zero — for CI gates that should surface drift
    /// without failing the build.
    #[arg(long, requires = "check_budget")]
    pub soft: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Input directory containing the `*.jsonl` files. Defaults to
    /// `.asd/conclusions/` relative to the database parent directory.
    #[arg(long, name = "in")]
    pub in_dir: Option<std::path::PathBuf>,
}

pub fn run(cfg: &Config, cmd: ConclusionsCmd) -> Result<()> {
    match cmd {
        ConclusionsCmd::List(args) => list(cfg, args),
        ConclusionsCmd::Export(args) => export(cfg, args),
        ConclusionsCmd::Import(args) => import(cfg, args),
    }
}

fn import(cfg: &Config, args: ImportArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let in_dir = args.in_dir.unwrap_or_else(|| {
        cfg.db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(".asd")
            .join("conclusions")
    });
    let results = conclusions_export::import_all(&engine, &in_dir, &cfg.agent_id)?;
    let payload = json!({
        "in_dir": in_dir.display().to_string(),
        "files": results.iter().map(|r| json!({
            "class": r.class,
            "file": r.file,
            "read": r.read,
            "imported": r.imported,
            "skipped_unknown_qname": r.skipped_unknown_qname,
            "skipped_parse_error": r.skipped_parse_error,
        })).collect::<Vec<_>>(),
        "total_read": results.iter().map(|r| r.read).sum::<usize>(),
        "total_imported": results.iter().map(|r| r.imported).sum::<usize>(),
        "total_skipped_unknown_qname": results.iter().map(|r| r.skipped_unknown_qname).sum::<usize>(),
        "total_skipped_parse_error": results.iter().map(|r| r.skipped_parse_error).sum::<usize>(),
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    // Plan T t-007: skips are no longer silent — anything read but not
    // imported gets a stderr warning with the per-class breakdown.
    let dropped: Vec<String> = results
        .iter()
        .filter(|r| r.skipped_unknown_qname + r.skipped_parse_error > 0)
        .map(|r| {
            format!(
                "{}: read {}, imported {}, skipped {} (unknown qname {}, parse error {})",
                r.class,
                r.read,
                r.imported,
                r.skipped_unknown_qname + r.skipped_parse_error,
                r.skipped_unknown_qname,
                r.skipped_parse_error
            )
        })
        .collect();
    if !dropped.is_empty() {
        eprintln!(
            "warning: conclusions import skipped entries:\n  {}",
            dropped.join("\n  ")
        );
    }
    Ok(())
}

fn list(cfg: &Config, args: ListArgs) -> Result<()> {
    use std::collections::{BTreeMap, HashSet};
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index = AsgIndexStore::from_engine(&engine);
    let ref_name = engine.ref_name.clone();

    let target_class = args.class.map(ConclusionClass::from);

    // If a specific symbol was requested, resolve it up front so we can
    // both validate it and restrict the ledger walk to its id.
    let target_symbol_id: Option<String> = if let Some(qname) = args.symbol.as_deref() {
        match index.get_symbol_by_qname(&ref_name, qname)? {
            Some(sym) => Some(sym.symbol_id),
            None => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "class": target_class.map(|c| c.filename_stem()),
                        "symbol": qname,
                        "buckets": {},
                        "warning": "symbol not found",
                    }))?
                );
                return Ok(());
            }
        }
    } else {
        None
    };

    let mut buckets: BTreeMap<&'static str, Vec<serde_json::Value>> = BTreeMap::new();
    for class in ConclusionClass::all() {
        if target_class.is_none() || target_class == Some(*class) {
            buckets.insert(class.filename_stem(), Vec::new());
        }
    }

    // Drive the walk from the LEDGER tree, not the symbol index. The
    // previous version resolved every indexed qname and called
    // `list_entries` per symbol — for a symbol with no conclusions that
    // falls through to an authoritative git `get_json`, so the cost
    // scaled with total symbol count (97k+ git probes on a large repo,
    // >12s and climbing). The ledger tree has one child per symbol that
    // actually has an entry, so this is O(symbols_with_conclusions);
    // qname/file resolve via the cached id map. Mirrors the
    // `conclusions_export::gather_buckets` fix.
    let id_map = index.build_id_map(&engine);
    let ledger_tree = engine
        .repo
        .get_tree(&ref_name, "/asd/v1/ledger")
        .unwrap_or(serde_json::Value::Null);
    let by_symbol = match ledger_tree {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };

    for (sym_id, per_symbol) in &by_symbol {
        if let Some(ref target) = target_symbol_id {
            if sym_id != target {
                continue;
            }
        }
        // Orphaned ledger entry (symbol no longer indexed): skip, same
        // as the prior code which only surfaced entries for resolvable
        // qnames.
        let qname = match id_map.get(sym_id) {
            Some(s) => s.qname.clone(),
            None => continue,
        };
        let entries_map = match per_symbol {
            serde_json::Value::Object(m) => m,
            _ => continue,
        };
        // Parse + apply the same superseded filter and newest-first sort
        // that `LedgerStore::list_entries` applied.
        let mut entries: Vec<agentstatedeveloper_core::LedgerEntry> = entries_map
            .values()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        let superseded: HashSet<String> = entries
            .iter()
            .flat_map(|e| e.supersedes.iter().cloned())
            .collect();
        entries.retain(|e| !superseded.contains(&e.entry_id));
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        for entry in entries {
            let class = entry.kind.conclusion_class();
            if let Some(filter) = target_class {
                if class != filter {
                    continue;
                }
            }
            let stem = class.filename_stem();
            if let Some(bucket) = buckets.get_mut(stem) {
                bucket.push(json!({
                    "entry_id": entry.entry_id,
                    "kind": kind_str(entry.kind),
                    "qname": qname,
                    "symbol_id": entry.symbol_id,
                    "summary": entry.summary,
                    "role": entry.role,
                    "command": entry.command,
                    "tags": entry.tags,
                    "created_at": entry.created_at,
                }));
            }
        }
    }

    let total: usize = buckets.values().map(|v| v.len()).sum();
    let payload = json!({
        "class": target_class.map(|c| c.filename_stem()),
        "symbol": args.symbol,
        "total": total,
        "buckets": buckets,
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn kind_str(k: LedgerKind) -> &'static str {
    k.as_str()
}

// -- Export ------------------------------------------------------------------
//
// The actual walk/serialize/write helpers live in
// `agentstatedeveloper_core::conclusions_export` so the MCP `conclusions_export`
// tool can call the same code without duplication.

fn export(cfg: &Config, args: ExportArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let out_dir = args.out.unwrap_or_else(|| {
        cfg.db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(".asd")
            .join("conclusions")
    });

    let counts = conclusions_export::export_all(&engine, &out_dir)?;

    // Plan K t-008: optionally enforce the size budget after writing.
    // Resolve the project root the same way export_all does (out_dir's
    // grandparent) so the budget read picks up the same .asd/config.toml
    // the layout choice did.
    let budget_report = if args.check_budget {
        let project_root = out_dir
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        let cfg_sidecar =
            agentstatedeveloper_core::sidecar_config::SidecarConfig::load_from_project(
                project_root,
            );
        let report = conclusions_export::check_budget(&out_dir, &cfg_sidecar.budget)?;
        Some(report)
    } else {
        None
    };

    if args.quiet {
        let total_entries: usize = counts.iter().map(|(_, n, _)| n).sum();
        let total_bytes: u64 = counts.iter().map(|(_, _, b)| b).sum();
        println!(
            "exported {} entries ({} bytes) to {}",
            total_entries,
            total_bytes,
            out_dir.display()
        );
    } else {
        let mut payload = json!({
            "out_dir": out_dir.display().to_string(),
            "files": counts.iter().map(|(stem, n, b)| json!({
                "class": stem,
                "file": format!("{stem}.jsonl"),
                "entries": n,
                "bytes": b,
            })).collect::<Vec<_>>(),
            "total_entries": counts.iter().map(|(_, n, _)| n).sum::<usize>(),
            "total_bytes": counts.iter().map(|(_, _, b)| b).sum::<u64>(),
        });
        if let Some(ref r) = budget_report {
            payload["budget"] = json!({
                "ok": r.ok,
                "total_bytes": r.total_bytes,
                "total_budget": r.total_budget,
                "over_total": r.over_total,
                "per_shard_budget": r.per_shard_budget,
                "shards": r.shards.iter().map(|s| json!({
                    "path": s.path,
                    "bytes": s.bytes,
                    "over_per_shard": s.over_per_shard,
                })).collect::<Vec<_>>(),
            });
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
    }

    // Plan K t-008: enforce the budget. `--soft` downgrades a hard
    // failure to a stderr warning so CI can surface drift without
    // breaking the build.
    if let Some(r) = budget_report {
        if !r.ok {
            let mut violations: Vec<String> = Vec::new();
            if r.over_total {
                violations.push(format!(
                    "total {} bytes > budget {} bytes",
                    r.total_bytes, r.total_budget
                ));
            }
            for s in &r.shards {
                if s.over_per_shard {
                    violations.push(format!(
                        "shard `{}` {} bytes > per-shard budget {} bytes",
                        s.path, s.bytes, r.per_shard_budget
                    ));
                }
            }
            let msg = format!("sidecar budget exceeded:\n  {}", violations.join("\n  "));
            if args.soft {
                eprintln!("warning: {msg}");
            } else {
                anyhow::bail!(msg);
            }
        }
    }
    Ok(())
}

// (write_jsonl, gather_buckets, ExportRecord and their tests live in
//  agentstatedeveloper_core::conclusions_export — single source of truth
//  shared by CLI and MCP.)
