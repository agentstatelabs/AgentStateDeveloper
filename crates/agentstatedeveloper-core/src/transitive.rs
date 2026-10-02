//! Transitive effect propagation.
//!
//! Given declared effects per symbol and a call graph (callees edges),
//! compute each symbol's *transitive* effects — the union of declared
//! effects across its callees, recursively, with `via` chains pointing
//! at the immediate callee that surfaced each effect.
//!
//! Computed per strongly connected component of the call graph (see
//! [`transitive_effects`]), so every member of a call cycle gets the same,
//! complete answer whatever order the symbols are visited in.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use std::collections::VecDeque;

use crate::effects::EffectStore;
use crate::error::Result;
use crate::index::IndexStore;
use crate::schema::{EffectCategory, EffectDecl, TransitiveEffect};

/// Compute transitive effects for each symbol in `symbol_ids` and write
/// them back via `effects.put_effects(...)`. Returns the count of
/// symbols whose `EffectDecl.transitive` actually changed.
pub fn propagate_transitive<I: IndexStore, E: EffectStore>(
    index: &I,
    effects: &E,
    ref_name: &str,
    symbol_ids: &[String],
) -> Result<usize> {
    // Load the part of the call graph these symbols reach, and the decls
    // along it — the same reads the recursive walk made, done up front.
    let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut decls: HashMap<String, EffectDecl> = HashMap::new();
    let mut queue: VecDeque<String> = symbol_ids.iter().cloned().collect();
    while let Some(sym) = queue.pop_front() {
        if callees_of.contains_key(&sym) {
            continue;
        }
        if let Some(decl) = effects.get_effects(ref_name, &sym)? {
            decls.insert(sym.clone(), decl);
        }
        let callees = index.get_callees(ref_name, &sym)?;
        queue.extend(callees.iter().cloned());
        callees_of.insert(sym, callees);
    }

    let mut updated: usize = 0;
    for (sym, new_transitive) in transitive_effects(&callees_of, &decls, symbol_ids) {
        // Compare against what is stored so unchanged decls are not
        // rewritten.
        let Some(mut decl) = decls.remove(&sym) else {
            continue;
        };
        if !transitive_eq(&decl.transitive, &new_transitive) {
            decl.transitive = new_transitive;
            // Use the ASD agent id for the write; the engine doesn't
            // surface a global "effect propagator" identity yet, so we
            // borrow a stable string callers can grep for.
            effects.put_effects(ref_name, &sym, &decl, "asd-transitive")?;
            updated += 1;
        }
    }

    Ok(updated)
}

