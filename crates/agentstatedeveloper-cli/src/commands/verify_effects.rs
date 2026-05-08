//! `asd verify-effects <qname>` — compare declared effects against what the
//! static checker infers from the current source, write mismatches back to
//! the EffectDecl, and report verification status.
//!
//! Status values:
//!   ok        — declared effects match inferred (or inferred is empty)
//!   mismatch  — at least one declared effect not inferred, or vice versa
//!   unverified — source file unreadable or no adapter for the language

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_adapters::default_adapters;
use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, EffectStore, Engine, IndexStore, ParsedSymbol,
};
use agentstatedeveloper_core::schema::{
    EffectCategory, EffectDecl, Mismatch, Verification, VerificationSource, VerificationStatus,
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct VerifyEffectsArgs {
    /// Fully-qualified symbol name.
    pub qname: String,

    /// Write verification result back to the EffectDecl store.
    #[arg(long)]
    pub write: bool,
}

pub fn run(cfg: &Config, args: VerifyEffectsArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let adapters = default_adapters();

    let symbol = index_store
        .get_symbol_by_qname(&engine.ref_name, &args.qname)?
        .ok_or_else(|| anyhow::anyhow!("symbol not found: {}", args.qname))?;

    let mut effect_decl = effect_store
        .get_effects(&engine.ref_name, &symbol.symbol_id)?
        .unwrap_or_else(|| EffectDecl {
            symbol_id: symbol.symbol_id.clone(),
            declared: Vec::new(),
            transitive: Vec::new(),
            verification: None,
            confidence: None,
            matched_policy: None,
        });

    // Find an adapter for the symbol's language.
    let adapter = adapters
        .iter()
        .find(|a| a.language() == symbol.language.as_str());

    let (status, mismatches, inferred_strs) = if let Some(adapter) = adapter {
        match std::fs::read_to_string(&symbol.file) {
            Ok(source) => {
                // Build a ParsedSymbol stub sufficient for infer_effects.
                let stub = ParsedSymbol {
                    qname: symbol.qname.clone(),
                    kind: symbol.kind,
                    start_line: symbol.start.line,
                    start_col: symbol.start.col,
                    end_line: symbol.end.line,
                    end_col: symbol.end.col,
                    body: String::new(),
                    signature: symbol.signature.clone(),
                    doc: symbol.doc.clone(),
                };
                let inferred: Vec<EffectCategory> = adapter
                    .infer_effects(&source, &stub)
                    .into_iter()
                    .map(|e| e.effect)
                    .collect();

                let declared_cats: Vec<EffectCategory> =
                    effect_decl.declared.iter().map(|e| e.effect.clone()).collect();

                let mut mismatches: Vec<Mismatch> = Vec::new();

                // Declared but not inferred = possible over-declaration.
                for cat in &declared_cats {
                    if !inferred.contains(cat) {
                        mismatches.push(Mismatch {
                            kind: "declared_not_inferred".to_string(),
                            effect: cat.clone(),
                            detected_in: Some(symbol.file.clone()),
                            note: Some("declared but not found by static checker".to_string()),
                        });
                    }
                }

                // Inferred but not declared = possible under-declaration.
                for cat in &inferred {
                    if !declared_cats.contains(cat) {
                        mismatches.push(Mismatch {
                            kind: "inferred_not_declared".to_string(),
                            effect: cat.clone(),
                            detected_in: Some(symbol.file.clone()),
                            note: Some("found by static checker but not in declared effects".to_string()),
                        });
                    }
                }

                let status = if mismatches.is_empty() {
                    VerificationStatus::Ok
                } else {
                    VerificationStatus::Mismatch
                };
                let inferred_strs: Vec<String> = inferred.iter().map(|e| e.as_str().to_string()).collect();
                (status, mismatches, inferred_strs)
            }
            Err(e) => {
                eprintln!("asd: cannot read {}: {e}", symbol.file);
                (VerificationStatus::Unverified, Vec::new(), Vec::new())
            }
        }
    } else {
        (VerificationStatus::Unverified, Vec::new(), Vec::new())
    };

    let verification = Verification {
        by: VerificationSource::StaticChecker,
        at: chrono::Utc::now(),
        status,
        mismatches: mismatches.clone(),
    };

    if args.write {
        effect_decl.verification = Some(verification);
        effect_store.put_effects(&engine.ref_name, &effect_decl)?;
    }

    let out = json!({
        "qname": symbol.qname,
        "symbol_id": symbol.symbol_id,
        "status": match status {
            VerificationStatus::Ok => "ok",
            VerificationStatus::Mismatch => "mismatch",
            VerificationStatus::Unverified => "unverified",
        },
        "declared": effect_decl.declared.iter().map(|e| e.effect.as_str()).collect::<Vec<_>>(),
        "inferred": inferred_strs,
        "mismatches": mismatches.iter().map(|m| json!({
            "kind": m.kind,
            "effect": m.effect.as_str(),
            "note": m.note,
        })).collect::<Vec<_>>(),
        "written": args.write,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
