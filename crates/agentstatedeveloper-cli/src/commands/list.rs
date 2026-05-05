//! `asd list <symbols|effects|ledger|stats>` — enumerate indexed objects with
//! optional filters, or show aggregate graph metrics.

use std::collections::HashMap;

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

    /// Show aggregate metrics about the graph
    Stats,
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

        ListCmd::Stats => {
            // --- symbols ---
            let sym_tree = tree_or_empty(&engine, "/asd/v1/index/by-qname");
            let symbols: Vec<Symbol> = sym_tree
                .as_object()
                .map(|m| m.values().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect())
                .unwrap_or_default();

            let mut by_lang: HashMap<&str, usize> = HashMap::new();
            let mut by_kind: HashMap<String, usize> = HashMap::new();
            let mut files: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for s in &symbols {
                *by_lang.entry(s.language.as_str()).or_default() += 1;
                *by_kind.entry(kind_str(&s.kind)).or_default() += 1;
                files.insert(s.file.as_str());
            }
            let mut by_lang: Vec<_> = by_lang.into_iter().map(|(k, v)| json!({"lang": k, "count": v})).collect();
            by_lang.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
            let mut by_kind: Vec<_> = by_kind.into_iter().map(|(k, v)| json!({"kind": k, "count": v})).collect();
            by_kind.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));

            // --- effects ---
            let eff_tree = tree_or_empty(&engine, "/asd/v1/effects");
            let decls: Vec<EffectDecl> = eff_tree
                .as_object()
                .map(|m| m.values().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect())
                .unwrap_or_default();

            let with_declared = decls.iter().filter(|d| !d.declared.is_empty()).count();
            let mut by_category: HashMap<&str, usize> = HashMap::new();
            let mut by_verification: HashMap<String, usize> = HashMap::new();
            for d in &decls {
                for e in &d.declared {
                    *by_category.entry(e.effect.as_str()).or_default() += 1;
                }
                for t in &d.transitive {
                    *by_category.entry(t.effect.as_str()).or_default() += 1;
                }
                let vs = d.verification.as_ref()
                    .map(|v| kind_str(&v.status))
                    .unwrap_or_else(|| "none".into());
                *by_verification.entry(vs).or_default() += 1;
            }
            let mut by_category: Vec<_> = by_category.into_iter()
                .map(|(k, v)| json!({"category": k, "count": v})).collect();
            by_category.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
            let mut by_verification: Vec<_> = by_verification.into_iter()
                .map(|(k, v)| json!({"status": k, "count": v})).collect();
            by_verification.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));

            // --- call graph ---
            let callees_tree = tree_or_empty(&engine, "/asd/v1/index/callees");
            let mut total_edges: usize = 0;
            let mut symbols_with_callees: usize = 0;
            if let Some(m) = callees_tree.as_object() {
                for v in m.values() {
                    if let Some(arr) = v.get("callees").and_then(|c| c.as_array()) {
                        if !arr.is_empty() {
                            symbols_with_callees += 1;
                            total_edges += arr.len();
                        }
                    }
                }
            }
            let callers_tree = tree_or_empty(&engine, "/asd/v1/index/callers");
            let symbols_with_callers = callers_tree
                .as_object()
                .map(|m| m.values().filter(|v| {
                    v.get("callers").and_then(|c| c.as_array()).map(|a| !a.is_empty()).unwrap_or(false)
                }).count())
                .unwrap_or(0);

            // --- ledger ---
            let ledger_tree = tree_or_empty(&engine, "/asd/v1/ledger");
            let mut ledger_total: usize = 0;
            let mut ledger_by_kind: HashMap<String, usize> = HashMap::new();
            if let Some(sym_map) = ledger_tree.as_object() {
                for per_symbol in sym_map.values() {
                    if let Some(entry_map) = per_symbol.as_object() {
                        for entry_val in entry_map.values() {
                            if let Ok(e) = serde_json::from_value::<LedgerEntry>(entry_val.clone()) {
                                ledger_total += 1;
                                *ledger_by_kind.entry(kind_str(&e.kind)).or_default() += 1;
                            }
                        }
                    }
                }
            }
            let mut ledger_by_kind: Vec<_> = ledger_by_kind.into_iter()
                .map(|(k, v)| json!({"kind": k, "count": v})).collect();
            ledger_by_kind.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "symbols": {
                        "total": symbols.len(),
                        "files": files.len(),
                        "by_language": by_lang,
                        "by_kind": by_kind,
                    },
                    "effects": {
                        "total_symbols": decls.len(),
                        "with_declared": with_declared,
                        "pure_or_undeclared": decls.len() - with_declared,
                        "by_category": by_category,
                        "by_verification_status": by_verification,
                    },
                    "call_graph": {
                        "total_edges": total_edges,
                        "symbols_with_callees": symbols_with_callees,
                        "symbols_with_callers": symbols_with_callers,
                    },
                    "ledger": {
                        "total": ledger_total,
                        "by_kind": ledger_by_kind,
                    },
                }))?
            );
        }
    }

    Ok(())
}
