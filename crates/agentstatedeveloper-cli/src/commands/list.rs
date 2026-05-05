//! `asd list <symbols|effects|ledger>` — enumerate indexed objects with
//! optional filters.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use agentstatedeveloper_core::{EffectDecl, Engine, LedgerEntry, Symbol};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(subcommand)]
    pub cmd: ListCmd,
}

#[derive(Debug, Subcommand)]
pub enum ListCmd {
    /// List all indexed symbols
    Symbols {
        /// Filter by language (rust, python, typescript, go, java, csharp, ruby, kotlin, swift)
        #[arg(long)]
        lang: Option<String>,

        /// Filter by symbol kind (function, method, class, module)
        #[arg(long)]
        kind: Option<String>,

        /// Filter by file path substring
        #[arg(long)]
        file: Option<String>,
    },

    /// List effect declarations
    Effects {
        /// Only show symbols with at least one declared effect
        #[arg(long)]
        has_declared: bool,

        /// Filter by effect category (e.g. io.net.out, log, pure)
        #[arg(long)]
        category: Option<String>,
    },

    /// List all ledger entries
    Ledger {
        /// Filter by entry kind (decision, assumption, constraint, rationale, hazard, tradeoff)
        #[arg(long)]
        kind: Option<String>,
    },
}

fn tree_or_empty(engine: &Engine, prefix: &str) -> Value {
    engine
        .repo
        .get_tree(&engine.ref_name, prefix)
        .unwrap_or(Value::Object(Default::default()))
}

fn kind_str(v: &impl serde::Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn run(cfg: &Config, args: ListArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    match args.cmd {
        ListCmd::Symbols { lang, kind, file } => {
            let tree = tree_or_empty(&engine, "/asd/v1/index/by-qname");
            let mut symbols: Vec<Symbol> = tree
                .as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();

            if let Some(l) = &lang {
                symbols.retain(|s| &s.language == l);
            }
            if let Some(k) = &kind {
                symbols.retain(|s| &kind_str(&s.kind) == k);
            }
            if let Some(f) = &file {
                symbols.retain(|s| s.file.contains(f.as_str()));
            }

            symbols.sort_by(|a, b| a.qname.cmp(&b.qname));

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "count": symbols.len(),
                    "symbols": symbols,
                }))?
            );
        }

        ListCmd::Effects { has_declared, category } => {
            let tree = tree_or_empty(&engine, "/asd/v1/effects");
            let mut decls: Vec<EffectDecl> = tree
                .as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();

            if has_declared {
                decls.retain(|d| !d.declared.is_empty());
            }
            if let Some(cat) = &category {
                decls.retain(|d| {
                    d.declared.iter().any(|e| e.effect.as_str() == cat.as_str())
                        || d.transitive.iter().any(|t| t.effect.as_str() == cat.as_str())
                });
            }

            decls.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "count": decls.len(),
                    "effects": decls,
                }))?
            );
        }

        ListCmd::Ledger { kind } => {
            let tree = tree_or_empty(&engine, "/asd/v1/ledger");
            let mut entries: Vec<LedgerEntry> = Vec::new();

            if let Some(sym_map) = tree.as_object() {
                for per_symbol in sym_map.values() {
                    if let Some(entry_map) = per_symbol.as_object() {
                        for entry_val in entry_map.values() {
                            if let Ok(e) =
                                serde_json::from_value::<LedgerEntry>(entry_val.clone())
                            {
                                entries.push(e);
                            }
                        }
                    }
                }
            }

            if let Some(k) = &kind {
                entries.retain(|e| &kind_str(&e.kind) == k);
            }

            entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "count": entries.len(),
                    "entries": entries,
                }))?
            );
        }
    }

    Ok(())
}
