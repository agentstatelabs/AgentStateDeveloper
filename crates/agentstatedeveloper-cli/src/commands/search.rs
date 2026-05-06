//! `asd search <query>` — ranked concept search over indexed symbols.
//!
//! Scores every symbol across name, signature, doc comment, file path, and
//! ledger summaries. Returns results sorted by relevance score, highest first.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{AsgIndexStore, AsgLedgerStore, Engine, IndexStore, LedgerStore};

use crate::config::Config;

const ASD_PATH_PREFIX: &str = "/asd/v1";

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Concept or keyword(s) to search for. Scored across symbol name,
    /// signature, doc comment, file path, and ledger entry summaries.
    pub query: String,

    /// Filter by symbol kind: module, function, method, class, variable.
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by language (e.g. "swift", "python", "typescript", "rust").
    #[arg(long)]
    pub language: Option<String>,

    /// Maximum results to show (default: 20).
    #[arg(long, default_value = "20")]
    pub limit: usize,
}

pub fn run(cfg: &Config, args: SearchArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let tokens: Vec<String> = args
        .query
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2)
        .collect();

    if tokens.is_empty() {
        println!("[]");
        return Ok(());
    }

    let kind_filter = args.kind.as_deref().map(|k| k.to_lowercase());
    let lang_filter = args.language.as_deref();

    let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
    let qnames: Vec<String> = match engine.repo.get_tree(&engine.ref_name, &prefix) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => vec![],
    };

    let index = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };

    let mut scored: Vec<(u32, agentstatedeveloper_core::Symbol)> = Vec::new();

    for qname in &qnames {
        let sym = match index.get_symbol_by_qname(&engine.ref_name, qname) {
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
            if qname_lower.contains(token.as_str()) {
                score += 4;
            }
            if !sig_lower.is_empty() && sig_lower.contains(token.as_str()) {
                score += 3;
            }
            if !doc_lower.is_empty() && doc_lower.contains(token.as_str()) {
                score += 3;
            }
            if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) {
                score += 2;
            }
            if file_lower.contains(token.as_str()) {
                score += 1;
            }
        }

        if score > 0 {
            scored.push((score, sym));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.qname.cmp(&b.1.qname)));
    scored.truncate(args.limit);

    if scored.is_empty() {
        println!("No results for {:?}", args.query);
        return Ok(());
    }

    for (score, sym) in &scored {
        let kind = format!("{:?}", sym.kind).to_lowercase();
        let sig = sym.signature.as_deref().unwrap_or("");
        let doc_preview = sym
            .doc
            .as_deref()
            .map(|d| {
                let s: String = d.chars().take(80).collect();
                format!("  doc: {}", s)
            })
            .unwrap_or_default();

        println!(
            "[{:3}] {} {} ({})",
            score, kind, sym.qname, sym.file
        );
        if !sig.is_empty() {
            println!("       sig: {}", sig);
        }
        if !doc_preview.is_empty() {
            println!("      {}", doc_preview);
        }
    }

    Ok(())
}
