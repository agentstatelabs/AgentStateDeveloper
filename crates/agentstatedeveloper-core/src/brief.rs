//! Plan D t-001 + t-007: shared brief-output projections.
//!
//! Brief mode strips load-bearing fields out of the verbose JSON each
//! command normally emits. Lives in core so both CLI (`asd --brief`)
//! and MCP (`ASD_FORMAT=brief` at server startup) use the same
//! projections — single source of truth per the prepare_change
//! duplication-memory lesson.

use crate::schema::Symbol;
use serde_json::{Value, json};

/// Plan D t-007: whether the spawned process should default to brief
/// output. Reads `ASD_FORMAT=brief` (case-insensitive). Hosts can also
/// flip the flag explicitly per-call without consulting this helper.
pub fn brief_from_env() -> bool {
    std::env::var("ASD_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("brief"))
        .unwrap_or(false)
}

/// Plan D t-005: deterministic query id for wrapper-side dedup.
/// `query_id("read", &["store.pricing.rates.get_rate"])` →
/// `"Qf3a2b1c..."` (first 7 hex chars of blake3 of the joined input).
/// Same inputs → same id; safe to compare across processes.
pub fn query_id(cmd: &str, args: &[&str]) -> String {
    let mut s = String::with_capacity(cmd.len() + 32);
    s.push_str(cmd);
    for a in args {
        s.push('|');
        s.push_str(a);
    }
    let h = blake3::hash(s.as_bytes());
    let hex = h.to_hex();
    let prefix: String = hex.chars().take(7).collect();
    format!("Q{prefix}")
}

/// Project a Symbol down to the brief field set.
pub fn brief_symbol(sym: &Symbol) -> Value {
    let doc_line = sym
        .doc
        .as_deref()
        .and_then(|d| d.lines().next().map(|l| l.trim().to_string()))
        .filter(|l| !l.is_empty());
    let mut out = serde_json::Map::new();
    out.insert("qname".into(), Value::String(sym.qname.clone()));
    out.insert(
        "file".into(),
        Value::String(format!("{}:{}", sym.file, sym.start.line)),
    );
    if let Some(sig) = &sym.signature {
        out.insert("signature".into(), Value::String(sig.clone()));
    }
    if let Some(d) = doc_line {
        out.insert("doc".into(), Value::String(d));
    }
    Value::Object(out)
}

/// Project a list of `{qname, file, line}` records (the call-graph output
/// shape) down to a single `qname (file:line)` string per entry.
pub fn brief_call_list(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|v| {
            let q = v.get("qname").and_then(Value::as_str)?;
            let f = v.get("file").and_then(Value::as_str)?;
            let l = v.get("line").and_then(Value::as_u64).unwrap_or(0);
            Some(format!("{q} ({f}:{l})"))
        })
        .collect()
}

/// Brief shape for `read`: keeps the call graph + effect/ledger summaries
/// but as counts + compact strings, not full nested objects.
pub fn brief_read(
    sym: &Symbol,
    callers: &[Value],
    callees: &[Value],
    effects: Option<&Value>,
    ledger_count: usize,
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("symbol".into(), brief_symbol(sym));
    let caller_strs = brief_call_list(callers);
    let callee_strs = brief_call_list(callees);
    if !caller_strs.is_empty() {
        out.insert("callers".into(), json!(caller_strs));
    }
    if !callee_strs.is_empty() {
        out.insert("callees".into(), json!(callee_strs));
    }
    if let Some(eff) = effects {
        if let Some(declared) = eff.get("declared").and_then(Value::as_array) {
            let cats: Vec<String> = declared
                .iter()
                .filter_map(|e| e.get("effect").and_then(Value::as_str).map(String::from))
                .collect();
            if !cats.is_empty() {
                out.insert("effects".into(), json!(cats));
            }
        }
    }
    if ledger_count > 0 {
        out.insert("ledger_count".into(), json!(ledger_count));
    }
    Value::Object(out)
}

