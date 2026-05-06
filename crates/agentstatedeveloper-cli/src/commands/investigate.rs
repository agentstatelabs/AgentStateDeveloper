//! `asd investigate <query>` — broad feature archaeology in one pass.
//!
//! 1. Scored search across qname/signature/doc/file/ledger to find entry points.
//! 2. For each top result: callers, callees, effects, invariants, hazards, notes.
//! 3. Prints a structured JSON report (or human-readable with --text).

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, Engine, IndexStore, LedgerStore,
};

use crate::commands::{context_for::assemble_symbol_context, graph::build_id_map};
use crate::config::Config;

const ASD_PATH_PREFIX: &str = "/asd/v1";

#[derive(Debug, Args)]
pub struct InvestigateArgs {
    /// Natural-language or keyword query. Scored across symbol name,
    /// signature, doc comment, file path, and ledger entries.
    pub query: String,

    /// Number of top entry-point symbols to fully expand (default: 5).
    #[arg(long, default_value = "5")]
    pub depth: usize,

    /// Filter by symbol kind: module, function, method, class, variable.
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by language (e.g. "swift", "python", "typescript", "rust").
    #[arg(long)]
    pub language: Option<String>,

    /// Include full source body of each symbol in output (can be large).
    #[arg(long, default_value = "false")]
    pub include_body: bool,
}

pub fn run(cfg: &Config, args: InvestigateArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let tokens: Vec<String> = args
        .query
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2)
        .collect();

    if tokens.is_empty() {
        println!("{}", json!({ "query": args.query, "entry_points": [] }));
        return Ok(());
    }

    let kind_filter = args.kind.as_deref().map(|k| k.to_lowercase());
    let lang_filter = args.language.as_deref();

    let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
    let qnames: Vec<String> = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => vec![],
    };

    let index_store = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let id_map = build_id_map(&engine);

    let mut scored: Vec<(u32, agentstatedeveloper_core::Symbol)> = Vec::new();

    for qname in &qnames {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };

        if let Some(ref k) = kind_filter {
            let sym_kind = match sym.kind {
                agentstatedeveloper_core::SymbolKind::Module => "module",
                agentstatedeveloper_core::SymbolKind::Function => "function",
                agentstatedeveloper_core::SymbolKind::Method => "method",
                agentstatedeveloper_core::SymbolKind::Class => "class",
                agentstatedeveloper_core::SymbolKind::Variable => "variable",
            };
            if sym_kind != k.as_str() {
                continue;
            }
        }
        if let Some(lang) = lang_filter {
            if sym.language != lang {
                continue;
            }
        }

        let qname_lower = sym.qname.to_lowercase();
        let sig_lower = sym.signature.as_deref().unwrap_or("").to_lowercase();
        let doc_lower = sym.doc.as_deref().unwrap_or("").to_lowercase();
        let file_lower = sym.file.to_lowercase();

        let ledger_text: String = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default()
            .iter()
            .map(|e| e.summary.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");

        let mut score: u32 = 0;
        for token in &tokens {
            if qname_lower.contains(token.as_str()) { score += 4; }
            if !sig_lower.is_empty() && sig_lower.contains(token.as_str()) { score += 3; }
            if !doc_lower.is_empty() && doc_lower.contains(token.as_str()) { score += 3; }
            if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) { score += 2; }
            if file_lower.contains(token.as_str()) { score += 1; }
        }

        if score > 0 {
            scored.push((score, sym));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.qname.cmp(&b.1.qname)));
    scored.truncate(args.depth);

    let mut entry_points: Vec<Value> = Vec::new();
    for (score, sym) in &scored {
        let ctx = assemble_symbol_context(
            &engine,
            &index_store,
            &effect_store,
            &ledger_store,
            sym,
            &id_map,
            args.include_body,
        )?;
        let mut ep = json!({ "score": score });
        if let (Some(obj), Some(ctx_obj)) = (ep.as_object_mut(), ctx.as_object()) {
            for (k, v) in ctx_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        entry_points.push(ep);
    }

    let out = json!({
        "query": args.query,
        "tokens": tokens,
        "entry_points": entry_points,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
