//! `asd checklist <query>` — structured pre-edit checklist.
//!
//! Builds on `investigate` entry-point search and ledger invariants to produce
//! an action-oriented list: files to inspect, invariants to preserve, tests to
//! run, known hazards, and effects to verify.
//!
//! Default output is Markdown; pass `--json` for machine-readable JSON.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{
    AsgEffectStore, AsgIndexStore, AsgLedgerStore, EffectStore, Engine, FtsFilters, IndexStore,
    LedgerKind, LedgerStore, classify_layer_sym, derive_cold_hints, estimate_tokens, git_dirty_files,
    intent_focus, load_layer_overrides, parse_intent, propose_test_path, stale_warning, symbol_tier,
    trim_for_agent,
};

use crate::commands::{
    graph::build_id_map,
    investigate::find_candidates,
    search::query_tokens,
};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct ChecklistArgs {
    /// Natural-language or keyword query (same as `asd investigate`).
    pub query: String,

    /// Number of top entry-point symbols to analyse (default: 5).
    #[arg(long, default_value = "5")]
    pub depth: usize,

    /// Filter by symbol kind.
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by language.
    #[arg(long)]
    pub language: Option<String>,

    /// Include test-file symbols as entry-point candidates.
    #[arg(long)]
    pub include_tests: bool,

    /// Suppress the stale-index warning.
    #[arg(long)]
    pub quiet: bool,

    /// Emit JSON instead of Markdown.
    #[arg(long)]
    pub json: bool,

    /// Adjust checklist framing for a specific intent.
    /// Values: bugfix, feature, refactor, test, architecture, ui.
    #[arg(long)]
    pub intent: Option<String>,

    /// Caller BFS depth for finding affected tests (default: 2).
    #[arg(long, default_value = "2")]
    pub test_depth: usize,

    /// Emit token-budgeted JSON for LLM consumption (implies --json).
    #[arg(long)]
    pub agent: bool,

    /// Token budget when --agent is set (default: 8000).
    #[arg(long, default_value = "8000")]
    pub agent_budget: usize,
}

