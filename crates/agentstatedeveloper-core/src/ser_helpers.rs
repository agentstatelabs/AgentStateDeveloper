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
}
