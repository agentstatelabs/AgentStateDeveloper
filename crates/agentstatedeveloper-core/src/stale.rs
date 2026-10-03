//! Index entries for symbols that no longer exist.
//!
//! `/asd/v1/index/by-qname` used to only ever grow. Most of what went stale
//! was not deleted code but line-number churn: same-named symbols in one file
//! are told apart by a `:<line>` suffix (`load:412`), and the suffix is part of
//! the symbol id, so an edit above them gave each a new id and left the old
//! entry behind — with any ledger entries still attached to it.
//!
//! A run decides staleness only for what it actually looked at: entries for
//! files it parsed whose qname it did not produce, and — on a run over the
//! store's whole project — entries for files that no longer exist. A stale
//! symbol whose body reappears unchanged under a new id in the same file
//! (same kind, same base name, same body fingerprint) has moved; anything
//! else is gone.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::schema::Symbol;

/// A by-qname entry this run found stale, with its map key.
pub(crate) struct Stale {
    pub key: String,
    pub symbol: Symbol,
}

/// `name` without the `:<line>` suffix that disambiguates same-named symbols.
pub(crate) fn base_qname(qname: &str) -> &str {
    match qname.rsplit_once(':') {
        Some((base, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => qname,
    }
}

/// Entries of `by_qname` (as the run's speculation forked it) that this run
/// shows to be stale: in a file it parsed (`parsed_files`, paths as the run
/// records them) without being produced again, or in a file `is_gone` says no
/// longer exists.
pub(crate) fn find_stale(
    by_qname: &serde_json::Map<String, Value>,
    produced_qnames: &HashSet<&str>,
    parsed_files: &HashSet<String>,
    is_gone: &dyn Fn(&str) -> bool,
) -> Vec<Stale> {
    by_qname
        .iter()
        .filter(|(qname, _)| !produced_qnames.contains(qname.as_str()))
        .filter_map(|(qname, value)| {
            let symbol: Symbol = serde_json::from_value(value.clone()).ok()?;
            let in_scope = parsed_files.contains(&symbol.file) || is_gone(&symbol.file);
            in_scope.then(|| Stale {
                key: qname.clone(),
                symbol,
            })
        })
        .collect()
}

/// For each stale symbol that moved, the id it moved to: the one symbol
/// produced this run in the same file with the same kind, base name and body
/// fingerprint. Ambiguous matches are left alone.
pub(crate) fn match_moves(stale: &[Stale], produced: &[Symbol]) -> HashMap<String, String> {
    // Same file, same kind, same name but for its line suffix, same body.
    fn key(s: &Symbol) -> (&str, String, &str, &str) {
        (
            s.file.as_str(),
            format!("{:?}", s.kind),
            base_qname(&s.qname),
            s.symbol_fp.as_str(),
        )
    }
    let mut candidates: HashMap<(&str, String, &str, &str), Vec<&str>> = HashMap::new();
    for s in produced {
        candidates
            .entry(key(s))
            .or_default()
            .push(s.symbol_id.as_str());
    }
    stale
        .iter()
        .filter_map(
            |st| match candidates.get(&key(&st.symbol)).map(Vec::as_slice) {
                Some([only]) if *only != st.symbol.symbol_id => {
                    Some((st.symbol.symbol_id.clone(), only.to_string()))
                }
                _ => None,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Position, SymbolKind};

    fn sym(qname: &str, file: &str, fp: &str) -> Symbol {
        Symbol {
            symbol_id: format!("sym_{qname}_{file}"),
            symbol_fp: fp.into(),
            qname: qname.into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: file.into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 2, col: 0 },
            signature: None,
            doc: None,
        }
    }

    fn map(symbols: &[Symbol]) -> serde_json::Map<String, Value> {
        symbols
            .iter()
            .map(|s| (s.qname.clone(), serde_json::to_value(s).unwrap()))
            .collect()
    }

    #[test]
    fn base_qname_strips_only_a_line_suffix() {
        assert_eq!(base_qname("m.f:12"), "m.f");
        assert_eq!(base_qname("m.f"), "m.f");
        assert_eq!(base_qname("Foo::bar"), "Foo::bar");
        assert_eq!(base_qname("m.f:x"), "m.f:x");
    }

    #[test]
    fn only_entries_in_files_the_run_parsed_or_saw_deleted_are_stale() {
        let stored = map(&[
            sym("a.f:1", "a.py", "fp1"),
            sym("a.g", "a.py", "fp2"),
            sym("b.h", "b.py", "fp3"),
            sym("c.k", "c.py", "fp4"),
        ]);
        let produced: HashSet<&str> = ["a.g"].into();
        let parsed: HashSet<String> = ["a.py".to_string()].into();
        let gone = |f: &str| f == "c.py";
        let mut stale: Vec<_> = find_stale(&stored, &produced, &parsed, &gone)
            .into_iter()
            .map(|s| s.key)
            .collect();
        stale.sort();
        // b.py was neither parsed nor deleted: out of scope.
        assert_eq!(stale, vec!["a.f:1", "c.k"]);
    }

    #[test]
    fn a_line_shift_is_matched_by_body_and_ambiguity_is_left_alone() {
        let old = sym("m.f:5", "m.py", "body2");
        let stale = vec![Stale {
            key: old.qname.clone(),
            symbol: old.clone(),
        }];
        let moved = sym("m.f:7", "m.py", "body2");
        let other = sym("m.f:3", "m.py", "body1");
        let found = match_moves(&stale, &[moved.clone(), other.clone()]);
        assert_eq!(found.get(&old.symbol_id), Some(&moved.symbol_id));

        // Two identical bodies under the same name: no guess.
        let twin = sym("m.f:9", "m.py", "body2");
        assert!(match_moves(&stale, &[moved, twin]).is_empty());
        // A different file is not a move.
        assert!(match_moves(&stale, &[sym("m.f:7", "n.py", "body2")]).is_empty());
    }
}
