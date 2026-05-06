//! `asd search <query>` — ranked concept search over indexed symbols.
//!
//! Primary path: BM25 via FTS5 table populated at `asd index` time.
//! Hybrid reranking: FTS BM25 score + ledger-text token boost.
//! Fallback: in-memory O(N) scoring when FTS table is empty or absent.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Engine, FtsFilters, IndexStore, LedgerStore, SearchFtsDb,
    SymbolKind, hybrid_boost,
};

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

    /// Include symbols from test files in results. By default tests are
    /// excluded so production entry points rank first.
    #[arg(long)]
    pub include_tests: bool,
}

pub fn run(cfg: &Config, args: SearchArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let ledger_store = AsgLedgerStore { repo: &engine.repo };

    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
    };

    // --- FTS path ---
    let fts_result = SearchFtsDb::open(&cfg.db_path)
        .ok()
        .filter(|fts| fts.has_data())
        .and_then(|fts| fts.search(&args.query, &filters, args.limit * 4).ok());

    if let Some(hits) = fts_result {
        let tokens = query_tokens(&args.query);

        // Hybrid reranking: BM25 + path/name boost + ledger boost.
        let mut scored: Vec<(f64, _)> = hits
            .into_iter()
            .map(|hit| {
                let boost = hybrid_boost(&hit, &tokens);
                let ledger_boost = {
                    let entries = ledger_store
                        .list_entries(&engine.ref_name, &hit.symbol_id)
                        .unwrap_or_default();
                    let text = entries
                        .iter()
                        .map(|e| e.summary.to_lowercase())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if text.is_empty() {
                        0.0
                    } else {
                        tokens.iter().filter(|t| text.contains(t.as_str())).count() as f64
                    }
                };
                (hit.bm25_score + boost + ledger_boost, hit)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.qname.cmp(&b.1.qname)));
        scored.truncate(args.limit);

        if scored.is_empty() {
            println!("No results for {:?}", args.query);
            return Ok(());
        }

        for (score, hit) in &scored {
            println!(
                "[{:.1}] {} {} ({}:{})",
                score, hit.kind, hit.qname, hit.file, hit.line
            );
            if let Some(sig) = &hit.signature {
                if !sig.is_empty() {
                    println!("       sig: {}", sig);
                }
            }
            if let Some(doc) = &hit.doc {
                if !doc.is_empty() {
                    let preview: String = doc.chars().take(80).collect();
                    println!("       doc: {}", preview);
                }
            }
        }
        return Ok(());
    }

    // --- Fallback: in-memory O(N) scoring ---
    eprintln!("asd: FTS index not populated — falling back to in-memory search (run `asd index` to enable fast search)");

    let tokens = query_tokens(&args.query);
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
    let mut scored: Vec<(u32, agentstatedeveloper_core::Symbol)> = Vec::new();

    for qname in &qnames {
        let sym = match index.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };

        if let Some(ref k) = kind_filter {
            let sym_kind = kind_str(&sym.kind);
            if sym_kind != k.as_str() { continue; }
        }
        if let Some(lang) = lang_filter {
            if sym.language != lang { continue; }
        }

        let score = in_memory_score(&sym, &tokens, &ledger_store, &engine);
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
        let kind = kind_str(&sym.kind);
        println!("[{:3}] {} {} ({})", score, kind, sym.qname, sym.file);
        if let Some(sig) = sym.signature.as_deref() {
            if !sig.is_empty() { println!("       sig: {}", sig); }
        }
        if let Some(doc) = sym.doc.as_deref() {
            if !doc.is_empty() {
                let preview: String = doc.chars().take(80).collect();
                println!("       doc: {}", preview);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
        .map(|t| t.to_lowercase())
        .filter(|t| t.len() >= 2)
        .collect()
}

pub(crate) fn kind_str(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "module",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Variable => "variable",
    }
}

pub(crate) fn in_memory_score(
    sym: &agentstatedeveloper_core::Symbol,
    tokens: &[String],
    ledger_store: &AsgLedgerStore,
    engine: &Engine,
) -> u32 {
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
    for token in tokens {
        if qname_lower.contains(token.as_str()) { score += 4; }
        if !sig_lower.is_empty() && sig_lower.contains(token.as_str()) { score += 3; }
        if !doc_lower.is_empty() && doc_lower.contains(token.as_str()) { score += 3; }
        if !ledger_text.is_empty() && ledger_text.contains(token.as_str()) { score += 2; }
        if file_lower.contains(token.as_str()) { score += 1; }
    }
    score
}
