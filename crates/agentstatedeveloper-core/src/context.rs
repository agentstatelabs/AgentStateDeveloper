//! Per-symbol context assembly for `asd context-for` (CLI) and the
//! `mcp__asd__context_for` MCP tool.
//!
//! Plan M t-001 (1.0.91): previously CLI exported a
//! `pub(crate)` `assemble_symbol_context` while MCP reimplemented
//! the same logic inline (~150 lines, byte-for-byte equivalent).
//! Lifted to core so both surfaces share a single source of truth.
//!
//! The function returns the per-symbol JSON value the agent sees:
//! invariants/hazards/known_bugs/concepts/ownership grouped from
//! the ledger, ownership discovery from git blame, covering tests,
//! effects + per-effect verification detail, callers, callees.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::effects::{AsgEffectStore, EffectStore};
use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::{LedgerKind, Symbol, VerificationStatus};
use crate::search_fts::SearchFtsDb;
use crate::{OwnershipSignal, discover_symbol_ownership, find_covering_tests};

/// Assemble the per-symbol context object — the building block both
/// CLI `asd context-for` and MCP `context_for` emit per requested
/// qname.
///
/// `ownership_hint`: when `Some`, skip the `discover_symbol_ownership`
/// git blame/log calls. Pass this when processing multiple symbols
/// to share per-file results.
///
/// `fts`: borrowed FTS connection from the Engine; used for
/// covering-test lookup. `None` skips the test scan (tests,
/// one-off calls).
///
/// `include_body`: when false, drops the symbol's `body` field
/// from the output to keep context compact (file + line range
/// tells the agent exactly where to look).
pub fn assemble_symbol_context(
    engine: &Engine,
    index_store: &AsgIndexStore<'_>,
    effect_store: &AsgEffectStore<'_>,
    ledger_store: &AsgLedgerStore<'_>,
    symbol: &Symbol,
    id_map: &HashMap<String, Symbol>,
    include_body: bool,
    fts: Option<&SearchFtsDb>,
    ownership_hint: Option<&OwnershipSignal>,
) -> crate::error::Result<Value> {
    // Callers and callees.
    let callee_ids = index_store.get_callees(&engine.ref_name, &symbol.symbol_id)?;
    let caller_ids = index_store.get_callers(&engine.ref_name, &symbol.symbol_id)?;

    let resolve = |ids: &[String]| -> Vec<Value> {
        ids.iter()
            .map(|id| {
                if let Some(s) = id_map.get(id) {
                    json!({ "qname": s.qname, "file": s.file, "line": s.start.line })
                } else {
                    json!({ "symbol_id": id })
                }
            })
            .collect()
    };

    // t-013: per-edge runtime confidence (callee_id → (confidence, static_known))
    // for this symbol's outgoing edges, from the edge-evidence sidecar.
    let edge_ev: HashMap<String, (f64, bool)> = {
        let tree = engine
            .repo
            .get_tree(
                &engine.ref_name,
                &crate::paths::edge_evidence_from_prefix(&symbol.symbol_id),
            )
            .unwrap_or(Value::Null);
        let mut m = HashMap::new();
        if let Some(obj) = tree.as_object() {
            for (callee_id, v) in obj {
                if let Ok(ev) =
                    serde_json::from_value::<crate::edge_confidence::EdgeEvidence>(v.clone())
                {
                    m.insert(callee_id.clone(), (ev.confidence(), ev.static_known));
                }
            }
        }
        m
    };
    let round2 = |x: f64| (x * 100.0).round() / 100.0;
    // Callees, each annotated with edge_confidence when runtime evidence exists.
    let callees_out: Vec<Value> = callee_ids
        .iter()
        .map(|id| {
            let mut row = if let Some(s) = id_map.get(id) {
                json!({ "qname": s.qname, "file": s.file, "line": s.start.line })
            } else {
                json!({ "symbol_id": id })
            };
            if let Some((conf, _)) = edge_ev.get(id) {
                row["edge_confidence"] = json!(round2(*conf));
            }
            row
        })
        .collect();
    // Runtime-only edges: calls observed at runtime but absent from the static
    // graph (dynamic dispatch the walker missed).
    let runtime_missed_callees: Vec<Value> = edge_ev
        .iter()
        .filter(|(_, (_, static_known))| !static_known)
        .map(|(id, (conf, _))| {
            let qname = id_map
                .get(id)
                .map(|s| s.qname.clone())
                .unwrap_or_else(|| id.clone());
            json!({ "qname": qname, "edge_confidence": round2(*conf) })
        })
        .collect();

    // Effects.
    let effects = effect_store.get_effects(&engine.ref_name, &symbol.symbol_id)?;

    // Ledger — all entries, newest first.
    let ledger = ledger_store.list_entries(&engine.ref_name, &symbol.symbol_id)?;

    // Group ledger by kind for readability.
    let mut invariants: Vec<Value> = Vec::new();
    let mut hazards: Vec<Value> = Vec::new();
    let mut ownership: Vec<Value> = Vec::new();
    let mut proofs: Vec<Value> = Vec::new();
    let mut validation_scenarios: Vec<Value> = Vec::new();
    let mut known_bugs: Vec<Value> = Vec::new();
    let mut concepts: Vec<Value> = Vec::new();
    let mut other_ledger: Vec<Value> = Vec::new();

    for entry in &ledger {
        let v = serde_json::to_value(entry)?;
        match entry.kind {
            LedgerKind::Invariant => invariants.push(v),
            LedgerKind::Hazard => hazards.push(v),
            LedgerKind::Ownership => ownership.push(v),
            LedgerKind::Proof => proofs.push(v),
            LedgerKind::ValidationScenario => validation_scenarios.push(v),
            LedgerKind::KnownBug => known_bugs.push(v),
            LedgerKind::Concept => concepts.push(v),
            _ => other_ledger.push(v),
        }
    }

    let mut sym_val = serde_json::to_value(symbol)?;
    if !include_body {
        if let Some(obj) = sym_val.as_object_mut() {
            obj.remove("body");
        }
    }

    // Plan G t-003: ownership discovery from git blame + doc-comment
    // annotations. Skip the git subprocess spawns when the caller
    // passes a pre-computed hint.
    let ownership_signal_owned;
    let ownership_signal = if let Some(hint) = ownership_hint {
        hint
    } else {
        ownership_signal_owned = discover_symbol_ownership(
            &symbol.file,
            symbol.start.line,
            symbol.end.line,
            symbol.doc.as_deref(),
        );
        &ownership_signal_owned
    };
    let mut discovered_ownership: serde_json::Map<String, Value> = serde_json::Map::new();
    if let Some(ref author) = ownership_signal.primary_author {
        discovered_ownership.insert("primary_author".into(), json!(author));
    }
    if let Some(ref doc_owner) = ownership_signal.doc_owner {
        discovered_ownership.insert("doc_owner".into(), json!(doc_owner));
    }
    if !ownership_signal.recent_committers.is_empty() {
        discovered_ownership.insert(
            "recent_committers".into(),
            json!(ownership_signal.recent_committers),
        );
    }
    if !ownership_signal.annotated.is_empty() {
        let annotated_val: Vec<Value> = ownership_signal
            .annotated
            .iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "source": serde_json::to_value(a.source).unwrap_or(json!("unknown")),
                })
            })
            .collect();
        discovered_ownership.insert("annotated".into(), json!(annotated_val));
    }

    // Plan G t-003: covering tests for this impl symbol.
    let covering_tests: Vec<Value> = find_covering_tests(fts, &symbol.qname)
        .into_iter()
        .map(|ct| {
            json!({
                "qname": ct.qname,
                "file": ct.file,
                "line": ct.line,
                "run_command": ct.run_command,
            })
        })
        .collect();

    // Per-effect verification detail — cross-reference declared
    // effects against verification mismatches so agents see
    // ok / mismatch / unverified per effect.
    let effects_detail: Vec<Value> = if let Some(ref decl) = effects {
        let mismatch_effects: std::collections::HashSet<String> = decl
            .verification
            .as_ref()
            .map(|v| {
                v.mismatches
                    .iter()
                    .map(|m| m.effect.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let overall_ok = decl
            .verification
            .as_ref()
            .map(|v| matches!(v.status, VerificationStatus::Ok))
            .unwrap_or(false);
        decl.declared
            .iter()
            .map(|e| {
                let effect_str = e.effect.as_str();
                let is_mismatched = mismatch_effects.contains(effect_str);
                let status = if decl.verification.is_none() {
                    "unverified"
                } else if is_mismatched {
                    "mismatch"
                } else if overall_ok {
                    "ok"
                } else {
                    "ok"
                };
                let mut obj = serde_json::Map::new();
                obj.insert("effect".into(), json!(effect_str));
                obj.insert("status".into(), json!(status));
                if let Some(ref adapter) = e.adapter {
                    obj.insert("adapter".into(), json!(adapter));
                }
                if let Some(ref pattern) = e.source_pattern {
                    obj.insert("source_pattern".into(), json!(pattern));
                }
                if let Some(note) = &e.note {
                    obj.insert("note".into(), json!(note));
                }
                Value::Object(obj)
            })
            .collect()
    } else {
        Vec::new()
    };

    // Cross-service edges (t-002): if this symbol owns any HTTP/pub-sub
    // endpoint, surface its endpoints and the matched edges so blast radius
    // crosses the service boundary. Omitted entirely when the symbol has none
    // (token economy). In-repo matches only; cross-repo is a Team-tier feature.
    let cross_service = {
        let tree = engine
            .repo
            .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
            .unwrap_or(Value::Null);
        let all = crate::cross_service::endpoints_from_tree(&tree);
        let mine: Vec<&crate::cross_service::ServiceEndpoint> = all
            .iter()
            .filter(|e| e.symbol_id == symbol.symbol_id)
            .collect();
        if mine.is_empty() {
            None
        } else {
            let edges = crate::cross_service::match_edges(&all);
            let touching: Vec<Value> = edges
                .iter()
                .filter(|e| {
                    e.from.symbol_id == symbol.symbol_id || e.to.symbol_id == symbol.symbol_id
                })
                .map(|e| {
                    let is_handler = e.to.symbol_id == symbol.symbol_id;
                    let peer = if is_handler { &e.from } else { &e.to };
                    json!({
                        "role": if is_handler { "handler" } else { "consumer" },
                        "contract": e.contract,
                        "peer_qname": peer.qname,
                        "peer_repo": peer.repo_id,
                        "cross_repo": e.cross_repo,
                        "confidence": e.confidence,
                    })
                })
                .collect();
            Some(json!({
                "endpoints": mine.iter().map(|e| json!({
                    "transport": e.transport,
                    "direction": e.direction,
                    "contract": e.contract,
                    "confidence": e.confidence,
                })).collect::<Vec<_>>(),
                "edges": touching,
            }))
        }
    };

    // Invariants and hazards are anti-footgun guards — surface them
    // first so agents see them before the call-graph details.
    let mut out = json!({
        "symbol": sym_val,
        "invariants": invariants,
        "hazards": hazards,
        "known_bugs": known_bugs,
        "concepts": concepts,
        "ownership": ownership,
        "ownership_discovery": discovered_ownership,
        "covering_tests": covering_tests,
        "validation_scenarios": validation_scenarios,
        "callers": resolve(&caller_ids),
        "callees": callees_out,
        "effects": effects,
        "effects_detail": effects_detail,
        "proofs": proofs,
        "decisions_and_notes": other_ledger,
    });
    if let Some(cs) = cross_service {
        out["cross_service"] = cs;
    }

    // Data-flow (t-002 slice 4): values flowing into this symbol's params
    // (incoming) and out of its call args (outgoing). Omitted when empty.
    let dataflow = {
        let tree = engine
            .repo
            .get_tree(&engine.ref_name, "/asd/v1/index/dataflow")
            .unwrap_or(Value::Null);
        let all = crate::dataflow::edges_from_tree(&tree);
        let incoming: Vec<Value> = all
            .iter()
            .filter(|e| e.to_symbol_id == symbol.symbol_id)
            .map(|e| json!({ "param": e.param, "from_arg": e.arg, "from_qname": e.from_qname }))
            .collect();
        let outgoing: Vec<Value> = all
            .iter()
            .filter(|e| e.from_symbol_id == symbol.symbol_id)
            .map(|e| json!({ "arg": e.arg, "to_param": e.param, "to_qname": e.to_qname }))
            .collect();
        if incoming.is_empty() && outgoing.is_empty() {
            None
        } else {
            Some(json!({ "incoming": incoming, "outgoing": outgoing }))
        }
    };
    if let Some(df) = dataflow {
        out["dataflow"] = df;
    }
    // t-013: runtime-observed calls the static graph missed (omit when none).
    if !runtime_missed_callees.is_empty() {
        out["runtime_missed_callees"] = json!(runtime_missed_callees);
    }
    Ok(out)
}
