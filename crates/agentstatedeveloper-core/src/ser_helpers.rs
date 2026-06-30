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

/// Recursive variant of `drop_empty_top_level`. Walks every nested
/// object and drops null/[]/{} children. Arrays' elements are
/// recursed into (an object inside an array still gets cleaned)
/// but the array itself is preserved at its length — position
/// often matters in arrays.
///
/// ExampleFlow refinement #1 (1.0.84): applied selectively to
/// `safe_change_recipe` in prepare-change responses. Without this,
/// `safe_change_recipe.preserve:[]`, `reference_only:[]`,
/// `likely_omitted_files:[]` etc. bloat the response on the
/// common case where the recipe has nothing to preserve / nothing
/// reference-only / nothing omitted.
///
/// Use sparingly outside of known-noisy subtrees — an empty array
/// inside a less-known object COULD be load-bearing semantics
/// ("explicit empty list" vs "no list"). For shapes you control
/// and know to be noise, this is the right tool.
pub fn drop_empty_recursive(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let cleaned: serde_json::Map<String, Value> = map
                .into_iter()
                .map(|(k, val)| (k, drop_empty_recursive(val)))
                .filter(|(_, val)| !is_empty_signal(val))
                .collect();
            Value::Object(cleaned)
        }
        Value::Array(arr) => {
            // Recurse into elements but don't drop empty array
            // entries — position in arrays carries meaning.
            Value::Array(arr.into_iter().map(drop_empty_recursive).collect())
        }
        other => other,
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

    #[test]
    fn drop_empty_recursive_strips_nested_empties() {
        // ExampleFlow refinement #1 (1.0.84): the recursive
        // variant walks into nested objects. safe_change_recipe
        // shape — many empty sibling arrays inside the recipe
        // object.
        let v = serde_json::json!({
            "safe_change_recipe": {
                "inspect": [{"file": "a.py"}],
                "preserve": [],
                "reference_only": [],
                "likely_omitted_files": [],
                "blast_radius": {
                    "total_callers": 95,
                    "callee_layer_distribution": {},
                }
            }
        });
        let out = drop_empty_recursive(v);
        let recipe = &out["safe_change_recipe"];
        assert!(recipe["inspect"].is_array());
        assert!(recipe.get("preserve").is_none(), "empty preserve dropped");
        assert!(
            recipe.get("reference_only").is_none(),
            "empty ref_only dropped"
        );
        assert!(
            recipe.get("likely_omitted_files").is_none(),
            "empty likely_omitted dropped"
        );
        assert!(
            recipe["blast_radius"]["total_callers"].is_number(),
            "non-empty kept"
        );
        assert!(
            recipe["blast_radius"]
                .get("callee_layer_distribution")
                .is_none(),
            "nested empty object dropped"
        );
    }

    #[test]
    fn drop_empty_recursive_preserves_array_positions() {
        // Arrays of objects: recurse INTO each element but don't
        // drop empty elements (position matters).
        let v = serde_json::json!({
            "items": [
                {"keep": 1, "drop_null": null},
                {"keep": 2, "drop_empty_obj": {}}
            ]
        });
        let out = drop_empty_recursive(v);
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "array length preserved");
        assert_eq!(items[0]["keep"], 1);
        assert!(items[0].get("drop_null").is_none());
        assert_eq!(items[1]["keep"], 2);
        assert!(items[1].get("drop_empty_obj").is_none());
    }
}
