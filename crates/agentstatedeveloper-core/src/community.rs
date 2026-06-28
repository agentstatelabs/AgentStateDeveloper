//! Community detection over the call graph (Plan competitive-harvest t-009).
//!
//! Partitions symbols into functional communities — groups that call each other
//! more than they call outside the group. Used to power `asd architecture`'s
//! "clusters" (a call-graph view, complementary to the path-based layer view).
//!
//! ## Why Louvain's local-move phase, not Label Propagation
//!
//! Label propagation is simpler but *collapses on bridged cliques*: a single
//! edge between two otherwise-dense modules causes one to adopt the other's
//! label, merging them. Code call graphs are full of such thin bridges, so LPA
//! would report one giant community. The modularity gain Louvain optimizes
//! resists this — joining a dense community through one edge barely raises
//! modularity, so dense modules stay separate. We run the local-moving phase
//! (one Louvain level); the multi-level aggregation that further coarsens
//! communities is a possible future refinement.
//!
//! Deterministic: nodes are visited in input order and ties break to the
//! lowest community id, so the same graph always yields the same partition.

use std::collections::HashMap;

/// Detect communities over an undirected graph. `nodes` should be passed in a
/// stable (e.g. sorted) order for reproducibility. `edges` are unordered pairs;
/// self-loops and edges to unknown nodes are ignored, duplicates collapse.
/// Returns a dense `node → community_id` map.
pub fn detect_communities(nodes: &[String], edges: &[(String, String)]) -> HashMap<String, usize> {
    let n = nodes.len();
    let idx: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();

    // Dedup'd undirected adjacency (simple graph).
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    {
        let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
        for (a, b) in edges {
            let (Some(&ia), Some(&ib)) = (idx.get(a.as_str()), idx.get(b.as_str())) else {
                continue;
            };
            if ia == ib {
                continue;
            }
            let key = if ia < ib { (ia, ib) } else { (ib, ia) };
            if seen.insert(key) {
                adj[ia].push(ib);
                adj[ib].push(ia);
            }
        }
    }

    let deg: Vec<f64> = adj.iter().map(|a| a.len() as f64).collect();
    let two_m: f64 = deg.iter().sum();
    if two_m == 0.0 {
        // No edges — every node is its own community.
        return nodes.iter().enumerate().map(|(i, s)| (s.clone(), i)).collect();
    }

    let mut comm: Vec<usize> = (0..n).collect();
    let mut sigma_tot: Vec<f64> = deg.clone(); // community → sum of member degrees

    let mut improved = true;
    let mut guard = 0;
    while improved && guard < 50 {
        improved = false;
        guard += 1;
        for i in 0..n {
            if adj[i].is_empty() {
                continue;
            }
            let ci = comm[i];
            let ki = deg[i];
            // Detach i from its community.
            sigma_tot[ci] -= ki;
            comm[i] = usize::MAX;

            // Sum of edges from i into each candidate community (k_{i,in}).
            let mut k_in: HashMap<usize, f64> = HashMap::new();
            for &j in &adj[i] {
                if comm[j] != usize::MAX {
                    *k_in.entry(comm[j]).or_default() += 1.0;
                }
            }

            // Best community by modularity gain ∝ k_{i,in} − Σtot·k_i/(2m).
            // Start from staying in ci; ties (within ε) break to the lowest id.
            let gain = |c: usize| k_in.get(&c).copied().unwrap_or(0.0) - sigma_tot[c] * ki / two_m;
            let mut best_c = ci;
            let mut best_gain = gain(ci);
            for &c in k_in.keys() {
                let g = gain(c);
                if g > best_gain + 1e-12 || ((g - best_gain).abs() <= 1e-12 && c < best_c) {
                    best_gain = g;
                    best_c = c;
                }
            }

            comm[i] = best_c;
            sigma_tot[best_c] += ki;
            if best_c != ci {
                improved = true;
            }
        }
    }

    // Canonicalize to dense ids, assigned in first-seen (input) order.
    let mut canon: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    let mut out = HashMap::with_capacity(n);
    for (i, s) in nodes.iter().enumerate() {
        let id = *canon.entry(comm[i]).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        out.insert(s.clone(), id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }
    fn edges(es: &[(&str, &str)]) -> Vec<(String, String)> {
        es.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }
    fn num_communities(m: &HashMap<String, usize>) -> usize {
        m.values().collect::<std::collections::HashSet<_>>().len()
    }

    #[test]
    fn two_triangles_one_bridge_stay_separate() {
        // The case that breaks label propagation: two triangles joined by a
        // single edge c—d must remain two communities.
        let ns = nodes(&["a", "b", "c", "d", "e", "f"]);
        let es = edges(&[
            ("a", "b"), ("b", "c"), ("a", "c"), // triangle 1
            ("d", "e"), ("e", "f"), ("d", "f"), // triangle 2
            ("c", "d"),                         // bridge
        ]);
        let comm = detect_communities(&ns, &es);
        assert_eq!(num_communities(&comm), 2, "{comm:?}");
        assert_eq!(comm["a"], comm["b"]);
        assert_eq!(comm["b"], comm["c"]);
        assert_eq!(comm["d"], comm["e"]);
        assert_eq!(comm["e"], comm["f"]);
        assert_ne!(comm["c"], comm["d"]);
    }

    #[test]
    fn single_clique_is_one_community() {
        let ns = nodes(&["a", "b", "c", "d"]);
        let es = edges(&[
            ("a", "b"), ("a", "c"), ("a", "d"),
            ("b", "c"), ("b", "d"), ("c", "d"),
        ]);
        assert_eq!(num_communities(&detect_communities(&ns, &es)), 1);
    }

    #[test]
    fn isolated_nodes_each_their_own() {
        let ns = nodes(&["a", "b", "c"]);
        let comm = detect_communities(&ns, &[]);
        assert_eq!(num_communities(&comm), 3);
    }

    #[test]
    fn deterministic_across_runs() {
        let ns = nodes(&["a", "b", "c", "d", "e", "f"]);
        let es = edges(&[
            ("a", "b"), ("b", "c"), ("a", "c"),
            ("d", "e"), ("e", "f"), ("d", "f"),
            ("c", "d"),
        ]);
        assert_eq!(detect_communities(&ns, &es), detect_communities(&ns, &es));
    }

    #[test]
    fn ignores_self_loops_and_unknown_nodes() {
        let ns = nodes(&["a", "b"]);
        let es = edges(&[("a", "a"), ("a", "b"), ("a", "ghost")]);
        let comm = detect_communities(&ns, &es);
        assert_eq!(comm["a"], comm["b"]); // connected → same community
        assert_eq!(num_communities(&comm), 1);
    }
}
