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
    AsgIndexStore, AsgLedgerStore, ConclusionClass, Engine, IndexStore, LedgerKind, LedgerStore,
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum ConclusionsCmd {
    /// List ledger entries grouped by conclusion class.
    List(ListArgs),
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

pub fn run(cfg: &Config, cmd: ConclusionsCmd) -> Result<()> {
    match cmd {
        ConclusionsCmd::List(args) => list(cfg, args),
    }
}

fn list(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index = AsgIndexStore::from_engine(&engine);
    let ledger = AsgLedgerStore::from_engine(&engine);
    let ref_name = engine.ref_name.clone();

    let target_class = args.class.map(ConclusionClass::from);

    // Resolve which symbols to scan: one specific qname or all indexed.
    let symbol_ids: Vec<(String, String)> = if let Some(qname) = args.symbol.as_deref() {
        match index.get_symbol_by_qname(&ref_name, qname)? {
            Some(sym) => vec![(sym.symbol_id, sym.qname)],
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
        let prefix = format!(
            "{}/index/by-qname",
            agentstatedeveloper_core::ASD_PATH_PREFIX
        );
        let tree = engine
            .repo
            .get_tree(&ref_name, &prefix)
            .unwrap_or(serde_json::Value::Null);
        let qnames: Vec<String> = match tree {
            serde_json::Value::Object(map) => map.keys().cloned().collect(),
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for qn in qnames {
            if let Some(sym) = index.get_symbol_by_qname(&ref_name, &qn)? {
                out.push((sym.symbol_id, sym.qname));
            }
        }
        out
    };

    // Bucket entries by class. Walk all symbols' ledger entries once.
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<&'static str, Vec<serde_json::Value>> = BTreeMap::new();
    for class in ConclusionClass::all() {
        if target_class.is_none() || target_class == Some(*class) {
            buckets.insert(class.filename_stem(), Vec::new());
        }
    }

    for (sym_id, qname) in &symbol_ids {
        let entries = ledger.list_entries(&ref_name, sym_id).unwrap_or_default();
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
