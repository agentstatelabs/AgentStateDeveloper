//! Serde skip-predicates for agent-mode token economy.
//!
//! Principle: minimize tokens the agent must process, subject to the
//! output remaining accurate. "Accurate" means: the data is correct.
//! Not "complete" — the agent doesn't need everything in one call.
//! Not "self-describing" — the agent has docs/schema. So any field
//! whose value carries no signal the agent can't derive from other
//! emitted fields should be omitted.
//!
//! Practical rule: emit `null` / `[]` / `{}` / `0` ONLY when zero
//! itself is the signal. Otherwise omit via
//! `#[serde(skip_serializing_if = "...")]`.

use std::collections::BTreeMap;

use serde_json::Value;

/// Token economy: drop top-level fields whose value is `null`, `[]`,
/// or `{}`. The agent doesn't need the absence-of-X echo when X has
/// no signal — they can infer "no entries" / "no warning" / "fresh"
/// from absence.
///
/// Top-level only: nested empty arrays/objects can be load-bearing
/// (e.g. an empty `dropped` list inside a populated bucket says
/// something different from an absent `dropped` list).
///
/// 1.0.79: applied after `trim_for_agent` in CLI/MCP agent-mode
/// hot paths. Together with the skip-zero serde predicates from
/// 1.0.78 this catches the remaining empty-field bloat: keys
/// whose value the json! macro emitted as null/[]/{} unconditionally.
pub fn drop_empty_top_level(v: Value) -> Value {
    if let Value::Object(map) = v {
        let filtered: serde_json::Map<String, Value> = map
            .into_iter()
            .filter(|(_, val)| !is_empty_signal(val))
            .collect();
        Value::Object(filtered)
    } else {
        v
    }
}

fn is_empty_signal(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Skip a numeric counter when it's zero. Useful for fields like
/// `surfaced`, `entries_applied`, etc. — zero on these means "nothing
/// happened on this axis," which is the default state the agent can
/// infer from absence.
pub fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// Skip a BTreeMap<String, usize> when ALL its values are zero.
/// Used for `by_kind` / `by_kind_dropped` etc. — when every count
/// is zero the map is pure noise.
pub fn is_all_zero_string_usize_map(m: &BTreeMap<String, usize>) -> bool {
    m.values().all(|v| *v == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_usize_skipped() {
        assert!(is_zero_usize(&0));
        assert!(!is_zero_usize(&1));
    }

    #[test]
    fn empty_map_is_all_zero() {
        let m: BTreeMap<String, usize> = BTreeMap::new();
        assert!(is_all_zero_string_usize_map(&m));
    }

    #[test]
    fn all_zero_map_is_skipped() {
        let mut m = BTreeMap::new();
        m.insert("a".into(), 0usize);
        m.insert("b".into(), 0);
        assert!(is_all_zero_string_usize_map(&m));
    }

    #[test]
    fn map_with_any_nonzero_value_is_not_skipped() {
        let mut m = BTreeMap::new();
        m.insert("a".into(), 0usize);
        m.insert("b".into(), 1);
        assert!(!is_all_zero_string_usize_map(&m));
    }

    #[test]
    fn drop_empty_removes_null_empty_array_empty_object() {
        let v = serde_json::json!({
            "kept": 1,
            "kept_str": "value",
            "drop_null": serde_json::Value::Null,
            "drop_empty_arr": [],
            "drop_empty_obj": {},
            "kept_arr": [1],
            "kept_obj": {"x": 1},
            "kept_zero": 0,         // 0 is signal; skip predicates handle it elsewhere
            "kept_false": false,    // booleans are signal
        });
        let out = drop_empty_top_level(v);
        let obj = out.as_object().unwrap();
        assert!(obj.contains_key("kept"));
        assert!(obj.contains_key("kept_str"));
        assert!(obj.contains_key("kept_arr"));
        assert!(obj.contains_key("kept_obj"));
        assert!(obj.contains_key("kept_zero"));
        assert!(obj.contains_key("kept_false"));
        assert!(!obj.contains_key("drop_null"));
        assert!(!obj.contains_key("drop_empty_arr"));
        assert!(!obj.contains_key("drop_empty_obj"));
    }

    #[test]
    fn drop_empty_only_touches_top_level() {
        // Nested empty arrays/objects are LOAD-BEARING — keep them.
        let v = serde_json::json!({
            "wrapper": { "nested_empty": [] }
        });
        let out = drop_empty_top_level(v);
        // wrapper kept (non-empty object); nested empty array stays.
        assert!(out["wrapper"]["nested_empty"].is_array());
    }

    #[test]
    fn drop_empty_no_op_on_non_object() {
        let v = serde_json::json!([1, 2, 3]);
        let out = drop_empty_top_level(v.clone());
        assert_eq!(out, v);
    }
}