/// Per-declarer transitive blast radius, derived entirely from the *stored*
/// transitive data written by [`propagate_transitive`] — no call-graph
/// reachability is recomputed here.
///
/// For each effect category `E` declared by at least one symbol, and each
/// declarer `S` of `E`, the blast radius of `S` is the number of distinct
/// *other* symbols whose stored `EffectDecl.transitive` carries `E` through
/// a `via` chain that reaches `S`. The `via` field on a transitive entry
/// names the immediate callee(s) that surfaced the effect, so walking
/// "who lists X in their via for E" upward from the declarer reconstructs
/// exactly the propagation paths `propagate_transitive` recorded.
///
/// Returns, per category, the declarers as `(symbol_id, blast_radius)`
/// sorted by blast radius descending (ties broken by symbol_id ascending)
/// so callers can take the top-N directly.
pub fn declared_effect_blast_radius(
    decls: &[(String, EffectDecl)],
) -> HashMap<EffectCategory, Vec<(String, usize)>> {
    // Declarers per category.
    let mut declarers: HashMap<EffectCategory, Vec<&str>> = HashMap::new();
    for (symbol_id, decl) in decls {
        for e in &decl.declared {
            declarers
                .entry(e.effect.clone())
                .or_default()
                .push(symbol_id.as_str());
        }
    }

    // Reverse propagation edges per category: child (via entry) → parents
    // that inherited the effect through that child.
    let mut inherits_via: HashMap<&EffectCategory, HashMap<&str, Vec<&str>>> = HashMap::new();
    for (symbol_id, decl) in decls {
        for t in &decl.transitive {
            let edges = inherits_via.entry(&t.effect).or_default();
            for child in &t.via {
                edges
                    .entry(child.as_str())
                    .or_default()
                    .push(symbol_id.as_str());
            }
        }
    }

    let mut out: HashMap<EffectCategory, Vec<(String, usize)>> = HashMap::new();
    for (cat, cat_declarers) in declarers {
        let edges = inherits_via.get(&cat);
        let mut ranked: Vec<(String, usize)> = cat_declarers
            .into_iter()
            .map(|declarer| {
                // BFS upward over the stored via-graph for this category.
                let mut visited: HashSet<&str> = HashSet::new();
                let mut queue: VecDeque<&str> = VecDeque::new();
                visited.insert(declarer);
                queue.push_back(declarer);
                while let Some(cur) = queue.pop_front() {
                    let Some(parents) = edges.and_then(|e| e.get(cur)) else {
                        continue;
                    };
                    for parent in parents {
                        if visited.insert(parent) {
                            queue.push_back(parent);
                        }
                    }
                }
                // Exclude the declarer itself from its own radius.
                (declarer.to_string(), visited.len() - 1)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.insert(cat, ranked);
    }
    out
}

/// Each of `symbol_ids` with its transitive effects: every effect category
/// reachable through a callee — declared by the callee or by anything it
/// reaches — with the direct callees it is reachable through (`via`), minus
/// the categories the symbol declares itself. Sorted by effect, then `via`.
///
/// Computed per strongly connected component of the call graph: every
/// member of a call cycle reaches whatever any member reaches. The memoized
/// DFS this replaces stopped at a cycle's first member, cached that partial
/// answer for the others, and visited symbols in `HashMap` order — so cycle
/// members could miss effects, and which ones changed between runs.
pub fn transitive_effects(
    callees_of: &HashMap<String, Vec<String>>,
    decls: &HashMap<String, EffectDecl>,
    symbol_ids: &[String],
) -> Vec<(String, Vec<TransitiveEffect>)> {
    let callees = |sym: &str| callees_of.get(sym).map_or(&[][..], Vec::as_slice);
    let (components, component_of) = call_graph_components(symbol_ids, callees_of);

    // Components come callees-first, so each one's callees are done before
    // it is.
    let mut reach: Vec<BTreeSet<EffectCategory>> = Vec::with_capacity(components.len());
    for (id, members) in components.iter().enumerate() {
        let mut reached = BTreeSet::new();
        for &member in members {
            if let Some(decl) = decls.get(member) {
                reached.extend(decl.declared.iter().map(|e| e.effect.clone()));
            }
            for callee in callees(member) {
                let other = component_of[callee.as_str()];
                if other != id {
                    reached.extend(reach[other].iter().cloned());
                }
            }
        }
        reach.push(reached);
    }

    symbol_ids
        .iter()
        .map(|sym| {
            let own: HashSet<&EffectCategory> = decls
                .get(sym)
                .map(|d| d.declared.iter().map(|e| &e.effect).collect())
                .unwrap_or_default();
            let mut via: BTreeMap<EffectCategory, BTreeSet<String>> = BTreeMap::new();
            for callee in callees(sym) {
                // A call to itself reaches nothing its other callees don't.
                if callee == sym {
                    continue;
                }
                for category in &reach[component_of[callee.as_str()]] {
                    if !own.contains(category) {
                        via.entry(category.clone())
                            .or_default()
                            .insert(callee.clone());
                    }
                }
            }
            let mut transitive: Vec<TransitiveEffect> = via
                .into_iter()
                .map(|(effect, via)| TransitiveEffect {
                    effect,
                    via: via.into_iter().collect(),
                    qualifiers: serde_json::Value::Null,
                })
                .collect();
            transitive.sort_by(|a, b| {
                a.effect
                    .as_str()
                    .cmp(b.effect.as_str())
                    .then_with(|| a.via.cmp(&b.via))
            });
            (sym.clone(), transitive)
        })
        .collect()
}

/// The strongly connected components of the call graph reachable from
/// `roots`, callees before callers, and each symbol's component. Tarjan's
/// algorithm, iterative so a deep call chain cannot overflow the stack.
fn call_graph_components<'a>(
    roots: &'a [String],
    callees_of: &'a HashMap<String, Vec<String>>,
) -> (Vec<Vec<&'a str>>, HashMap<&'a str, usize>) {
    #[derive(Default)]
    struct Search<'a> {
        order: HashMap<&'a str, usize>,
        low: HashMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on_stack: HashSet<&'a str>,
        // (symbol, index of its next callee to visit)
        work: Vec<(&'a str, usize)>,
    }
    impl<'a> Search<'a> {
        fn visit(&mut self, sym: &'a str) {
            let n = self.order.len();
            self.order.insert(sym, n);
            self.low.insert(sym, n);
            self.stack.push(sym);
            self.on_stack.insert(sym);
            self.work.push((sym, 0));
        }
        fn lower(&mut self, sym: &'a str, to: usize) {
            let low = self.low.get_mut(sym).expect("visited");
            *low = (*low).min(to);
        }
    }

    let mut search = Search::default();
    let mut components: Vec<Vec<&str>> = Vec::new();
    let mut component_of: HashMap<&str, usize> = HashMap::new();

    for root in roots {
        if search.order.contains_key(root.as_str()) {
            continue;
        }
        search.visit(root.as_str());
        while let Some(&(sym, next)) = search.work.last() {
            let callees: &'a [String] = callees_of.get(sym).map_or(&[], Vec::as_slice);
            if let Some(callee) = callees.get(next) {
                search.work.last_mut().expect("non-empty").1 += 1;
                let callee = callee.as_str();
                if !search.order.contains_key(callee) {
                    search.visit(callee);
                } else if search.on_stack.contains(callee) {
                    let reached = search.order[callee];
                    search.lower(sym, reached);
                }
                continue;
            }
            search.work.pop();
            if let Some(&(caller, _)) = search.work.last() {
                let reached = search.low[sym];
                search.lower(caller, reached);
            }
            if search.low[sym] == search.order[sym] {
                let id = components.len();
                let mut members = Vec::new();
                loop {
                    let member = search.stack.pop().expect("on the stack");
                    search.on_stack.remove(member);
                    component_of.insert(member, id);
                    members.push(member);
                    if member == sym {
                        break;
                    }
                }
                components.push(members);
            }
        }
    }
    (components, component_of)
}

/// Order-insensitive equality on TransitiveEffect lists. We already sort
/// before writing, but reads from disk may pre-date a sort fix — so be
/// defensive and compare as multisets keyed on (effect, sorted via).
pub(crate) fn transitive_eq(a: &[TransitiveEffect], b: &[TransitiveEffect]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Effect;

    fn graph(edges: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        let mut g: HashMap<String, Vec<String>> = HashMap::new();
        for (from, to) in edges {
            g.entry(from.to_string()).or_default().push(to.to_string());
        }
        g
    }

    fn decls(declared: &[(&str, &[EffectCategory])]) -> HashMap<String, EffectDecl> {
        declared
            .iter()
            .map(|(sym, cats)| {
                let decl = EffectDecl {
                    symbol_id: sym.to_string(),
                    declared: cats.iter().cloned().map(Effect::new).collect(),
                    transitive: Vec::new(),
                    verification: None,
                    confidence: None,
                    runtime: None,
                    matched_policy: None,
                };
                (sym.to_string(), decl)
            })
            .collect()
    }

    /// symbol -> [(effect, via)], for comparing results.
    fn summary(
        out: Vec<(String, Vec<TransitiveEffect>)>,
    ) -> BTreeMap<String, Vec<(String, Vec<String>)>> {
        out.into_iter()
            .map(|(sym, t)| {
                let t = t
                    .into_iter()
                    .map(|t| (t.effect.as_str().to_string(), t.via))
                    .collect();
                (sym, t)
            })
            .collect()
    }

    fn ids(order: &[&str]) -> Vec<String> {
        order.iter().map(|s| s.to_string()).collect()
    }

    /// a -> b -> a, and b -> t, which writes to the network. Both a and b
    /// reach it. The memoized DFS gave whichever of a and b it reached
    /// second nothing, depending on visit order.
    #[test]
    fn every_member_of_a_cycle_reaches_what_the_cycle_reaches() {
        let g = graph(&[("a", "b"), ("b", "a"), ("b", "t")]);
        let d = decls(&[("a", &[]), ("b", &[]), ("t", &[EffectCategory::IoNetOut])]);
        let expected = BTreeMap::from([
            (
                "a".to_string(),
                vec![("io.net.out".to_string(), ids(&["b"]))],
            ),
            (
                "b".to_string(),
                vec![("io.net.out".to_string(), ids(&["a", "t"]))],
            ),
            ("t".to_string(), vec![]),
        ]);
        for order in [
            ["a", "b", "t"],
            ["a", "t", "b"],
            ["b", "a", "t"],
            ["b", "t", "a"],
            ["t", "a", "b"],
            ["t", "b", "a"],
        ] {
            assert_eq!(
                summary(transitive_effects(&g, &d, &ids(&order))),
                expected,
                "visited in order {order:?}"
            );
        }
    }

    #[test]
    fn recursion_adds_nothing_via_itself() {
        let g = graph(&[("s", "s"), ("s", "t")]);
        let d = decls(&[("s", &[]), ("t", &[EffectCategory::IoNetOut])]);
        assert_eq!(
            summary(transitive_effects(&g, &d, &ids(&["s"])))["s"],
            vec![("io.net.out".to_string(), ids(&["t"]))]
        );
    }

    #[test]
    fn a_symbols_own_effects_are_not_transitive() {
        let g = graph(&[("a", "b")]);
        let d = decls(&[
            ("a", &[EffectCategory::IoNetOut]),
            ("b", &[EffectCategory::IoNetOut, EffectCategory::IoFsRead]),
        ]);
        assert_eq!(
            summary(transitive_effects(&g, &d, &ids(&["a"])))["a"],
            vec![("io.fs.read".to_string(), ids(&["b"]))]
        );
    }

    /// The recursive walk overflowed the stack on a deep enough chain.
    #[test]
    fn a_deep_call_chain_does_not_overflow_the_stack() {
        const DEPTH: usize = 200_000;
        let names: Vec<String> = (0..DEPTH).map(|i| format!("n{i}")).collect();
        let mut g: HashMap<String, Vec<String>> = HashMap::new();
        for pair in names.windows(2) {
            g.insert(pair[0].clone(), vec![pair[1].clone()]);
        }
        let d = decls(&[(names[DEPTH - 1].as_str(), &[EffectCategory::IoNetOut])]);
        let out = transitive_effects(&g, &d, &names[..1]);
        assert_eq!(
            summary(out)["n0"],
            vec![("io.net.out".to_string(), ids(&["n1"]))]
        );
    }
}
