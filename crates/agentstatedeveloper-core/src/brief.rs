//! Plan D t-001 + t-007: shared brief-output projections.
//!
//! Brief mode strips load-bearing fields out of the verbose JSON each
//! command normally emits. Lives in core so both CLI (`asd --brief`)
//! and MCP (`ASD_FORMAT=brief` at server startup) use the same
//! projections — single source of truth per the prepare_change
//! duplication-memory lesson.

use crate::schema::Symbol;
use serde_json::{json, Value};

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
        assert_eq!(obj.get("qname").and_then(Value::as_str), Some("store.pricing.get_rate"));
        assert_eq!(obj.get("file").and_then(Value::as_str), Some("store/pricing.py:14"));
        for noise in ["symbol_id", "symbol_fp", "language", "kind", "col", "end", "start"] {
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

    #[test]
    fn brief_from_env_reads_env_var() {
        let prev = std::env::var("ASD_FORMAT").ok();
        unsafe { std::env::set_var("ASD_FORMAT", "brief"); }
        assert!(brief_from_env());
        unsafe { std::env::set_var("ASD_FORMAT", "BRIEF"); }
        assert!(brief_from_env());
        unsafe { std::env::set_var("ASD_FORMAT", "json"); }
        assert!(!brief_from_env());
        unsafe { std::env::remove_var("ASD_FORMAT"); }
        assert!(!brief_from_env());
        if let Some(v) = prev {
            unsafe { std::env::set_var("ASD_FORMAT", v); }
        }
    }
}
