//! Plan D t-001: shared brief-output projections used by `read`, `search`,
//! `callers`, `callees`, `context-for`, etc.
//!
//! Brief mode strips load-bearing fields out of the verbose JSON each
//! command normally emits. Crucible's A/B testing showed assisted-arm
//! agents paying 2.5-5.7x baseline tokens primarily on response size.
//! Field set kept: qname, file, line, signature, doc first-line. Field set
//! dropped: symbol_id, symbol_fp, language, kind, col, end positions,
//! full doc body, empty arrays.

use agentstatedeveloper_core::Symbol;
use serde_json::{json, Value};

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
            let l = v
                .get("line")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some(format!("{q} ({f}:{l})"))
        })
        .collect()
}

/// Brief shape for `asd read`. Keeps the call graph + effect/ledger
/// summaries but as counts + compact strings, not full nested objects.
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
    // Effect categories only — drop verification / confidence / matched_policy.
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentstatedeveloper_core::{Position, Symbol, SymbolKind};

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
        assert_eq!(
            obj.get("signature").and_then(Value::as_str),
            Some("def get_rate(region: str) -> float")
        );
        assert_eq!(
            obj.get("doc").and_then(Value::as_str),
            Some("Return the sales-tax rate for region.")
        );
        // Dropped fields:
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
        assert_eq!(obj.get("qname").and_then(Value::as_str), Some("store.pricing.get_rate"));
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
    fn brief_read_drops_empty_branches() {
        let s = sample_symbol();
        let v = brief_read(&s, &[], &[], None, 0);
        let obj = v.as_object().unwrap();
        assert!(obj.get("callers").is_none());
        assert!(obj.get("callees").is_none());
        assert!(obj.get("effects").is_none());
        assert!(obj.get("ledger_count").is_none());
        // Symbol always present.
        assert!(obj.get("symbol").is_some());
    }
}
