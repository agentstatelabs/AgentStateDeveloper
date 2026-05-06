//! Transitive effect propagation.
//!
//! Given declared effects per symbol and a call graph (callees edges),
//! compute each symbol's *transitive* effects — the union of declared
//! effects across its callees, recursively, with `via` chains pointing
//! at the immediate callee that surfaced each effect.
//!
//! The algorithm is a memoized DFS over the call graph. Cycles are
//! handled by tracking the current recursion stack: re-entering a
//! symbol that's already on the stack yields an empty contribution
//! (the partial result for that symbol is still returned via memoization
//! once the original call completes).

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::effects::EffectStore;
use crate::error::Result;
use crate::index::IndexStore;
use crate::schema::{EffectCategory, TransitiveEffect};

/// Compute transitive effects for each symbol in `symbol_ids` and write
/// them back via `effects.put_effects(...)`. Returns the count of
/// symbols whose `EffectDecl.transitive` actually changed.
pub fn propagate_transitive<I: IndexStore, E: EffectStore>(
    index: &I,
    effects: &E,
    ref_name: &str,
    symbol_ids: &[String],
) -> Result<usize> {
    // Memoization: symbol_id -> map of effect category -> set of immediate
    // callees that surfaced that effect. We keep the intermediate
    // representation (BTreeSet<String>) so we can merge across recursion
    // levels without rebuilding the via list.
    let mut memo: HashMap<String, HashMap<EffectCategory, BTreeSet<String>>> = HashMap::new();
    let mut updated: usize = 0;

    for sym in symbol_ids {
        let mut stack: HashSet<String> = HashSet::new();
        let computed = compute(index, effects, ref_name, sym, &mut memo, &mut stack)?;

        // Pull the symbol's existing EffectDecl (if any) so we can compare
        // against the freshly computed transitive set and skip writes that
        // wouldn't change anything on disk.
        let Some(mut decl) = effects.get_effects(ref_name, sym)? else {
            continue;
        };

        // Excluded categories already declared on this symbol; transitive
        // de-dups against declared.
        let declared_cats: HashSet<EffectCategory> =
            decl.declared.iter().map(|e| e.effect.clone()).collect();

        let mut new_transitive: Vec<TransitiveEffect> = computed
            .into_iter()
            .filter(|(cat, _)| !declared_cats.contains(cat))
            .map(|(cat, via_set)| TransitiveEffect {
                effect: cat,
                via: via_set.into_iter().collect(),
                qualifiers: serde_json::Value::Null,
            })
            .collect();

        // Deterministic ordering — sort by effect string then by via list.
        new_transitive.sort_by(|a, b| {
            a.effect
                .as_str()
                .cmp(b.effect.as_str())
                .then_with(|| a.via.cmp(&b.via))
        });

        if !transitive_eq(&decl.transitive, &new_transitive) {
            decl.transitive = new_transitive;
            // Use the ASD agent id for the write; the engine doesn't
            // surface a global "effect propagator" identity yet, so we
            // borrow a stable string callers can grep for.
            effects.put_effects(ref_name, sym, &decl, "asd-transitive")?;
            updated += 1;
        }
    }

    Ok(updated)
}

/// Recursive worker. Returns `transitive[sym]` as a category->via map.
/// `transitive` does NOT include `sym`'s own declared effects.
fn compute<I: IndexStore, E: EffectStore>(
    index: &I,
    effects: &E,
    ref_name: &str,
    sym: &str,
    memo: &mut HashMap<String, HashMap<EffectCategory, BTreeSet<String>>>,
    stack: &mut HashSet<String>,
) -> Result<HashMap<EffectCategory, BTreeSet<String>>> {
    if let Some(cached) = memo.get(sym) {
        return Ok(cached.clone());
    }
    if stack.contains(sym) {
        // Cycle: contribute nothing on re-entry. The first invocation
        // will eventually populate the memo entry.
        return Ok(HashMap::new());
    }
    stack.insert(sym.to_string());

    let mut acc: HashMap<EffectCategory, BTreeSet<String>> = HashMap::new();
    let callees = index.get_callees(ref_name, sym)?;

    for callee in &callees {
        // Direct contribution: the callee's *declared* effects flow up
        // to `sym` with via=[callee].
        if let Some(decl) = effects.get_effects(ref_name, callee)? {
            for e in &decl.declared {
                acc.entry(e.effect.clone())
                    .or_default()
                    .insert(callee.clone());
            }
        }

        // Indirect contribution: the callee's transitive effects also
        // flow up. We attribute via=[callee] (the immediate edge from
        // `sym`'s perspective) — the deeper chain is recoverable by
        // walking each callee's own EffectDecl.
        let callee_transitive = compute(index, effects, ref_name, callee, memo, stack)?;
        for (cat, _via) in callee_transitive {
            acc.entry(cat).or_default().insert(callee.clone());
        }
    }

    stack.remove(sym);
    memo.insert(sym.to_string(), acc.clone());
    Ok(acc)
}

/// Order-insensitive equality on TransitiveEffect lists. We already sort
/// before writing, but reads from disk may pre-date a sort fix — so be
/// defensive and compare as multisets keyed on (effect, sorted via).
fn transitive_eq(a: &[TransitiveEffect], b: &[TransitiveEffect]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let to_key = |t: &TransitiveEffect| {
        let mut via = t.via.clone();
        via.sort();
        (t.effect.clone(), via)
    };
    let mut a_keys: Vec<_> = a.iter().map(to_key).collect();
    let mut b_keys: Vec<_> = b.iter().map(to_key).collect();
    a_keys.sort();
    b_keys.sort();
    a_keys == b_keys
}