/// Plan D t-007: brief shape for `code_search` / `search`. Projects each
/// FTS hit down to `{qname, file, line, signature, doc, score?}`.
pub fn brief_search_results(hits: &[Value]) -> Vec<Value> {
    hits.iter()
        .map(|hit| {
            let mut out = serde_json::Map::new();
            if let Some(q) = hit.get("qname").and_then(Value::as_str) {
                out.insert("qname".into(), Value::String(q.into()));
            }
            let file = hit.get("file").and_then(Value::as_str).unwrap_or("");
            let line = hit.get("line").and_then(Value::as_u64).unwrap_or(0);
            if !file.is_empty() {
                out.insert("file".into(), Value::String(format!("{file}:{line}")));
            }
            if let Some(s) = hit.get("signature").and_then(Value::as_str) {
                out.insert("signature".into(), Value::String(s.into()));
            }
            if let Some(d) = hit
                .get("doc")
                .and_then(Value::as_str)
                .and_then(|s| s.lines().next())
            {
                let d = d.trim();
                if !d.is_empty() {
                    out.insert("doc".into(), Value::String(d.into()));
                }
            }
            if let Some(score) = hit.get("score") {
                out.insert("score".into(), score.clone());
            } else if let Some(score) = hit.get("bm25_score") {
                out.insert("score".into(), score.clone());
            }
            Value::Object(out)
        })
        .collect()
}

/// Plan F t-006: brief projection for an `investigate` response.
/// Strips the verbose entry-point bodies + ambiguity/possible-misses
/// metadata. Flattens `by_layer` (the MCP handler shape) into a single
/// `entry_points` list when present; otherwise uses a top-level
/// `entry_points` field directly (the CLI handler shape).
pub fn brief_investigate(full: &Value) -> Value {
    let mut out = serde_json::Map::new();
    // Token economy (1.0.80): no `query` echo — the agent has it.
    // `query_id` (trace marker, not input) preserved below.
    let entry_points: Vec<Value> =
        if let Some(eps) = full.get("entry_points").and_then(Value::as_array) {
            eps.clone()
        } else if let Some(by_layer) = full.get("by_layer").and_then(Value::as_object) {
            // Flatten { layer: [eps] } → single eps list.
            by_layer
                .values()
                .filter_map(|v| v.as_array().cloned())
                .flatten()
                .collect()
        } else {
            Vec::new()
        };
    if !entry_points.is_empty() {
        out.insert(
            "entry_points".into(),
            json!(brief_search_results(&entry_points)),
        );
    }
    for k in ["safe_change_recipe", "stale", "query_id"] {
        if let Some(v) = full.get(k) {
            if !v.is_null() {
                out.insert(k.into(), v.clone());
            }
        }
    }
    crate::ser_helpers::drop_empty_top_level(Value::Object(out))
}

/// Plan F t-006: brief projection for a `prepare_change` response.
/// Keeps the load-bearing slices an agent acts on (likely_edit_files,
/// safe_change_recipe, design_invariants, known_hazards) and drops the
/// orientation context (by_layer / recently_touched / scenario_tests /
/// suggested_test_coverage / effects_summary) that bloats output.
pub fn brief_prepare_change(full: &Value) -> Value {
    let mut out = serde_json::Map::new();
    // Token economy (1.0.80): no `description` echo (the agent
    // literally just sent it). `intent`/`focus` are canonicalized
    // derivatives, so they stay. `query_id`/`stale` preserved as
    // trace/diagnostic signals.
    for k in ["intent", "focus", "query_id", "stale"] {
        if let Some(v) = full.get(k) {
            if !v.is_null() {
                out.insert(k.into(), v.clone());
            }
        }
    }
    // Project per-file entries down to {file, why, layer?, top_symbol?}.
    if let Some(arr) = full.get("likely_edit_files").and_then(Value::as_array) {
        let compact: Vec<Value> = arr
            .iter()
            .map(|f| {
                let mut o = serde_json::Map::new();
                for k in ["file", "why", "top_symbol", "layer"] {
                    if let Some(v) = f.get(k) {
                        if !v.is_null() {
                            o.insert(k.into(), v.clone());
                        }
                    }
                }
                Value::Object(o)
            })
            .collect();
        out.insert("likely_edit_files".into(), json!(compact));
    }
    for k in [
        "safe_change_recipe",
        "design_invariants",
        "known_hazards",
        "prior_thinking",
    ] {
        if let Some(v) = full.get(k) {
            let nonempty = v.as_array().map(|a| !a.is_empty()).unwrap_or(false)
                || v.as_object().map(|o| !o.is_empty()).unwrap_or(false);
            if nonempty {
                out.insert(k.into(), v.clone());
            }
        }
    }
    // ExampleFlow refinement (1.0.76): thinking_summary passes
    // through (small, load-bearing — agents read it to tell "filtered"
    // from "absent" even when prior_thinking is empty/missing).
    // Token economy (1.0.80): drop_empty_top_level still strips it if
    // the struct's own skip-predicates collapsed it to `{}` — the
    // ExampleFlow signal needs at LEAST one non-zero field to be
    // worth emitting.
    if let Some(v) = full.get("thinking_summary") {
        out.insert("thinking_summary".into(), v.clone());
    }
    crate::ser_helpers::drop_empty_top_level(Value::Object(out))
}