pub fn run(cfg: &Config, args: ChecklistArgs) -> Result<()> {
    if !args.quiet {
        if let Some(warn) = stale_warning(&cfg.db_path, 3600) {
            eprintln!("{warn}");
        }
    }
    let intent = args.intent.as_deref().and_then(parse_intent).unwrap_or("");
    let layer_overrides = load_layer_overrides(&cfg.db_path);
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index_store = AsgIndexStore { repo: &engine.repo };
    let ledger_store = AsgLedgerStore { repo: &engine.repo };
    let effect_store = AsgEffectStore { repo: &engine.repo };
    let id_map = build_id_map(&engine);

    let tokens = query_tokens(&args.query);
    if tokens.is_empty() {
        if args.json {
            println!("{}", json!({"query": args.query, "items": {}}));
        } else {
            println!("# Pre-edit checklist: {}\n\nNo matching symbols.", args.query);
        }
        return Ok(());
    }

    let filters = FtsFilters {
        kind: args.kind.as_deref().map(|k| k.to_lowercase()),
        language: args.language.as_deref().map(|l| l.to_lowercase()),
        include_tests: args.include_tests,
    };

    let candidates = find_candidates(
        &engine,
        &cfg.db_path,
        &args.query,
        &tokens,
        &filters,
        &ledger_store,
        &index_store,
        args.depth,
    );

    // --- Files to inspect -------------------------------------------------
    let mut files_to_inspect: Vec<Value> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();

    // --- Invariants, hazards, effects, notes ------------------------------
    let mut invariants: Vec<Value> = Vec::new();
    let mut hazards: Vec<Value> = Vec::new();
    let mut effects_list: Vec<Value> = Vec::new();
    let mut seen_inv: HashSet<String> = HashSet::new();

    // --- Affected tests (BFS from each entry point) -----------------------
    let mut test_rows: Vec<Value> = Vec::new();
    let mut seen_tests: HashSet<String> = HashSet::new();

    for (_score, qname) in &candidates {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        let tier = symbol_tier(&sym.file);
        let layer = classify_layer_sym(&sym.file, &sym.qname, tier, &layer_overrides);

        // Files to inspect.
        if seen_files.insert(sym.file.clone()) {
            files_to_inspect.push(json!({
                "file": sym.file,
                "qname": sym.qname,
                "layer": layer,
                "line": sym.start.line,
            }));
        }

        // Ledger entries.
        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();
        for entry in &entries {
            let key = entry.summary.clone();
            match entry.kind {
                LedgerKind::Invariant => {
                    if seen_inv.insert(key) {
                        invariants.push(json!({
                            "summary": entry.summary,
                            "source": sym.qname,
                            "body": entry.body,
                        }));
                    }
                }
                LedgerKind::Hazard => {
                    hazards.push(json!({
                        "summary": entry.summary,
                        "source": sym.qname,
                        "body": entry.body,
                    }));
                }
                _ => {}
            }
        }

        // Effects.
        if let Ok(Some(decl)) = effect_store.get_effects(&engine.ref_name, &sym.symbol_id) {
            for eff in &decl.declared {
                let key = format!("{:?}", eff.effect);
                effects_list.push(json!({
                    "category": key,
                    "source": sym.qname,
                }));
            }
        }

        // Affected tests via BFS.
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        visited.insert(sym.symbol_id.clone());
        queue.push_back((sym.symbol_id.clone(), 0));
        while let Some((sid, depth)) = queue.pop_front() {
            if depth >= args.test_depth { continue; }
            let callers = index_store
                .get_callers(&engine.ref_name, &sid)
                .unwrap_or_default();
            for caller_id in callers {
                if visited.contains(&caller_id) { continue; }
                visited.insert(caller_id.clone());
                if let Some(s) = id_map.get(&caller_id) {
                    if symbol_tier(&s.file) == 2 && seen_tests.insert(s.qname.clone()) {
                        test_rows.push(json!({
                            "qname": s.qname,
                            "file": s.file,
                            "line": s.start.line,
                        }));
                    }
                    if depth + 1 < args.test_depth {
                        queue.push_back((caller_id, depth + 1));
                    }
                }
            }
        }
    }

    // Deduplicate effects by category+source.
    effects_list.dedup_by(|a, b| {
        a.get("category").and_then(Value::as_str) == b.get("category").and_then(Value::as_str)
            && a.get("source").and_then(Value::as_str) == b.get("source").and_then(Value::as_str)
    });

    let focus = intent_focus(intent);

    // --- Staleness + test-gap -------------------------------------------
    let dirty = git_dirty_files();
    let stale_symbols: Vec<&str> = files_to_inspect
        .iter()
        .filter_map(|v| v.get("file").and_then(Value::as_str))
        .filter(|f| dirty.contains(*f))
        .collect();
    let test_gap = test_rows.is_empty();
    let proposed_test_path = test_gap.then(|| {
        files_to_inspect.first()
            .and_then(|v| v.get("file").and_then(Value::as_str))
            .map(propose_test_path)
    }).flatten();
    let suggested_test_coverage: Vec<String> = if test_gap {
        let mut hints: Vec<String> = invariants.iter()
            .filter_map(|inv| inv.get("summary").and_then(Value::as_str))
            .map(|s| s.to_string())
            .collect();
        for eff in &effects_list {
            if let Some(cat) = eff.get("category").and_then(Value::as_str) {
                let hint = format!("verify {} after change", cat.to_lowercase());
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
            }
        }
        // Cold-start fallback: if no recorded invariants, derive hints from
        // the top candidate symbol's own name, signature, and doc comment.
        if invariants.is_empty() {
            if let Some((_, qname)) = candidates.first() {
                if let Ok(Some(sym)) = index_store.get_symbol_by_qname(&engine.ref_name, qname) {
                    for h in derive_cold_hints(&sym.qname, sym.signature.as_deref(), sym.doc.as_deref()) {
                        if !hints.contains(&h) {
                            hints.push(h);
                        }
                    }
                }
            }
        }
        hints
    } else {
        vec![]
    };

    if args.agent || args.json {
        let out = json!({
            "query": args.query,
            "intent": if intent.is_empty() { Value::Null } else { json!(intent) },
            "focus": if focus.is_empty() { Value::Null } else { json!(focus) },
            "files_to_inspect": files_to_inspect,
            "invariants_to_preserve": invariants,
            "tests_to_run": test_rows,
            "test_gap": test_gap,
            "proposed_test_path": proposed_test_path,
            "suggested_test_coverage": suggested_test_coverage,
            "stale_symbols": stale_symbols,
            "known_hazards": hazards,
            "effects_to_verify": effects_list,
        });
        let out = if args.agent {
            let max_list = (args.agent_budget / 500).max(3).min(20);
            let trimmed = trim_for_agent(&out, max_list);
            let json_str = serde_json::to_string_pretty(&trimmed)?;
            let token_est = estimate_tokens(&json_str);
            let mut v = trimmed;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("token_estimate".into(), json!(token_est));
            }
            v
        } else {
            out
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_markdown(
            &args.query,
            intent,
            focus,
            &files_to_inspect,
            &invariants,
            &test_rows,
            test_gap,
            proposed_test_path.as_deref(),
            &suggested_test_coverage,
            &stale_symbols,
            &hazards,
            &effects_list,
        );
    }
    Ok(())
}

