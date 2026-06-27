//! Intra-process data-flow edges (Plan competitive-harvest t-002, slice 4).
//!
//! Distinct from the contract-keyed cross-service layer: a data-flow edge is a
//! direct symbol→symbol link *within one process* recording that a value at a
//! call site flows into a callee's parameter (`arg → param`). No contract, no
//! cross-repo matching — it's the "argument-to-parameter" half of what
//! codebase-memory-mcp calls `DATA_FLOWS`.
//!
//! Detection is split like the endpoint layer: a language adapter reports
//! [`DetectedDataFlow`] (call site + argument + position, by qname); the index
//! pipeline resolves the callee's parameter name (from its signature) and
//! symbol identity to produce a [`DataFlowEdge`].

use serde::{Deserialize, Serialize};

/// How a value reaches its sink. Currently only argument→parameter; field-access
/// chains are a planned addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataFlowKind {
    /// A call-site argument flowing into the callee's parameter.
    ArgToParam,
}

/// A resolved data-flow edge: a value flows from a call site in `from` into
/// `param` of the callee `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataFlowEdge {
    pub kind: DataFlowKind,
    pub from_symbol_id: String,
    pub from_qname: String,
    pub to_symbol_id: String,
    pub to_qname: String,
    /// The callee parameter the value flows into.
    pub param: String,
    /// The argument expression at the call site (a simple identifier today).
    pub arg: String,
    pub file: String,
    pub line: u32,
    pub confidence: f64,
}

/// A data-flow site detected by an adapter, before the pipeline resolves the
/// callee's parameter name and symbol identity. The adapter knows the callee by
/// qname and the argument's *position*; the pipeline maps position → param name
/// using the callee's signature.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedDataFlow {
    pub caller_qname: String,
    pub callee_qname: String,
    /// Zero-based positional index of the argument.
    pub arg_index: usize,
    /// The argument expression (a simple identifier).
    pub arg: String,
    pub file: String,
    pub line: u32,
    pub confidence: f64,
}

/// Extract positional parameter names from a function signature, e.g.
/// `"def get_user(id, name=None, *args)"` → `["id", "name"]`. Skips
/// `*args`/`**kwargs` and the `self`/`cls` receiver. Language-agnostic enough
/// for the common `name(params)` shape.
pub fn parse_params(signature: &str) -> Vec<String> {
    let Some(open) = signature.find('(') else {
        return Vec::new();
    };
    let after = &signature[open + 1..];
    let Some(close) = matching_paren(after) else {
        return Vec::new();
    };
    let inner = &after[..close];
    split_top_level(inner)
        .into_iter()
        .filter_map(|p| {
            let p = p.trim();
            // *args / **kwargs receive no single positional value.
            if p.starts_with('*') {
                return None;
            }
            // Strip type annotation / default: `id: int = 3` → `id`.
            let name = p.split([':', '=']).next().unwrap_or("").trim();
            if name.is_empty() || name == "self" || name == "cls" {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Split on commas at bracket-depth 0, so commas inside a default value's
/// `foo(1, 2)` / `[a, b]` don't split the parameter list.
fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].to_string());
    out
}

/// Index of the `)` matching the first unconsumed `(` — i.e. paren-depth aware,
/// so nested parens in defaults/annotations don't end the list early.
fn matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => return Some(i),
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Resolve a detected data-flow site into a [`DataFlowEdge`], given a way to
/// look up a qname's symbol_id and a callee's parameter list. Returns `None`
/// when the caller or callee can't be resolved, or the arg position has no
/// corresponding parameter.
pub fn resolve_edge(
    det: &DetectedDataFlow,
    sym_id_of: impl Fn(&str) -> Option<String>,
    params_of: impl Fn(&str) -> Option<Vec<String>>,
) -> Option<DataFlowEdge> {
    let from_symbol_id = sym_id_of(&det.caller_qname)?;
    let to_symbol_id = sym_id_of(&det.callee_qname)?;
    let params = params_of(&det.callee_qname)?;
    let param = params.get(det.arg_index)?.clone();
    Some(DataFlowEdge {
        kind: DataFlowKind::ArgToParam,
        from_symbol_id,
        from_qname: det.caller_qname.clone(),
        to_symbol_id,
        to_qname: det.callee_qname.clone(),
        param,
        arg: det.arg.clone(),
        file: det.file.clone(),
        line: det.line,
        confidence: det.confidence,
    })
}

/// Flatten the on-disk data-flow registry (`from_symbol_id → [DataFlowEdge]`)
/// into a flat list. Malformed entries are skipped.
pub fn edges_from_tree(tree: &serde_json::Value) -> Vec<DataFlowEdge> {
    let mut out = Vec::new();
    let Some(by_from) = tree.as_object() else {
        return out;
    };
    for arr in by_from.values() {
        let Some(arr) = arr.as_array() else {
            continue;
        };
        for e in arr {
            if let Ok(edge) = serde_json::from_value::<DataFlowEdge>(e.clone()) {
                out.push(edge);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_params_basic_and_annotated() {
        assert_eq!(parse_params("def f(a, b, c)"), vec!["a", "b", "c"]);
        assert_eq!(parse_params("def get_user(id: int, name: str = \"x\")"), vec!["id", "name"]);
    }

    #[test]
    fn parse_params_skips_receiver_and_varargs() {
        assert_eq!(parse_params("def m(self, x, *args, **kwargs)"), vec!["x"]);
        assert_eq!(parse_params("def c(cls, y)"), vec!["y"]);
    }

    #[test]
    fn parse_params_handles_nested_parens_in_defaults() {
        // The default value's parens must not truncate the list.
        assert_eq!(parse_params("def f(a, b=foo(1, 2), c)"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_params_empty_and_malformed() {
        assert!(parse_params("def f()").is_empty());
        assert!(parse_params("no parens here").is_empty());
    }

    #[test]
    fn resolve_edge_maps_position_to_param() {
        let det = DetectedDataFlow {
            caller_qname: "m.caller".into(),
            callee_qname: "m.get_user".into(),
            arg_index: 0,
            arg: "uid".into(),
            file: "m.py".into(),
            line: 4,
            confidence: 0.8,
        };
        let edge = resolve_edge(
            &det,
            |q| Some(format!("sym_{q}")),
            |q| (q == "m.get_user").then(|| vec!["id".to_string(), "name".to_string()]),
        )
        .expect("edge resolves");
        assert_eq!(edge.from_symbol_id, "sym_m.caller");
        assert_eq!(edge.to_symbol_id, "sym_m.get_user");
        assert_eq!(edge.param, "id");
        assert_eq!(edge.arg, "uid");
    }

    #[test]
    fn resolve_edge_none_when_arg_position_has_no_param() {
        let det = DetectedDataFlow {
            caller_qname: "m.caller".into(),
            callee_qname: "m.f".into(),
            arg_index: 5, // out of range
            arg: "x".into(),
            file: "m.py".into(),
            line: 1,
            confidence: 0.8,
        };
        assert!(
            resolve_edge(&det, |q| Some(q.to_string()), |_| Some(vec!["only".to_string()])).is_none()
        );
    }

    #[test]
    fn resolve_edge_none_when_callee_unresolved() {
        let det = DetectedDataFlow {
            caller_qname: "m.caller".into(),
            callee_qname: "external.thing".into(),
            arg_index: 0,
            arg: "x".into(),
            file: "m.py".into(),
            line: 1,
            confidence: 0.8,
        };
        // params_of returns None for the unknown callee.
        assert!(resolve_edge(&det, |q| Some(q.to_string()), |_| None).is_none());
    }
}