/// Plan F t-006: brief projection for `context_for`. Keeps the symbol
/// signature + top callers/callees as compact "qname (file:line)"
/// strings; drops nested ledger bodies + full effect declarations.
pub fn brief_context_for(full: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(sym) = full.get("symbol") {
        // The symbol entry already has the brief shape if it came from
        // brief_symbol; otherwise re-project the minimum fields.
        let mut s = serde_json::Map::new();
        for k in ["qname", "file", "line", "signature", "doc"] {
            if let Some(v) = sym.get(k) {
                if !v.is_null() {
                    s.insert(k.into(), v.clone());
                }
            }
        }
        out.insert("symbol".into(), Value::Object(s));
    }
    for (k, max) in [("callers", 3usize), ("callees", 3usize)] {
        if let Some(arr) = full.get(k).and_then(Value::as_array) {
            let lines = brief_call_list(arr);
            let capped: Vec<String> = lines.into_iter().take(max).collect();
            if !capped.is_empty() {
                out.insert(k.into(), json!(capped));
            }
        }
    }
    if let Some(eff) = full.get("effects") {
        if let Some(declared) = eff.get("declared").and_then(Value::as_array) {
            let cats: Vec<String> = declared
                .iter()
                .filter_map(|e| e.get("effect").and_then(Value::as_str).map(String::from))
                .collect();
            if !cats.is_empty() {
                out.insert("effects".into(), json!(cats));
            }
        }
    }
    // Accept either `ledger` (CLI shape) or `decisions_and_notes` (MCP
    // shape) — both are arrays of ledger-like entries.
    let ledger_count = full
        .get("ledger")
        .or_else(|| full.get("decisions_and_notes"))
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    if ledger_count > 0 {
        out.insert("ledger_count".into(), json!(ledger_count));
    }
    Value::Object(out)
}