fn print_markdown(
    query: &str,
    intent: &str,
    focus: &str,
    files: &[Value],
    invariants: &[Value],
    tests: &[Value],
    test_gap: bool,
    proposed_test_path: Option<&str>,
    suggested_coverage: &[String],
    stale_symbols: &[&str],
    hazards: &[Value],
    effects: &[Value],
) {
    println!("# Pre-edit checklist: {query}");
    if !intent.is_empty() {
        println!("\n**Intent:** {intent}  ");
        println!("**Focus:** {focus}");
    }

    println!("\n## Files to inspect");
    if files.is_empty() {
        println!("_(no entry points found)_");
    } else {
        for f in files {
            let file = f.get("file").and_then(Value::as_str).unwrap_or("");
            let qname = f.get("qname").and_then(Value::as_str).unwrap_or("");
            let layer = f.get("layer").and_then(Value::as_str).unwrap_or("");
            let line = f.get("line").and_then(Value::as_u64).unwrap_or(0);
            println!("- [ ] `{file}:{line}` — `{qname}` [{layer}]");
        }
    }

    println!("\n## Invariants to preserve");
    if invariants.is_empty() {
        println!("_(no invariants recorded — add with `asd ledger append --kind invariant`)_");
    } else {
        for inv in invariants {
            let summary = inv.get("summary").and_then(Value::as_str).unwrap_or("");
            let source = inv.get("source").and_then(Value::as_str).unwrap_or("");
            println!("- [ ] {summary} _(from `{source}`)_");
        }
    }

    println!("\n## Tests to run");
    if tests.is_empty() {
        if test_gap {
            println!("_(no test callers found — test gap detected)_");
            if let Some(path) = proposed_test_path {
                println!("_Suggested test file: `{path}`_");
            }
            if !suggested_coverage.is_empty() {
                println!("\n_Suggested behaviours to cover:_");
                for hint in suggested_coverage {
                    println!("- [ ] {hint}");
                }
            }
        } else {
            println!("_(no test callers found within BFS depth)_");
        }
    } else {
        for t in tests {
            let qname = t.get("qname").and_then(Value::as_str).unwrap_or("");
            let file = t.get("file").and_then(Value::as_str).unwrap_or("");
            println!("- [ ] `{qname}` — {file}");
        }
    }

    if !stale_symbols.is_empty() {
        println!("\n## Staleness warning");
        println!("_The following files are modified since the last index — symbol ranges may be stale:_");
        for f in stale_symbols {
            println!("- `{f}`");
        }
    }

    if !hazards.is_empty() {
        println!("\n## Known hazards");
        for h in hazards {
            let summary = h.get("summary").and_then(Value::as_str).unwrap_or("");
            let source = h.get("source").and_then(Value::as_str).unwrap_or("");
            println!("- [ ] {summary} _(from `{source}`)_");
        }
    }

    if !effects.is_empty() {
        println!("\n## Effects to verify");
        let mut seen: HashSet<String> = HashSet::new();
        for e in effects {
            let cat = e.get("category").and_then(Value::as_str).unwrap_or("");
            let src = e.get("source").and_then(Value::as_str).unwrap_or("");
            let key = format!("{cat}:{src}");
            if seen.insert(key) {
                println!("- [ ] `{cat}` — declared by `{src}`");
            }
        }
    }
}