/// Plan F t-006: brief projection for a `conclusions_list` response.
/// Drops symbol_id (internal) and skips None role/command/tags per-entry.
pub fn brief_conclusions_list(full: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["class", "symbol", "total"] {
        if let Some(v) = full.get(k) {
            if !v.is_null() {
                out.insert(k.into(), v.clone());
            }
        }
    }
    if let Some(buckets) = full.get("buckets").and_then(Value::as_object) {
        let compact: serde_json::Map<String, Value> = buckets
            .iter()
            .map(|(stem, entries)| {
                let arr = entries.as_array().cloned().unwrap_or_default();
                let trimmed: Vec<Value> = arr
                    .iter()
                    .map(|e| {
                        let mut o = serde_json::Map::new();
                        for k in [
                            "entry_id",
                            "kind",
                            "qname",
                            "summary",
                            "role",
                            "command",
                            "created_at",
                        ] {
                            if let Some(v) = e.get(k) {
                                if !v.is_null() {
                                    o.insert(k.into(), v.clone());
                                }
                            }
                        }
                        if let Some(tags) = e.get("tags").and_then(Value::as_array) {
                            if !tags.is_empty() {
                                o.insert("tags".into(), json!(tags));
                            }
                        }
                        Value::Object(o)
                    })
                    .collect();
                (stem.clone(), json!(trimmed))
            })
            .collect();
        out.insert("buckets".into(), Value::Object(compact));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Position, Symbol, SymbolKind};

    fn sample_symbol() -> Symbol {
        Symbol {
            symbol_id: "sym_abc".into(),
            symbol_fp: "fp_xyz".into(),
            qname: "store.pricing.get_rate".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "store/pricing.py".into(),
            start: Position { line: 14, col: 1 },
            end: Position { line: 23, col: 41 },
            signature: Some("def get_rate(region: str) -> float".into()),
            doc: Some("Return the sales-tax rate for region.\nLong description.".into()),
        }
    }

    #[test]
    fn brief_symbol_drops_noise_fields() {
        let s = sample_symbol();
        let v = brief_symbol(&s);
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.get("qname").and_then(Value::as_str),
            Some("store.pricing.get_rate")
        );
        assert_eq!(
            obj.get("file").and_then(Value::as_str),
            Some("store/pricing.py:14")
        );
        for noise in [
            "symbol_id",
            "symbol_fp",
            "language",
            "kind",
            "col",
            "end",
            "start",
        ] {
            assert!(obj.get(noise).is_none(), "brief symbol must drop `{noise}`");
        }
    }

    #[test]
    fn brief_symbol_omits_empty_doc_and_signature() {
        let mut s = sample_symbol();
        s.doc = None;
        s.signature = None;
        let v = brief_symbol(&s);
        let obj = v.as_object().unwrap();
        assert!(obj.get("doc").is_none());
        assert!(obj.get("signature").is_none());
    }

    #[test]
    fn brief_call_list_compacts_to_one_liners() {
        let calls = vec![
            json!({"qname": "a.b.c", "file": "a.py", "line": 5}),
            json!({"qname": "x.y", "file": "x.py", "line": 12}),
        ];
        let out = brief_call_list(&calls);
        assert_eq!(out, vec!["a.b.c (a.py:5)", "x.y (x.py:12)"]);
    }

    #[test]
    fn brief_search_results_projects_fts_hit_shape() {
        let hits = vec![json!({
            "qname": "a.b",
            "file": "a.py",
            "line": 7,
            "signature": "def b()",
            "doc": "First line.\nSecond.",
            "bm25_score": 12.5,
            "ledger_text": "ignored noise",
        })];
        let out = brief_search_results(&hits);
        assert_eq!(out.len(), 1);
        let o = out[0].as_object().unwrap();
        assert_eq!(o.get("qname").and_then(Value::as_str), Some("a.b"));
        assert_eq!(o.get("file").and_then(Value::as_str), Some("a.py:7"));
        assert_eq!(o.get("signature").and_then(Value::as_str), Some("def b()"));
        assert_eq!(o.get("doc").and_then(Value::as_str), Some("First line."));
        assert!(o.get("score").is_some());
        assert!(o.get("ledger_text").is_none());
    }

    #[test]
    fn query_id_is_deterministic_and_starts_with_q() {
        let a = query_id("read", &["x"]);
        let b = query_id("read", &["x"]);
        assert_eq!(a, b);
        assert!(a.starts_with('Q'));
        assert_eq!(a.len(), 8);
    }

    // -- Plan F t-006: complex-handler projections ---------------------------

    #[test]
    fn brief_investigate_keeps_entry_points_and_drops_noise() {
        let full = json!({
            "query": "playhead",
            "entry_points": [{"qname":"a.b","file":"a.py","line":3,"signature":"def b()","doc":"hi"}],
            "ambiguous_terms": ["playhead"],
            "possible_misses": [{"layer":"app"}],
            "stale": null,
        });
        let v = brief_investigate(&full);
        let o = v.as_object().unwrap();
        // Token economy (1.0.80): input echo `query` is no longer
        // emitted — the agent has it.
        assert!(
            !o.contains_key("query"),
            "input echo `query` dropped by brief"
        );
        assert!(o.contains_key("entry_points"));
        assert!(!o.contains_key("ambiguous_terms"));
        assert!(!o.contains_key("possible_misses"));
        assert!(!o.contains_key("stale"), "null stale should be dropped");
    }

    #[test]
    fn brief_prepare_change_keeps_load_bearing_drops_orientation() {
        let full = json!({
            "description": "migrate tests",
            "intent": "test",
            "likely_edit_files": [{
                "file": "x.py", "why": "matched 'test'", "top_symbol": "a.b",
                "layer": "tests", "score": 3.5, "hot": false,
            }],
            "by_layer": {"tests": []},
            "recently_touched": [],
            "scenario_tests": [],
            "suggested_test_coverage": [],
            "design_invariants": [{"summary":"i1"}],
            "known_hazards": [],
            "safe_change_recipe": {"edit": []},
        });
        let v = brief_prepare_change(&full);
        let o = v.as_object().unwrap();
        assert!(o.contains_key("likely_edit_files"));
        assert!(o.contains_key("safe_change_recipe"));
        assert!(o.contains_key("design_invariants"));
        assert!(!o.contains_key("by_layer"));
        assert!(!o.contains_key("recently_touched"));
        assert!(
            !o.contains_key("known_hazards"),
            "empty array should be dropped"
        );
        // likely_edit_files entries should keep file/why/top_symbol/layer.
        let lef = o["likely_edit_files"].as_array().unwrap();
        let f0 = lef[0].as_object().unwrap();
        assert!(f0.contains_key("file"));
        assert!(f0.contains_key("why"));
        assert!(f0.contains_key("top_symbol"));
        assert!(!f0.contains_key("score"));
        assert!(!f0.contains_key("hot"));
    }

    #[test]
    fn brief_context_for_caps_callers_callees_to_top_three() {
        let full = json!({
            "symbol": {"qname":"a.b","file":"a.py","line":1,"signature":"def b()","doc":"d"},
            "callers": [
                {"qname":"c1","file":"c1.py","line":1},
                {"qname":"c2","file":"c2.py","line":2},
                {"qname":"c3","file":"c3.py","line":3},
                {"qname":"c4","file":"c4.py","line":4},
            ],
            "callees": [],
            "effects": {"declared":[{"effect":"io.fs.read"}]},
            "ledger": [{"summary":"e1"}, {"summary":"e2"}],
        });
        let v = brief_context_for(&full);
        let o = v.as_object().unwrap();
        let callers = o["callers"].as_array().unwrap();
        assert_eq!(callers.len(), 3, "callers should be capped to 3");
        assert!(
            !o.contains_key("callees"),
            "empty callees should be dropped"
        );
        assert_eq!(o["ledger_count"].as_u64(), Some(2));
        assert_eq!(o["effects"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn brief_conclusions_list_drops_internal_fields_and_nulls() {
        let full = json!({
            "class": "decisions",
            "symbol": null,
            "total": 1,
            "buckets": {
                "decisions": [{
                    "entry_id": "led_1",
                    "kind": "decision",
                    "qname": "a.b",
                    "symbol_id": "sym_secret",
                    "summary": "ok",
                    "role": null,
                    "command": null,
                    "tags": [],
                    "created_at": "2026-06-02T00:00:00Z",
                }],
            },
        });
        let v = brief_conclusions_list(&full);
        let o = v.as_object().unwrap();
        assert!(!o.contains_key("symbol"), "null symbol should be dropped");
        let bucket = o["buckets"]["decisions"].as_array().unwrap();
        let entry = bucket[0].as_object().unwrap();
        assert!(
            !entry.contains_key("symbol_id"),
            "internal symbol_id must be dropped"
        );
        assert!(!entry.contains_key("role"), "null role must be dropped");
        assert!(
            !entry.contains_key("command"),
            "null command must be dropped"
        );
        assert!(!entry.contains_key("tags"), "empty tags must be dropped");
        assert_eq!(entry["entry_id"].as_str(), Some("led_1"));
        assert_eq!(entry["kind"].as_str(), Some("decision"));
    }

    #[test]
    fn brief_from_env_reads_env_var() {
        let prev = std::env::var("ASD_FORMAT").ok();
        unsafe {
            std::env::set_var("ASD_FORMAT", "brief");
        }
        assert!(brief_from_env());
        unsafe {
            std::env::set_var("ASD_FORMAT", "BRIEF");
        }
        assert!(brief_from_env());
        unsafe {
            std::env::set_var("ASD_FORMAT", "json");
        }
        assert!(!brief_from_env());
        unsafe {
            std::env::remove_var("ASD_FORMAT");
        }
        assert!(!brief_from_env());
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("ASD_FORMAT", v);
            }
        }
    }
}
