//! On-demand instruction disclosure (`help`) — the asd half of the
//! cross-binary help system shared with `ctx` (CTXone).
//!
//! Feature docs are compiled INTO the binary, so `help` always describes the
//! exact code that is running (version-pinned, immutable, cannot drift). They
//! are served through the MCP `help` tool and the `asd help` CLI. Lives in
//! `core` because both the `-cli` and `-mcp` crates depend on it and must
//! return byte-identical docs.
//!
//! Cross-binary contract (must match `ctx`): each binary publishes a
//! lightweight manifest (feature -> synopsis -> owner -> version) into the
//! shared index at `$AGENTSTATE_HELP_INDEX` (default
//! `$HOME/.config/agentstate/help-index.json`), keyed by tool name so `ctx`
//! and `asd` own separate slices. A unified `help` reads the union.

use serde::Serialize;
use serde_json::{Value, json};

/// Which binary owns these docs. Written into the manifest so a unified `help`
/// can route `help <feature>` to the tool that implements it.
pub const OWNER: &str = "asd";

/// Version of the running binary — stamped into every response so an agent (or
/// a stale manifest) can detect a mismatch against the code it is calling.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, Serialize)]
pub struct HelpParam {
    pub name: &'static str,
    pub required: bool,
    pub desc: &'static str,
}

/// One feature's compiled documentation. `feature` matches the tool/CLI name
/// so lookups are stable.
#[derive(Debug, Clone, Serialize)]
pub struct HelpDoc {
    pub feature: &'static str,
    pub group: &'static str,
    pub synopsis: &'static str,
    pub syntax: &'static str,
    pub params: &'static [HelpParam],
    pub examples: &'static [&'static str],
    pub gotchas: &'static [&'static str],
    pub related: &'static [&'static str],
}

macro_rules! p {
    ($name:literal, $req:literal, $desc:literal) => {
        HelpParam { name: $name, required: $req, desc: $desc }
    };
}

/// The compiled registry. Discoverability-forward hot core; the no-arg catalog
/// surfaces all of it so agents reach for features they'd otherwise miss.
pub const REGISTRY: &[HelpDoc] = &[
    // ---- orient -----------------------------------------------------------
    HelpDoc {
        feature: "architecture",
        group: "orient",
        synopsis: "One-call cold-start overview: languages, packages, layers, call-graph communities, routes, hotspots.",
        syntax: "asd architecture [--top <n=12>] [--agent]",
        params: &[
            p!("--top", false, "How many packages/hotspots to list (default 12)."),
            p!("--agent", false, "Machine-readable JSON output."),
        ],
        examples: &["asd architecture", "asd architecture --top 20 --agent"],
        gotchas: &["Read-only. Run this first in an unfamiliar repo, before touching anything."],
        related: &["map", "status", "investigate"],
    },
    HelpDoc {
        feature: "map",
        group: "orient",
        synopsis: "Initial-read project map; persists package boundaries and test-file roles as Ownership ledger entries.",
        syntax: "asd map [--agent]",
        params: &[p!("--agent", false, "Machine-readable JSON output.")],
        examples: &["asd map"],
        gotchas: &["Writes Ownership ledger entries — this is a read/orient step that also records structure."],
        related: &["architecture", "status"],
    },
    HelpDoc {
        feature: "status",
        group: "orient",
        synopsis: "Workspace index health: trust score, dirty files, concept gaps, staleness.",
        syntax: "asd status [--agent]",
        params: &[p!("--agent", false, "Machine-readable JSON output.")],
        examples: &["asd status"],
        gotchas: &["For a single go/no-go reliability number use `trust`; to refresh a stale index use `index`/`reindex`."],
        related: &["trust", "index"],
    },
    HelpDoc {
        feature: "trust",
        group: "orient",
        synopsis: "State Trust Score rollup — 'can I rely on asd right now?' as one number.",
        syntax: "asd trust [--agent]",
        params: &[p!("--agent", false, "Machine-readable JSON output.")],
        examples: &["asd trust"],
        gotchas: &["A low score means reindex or investigate before trusting asd's answers."],
        related: &["status", "scorecard"],
    },
    // ---- understand -------------------------------------------------------
    HelpDoc {
        feature: "prepare_change",
        group: "understand",
        synopsis: "One-call agent-ready context package for a planned change (investigate + impact + checklist).",
        syntax: "asd prepare-change <query> [--agent]",
        params: &[p!("query", true, "Free-text description of the change you intend to make.")],
        examples: &["asd prepare-change \"add rate limiting to the login endpoint\""],
        gotchas: &["Call this FIRST before any non-trivial change — it bundles the understand+impact+checklist steps."],
        related: &["investigate", "impact", "checklist"],
    },
    HelpDoc {
        feature: "investigate",
        group: "understand",
        synopsis: "Feature archaeology in one pass: entry points + call chains, effects, invariants, hazards.",
        syntax: "asd investigate <query> [--agent]",
        params: &[p!("query", true, "Feature/behavior to trace through the codebase.")],
        examples: &["asd investigate \"how does session token refresh work\""],
        gotchas: &["Understand before touching. For blast radius of ONE known symbol use `impact` instead."],
        related: &["prepare_change", "impact", "context_for"],
    },
    HelpDoc {
        feature: "impact",
        group: "understand",
        synopsis: "Blast-radius for ONE known symbol before editing: transitive callers, effects, invariants, tests, git touches.",
        syntax: "asd impact <symbol> [--agent]",
        params: &[p!("symbol", true, "Fully-qualified symbol name to analyze.")],
        examples: &["asd impact myapp::auth::verify_token"],
        gotchas: &["Needs a known symbol. To find the symbol first use `search`/`references`."],
        related: &["investigate", "since", "references"],
    },
    HelpDoc {
        feature: "checklist",
        group: "understand",
        synopsis: "Pre-edit checklist for a free-text query: files, invariants, tests, hazards, effects.",
        syntax: "asd checklist <query> [--agent]",
        params: &[p!("query", true, "The change/area you're about to edit.")],
        examples: &["asd checklist \"refactor the payment retry loop\""],
        gotchas: &["Lighter than `prepare_change` — no full investigate/impact bundle."],
        related: &["prepare_change", "impact"],
    },
    HelpDoc {
        feature: "context_for",
        group: "understand",
        synopsis: "Deep per-symbol context for comma-separated qnames: signature, callers/callees, effects, ledger, tests.",
        syntax: "asd context-for <qnames> [--agent]",
        params: &[p!("qnames", true, "Comma-separated fully-qualified symbol names.")],
        examples: &["asd context-for myapp::db::pool,myapp::db::migrate"],
        gotchas: &["For a whole-feature trace rather than named symbols, use `investigate`."],
        related: &["impact", "investigate", "references"],
    },
    HelpDoc {
        feature: "since",
        group: "understand",
        synopsis: "Symbols changed since a commit + combined blast radius (PR review).",
        syntax: "asd since <ref> [--agent]",
        params: &[p!("ref", true, "Git ref/commit to diff from (e.g. main, a SHA).")],
        examples: &["asd since main"],
        gotchas: &["Pairs with `impact` — this is the multi-symbol version for a whole diff."],
        related: &["impact", "task_close"],
    },
    // ---- search -----------------------------------------------------------
    HelpDoc {
        feature: "search",
        group: "search",
        synopsis: "Full ranked symbol search with confidence, uncertainty, and feedback adjustments (agent-grade).",
        syntax: "asd search <query> [--agent]",
        params: &[p!("query", true, "Concept or symbol to search for.")],
        examples: &["asd search \"rate limiter\""],
        gotchas: &["Heavier/ranked. For a lighter BM25 concept scan use `code_search`; for exact identifier hits use `references`."],
        related: &["code_search", "references", "context_for"],
    },
    HelpDoc {
        feature: "code_search",
        group: "search",
        synopsis: "Ranked concept search (FTS5/BM25) — lighter than `search`.",
        syntax: "asd search <query> [--agent]   (CLI alias: code_search; distinct MCP tool)",
        params: &[p!("query", true, "Concept text to rank against the index.")],
        examples: &["asd search \"retry backoff\""],
        gotchas: &[
            "At the CLI this is the `search` command (code_search/code_query are aliases); as an MCP tool it is distinct from `search`.",
        ],
        related: &["search", "references"],
    },
    HelpDoc {
        feature: "references",
        group: "search",
        synopsis: "Exact-identifier occurrences across the repo (rg-style completeness).",
        syntax: "asd references <identifier> [--agent]",
        params: &[p!("identifier", true, "Exact identifier to find every occurrence of.")],
        examples: &["asd references verify_token"],
        gotchas: &["Exact-match, not ranked concept search — use `search`/`code_search` for fuzzy concepts."],
        related: &["search", "impact"],
    },
    // ---- ledger -----------------------------------------------------------
    HelpDoc {
        feature: "task_close",
        group: "ledger",
        synopsis: "Write Proof/ValidationScenario ledger entries for the symbols affected by HEAD.",
        syntax: "asd task-close [--agent]",
        params: &[],
        examples: &["asd task-close"],
        gotchas: &["Run after making a change to record proof; pairs with `since` to know what HEAD touched."],
        related: &["since", "scorecard"],
    },
    // ---- meta -------------------------------------------------------------
    HelpDoc {
        feature: "help",
        group: "meta",
        synopsis: "Get exact syntax, examples, and gotchas for a feature on demand — call before using one you're unsure of.",
        syntax: "asd help [topic]",
        params: &[p!("topic", false, "Feature name or a phrase; omit to list the whole catalog.")],
        examples: &["asd help", "asd help impact", "asd help \"blast radius\""],
        gotchas: &["Docs are version-pinned to the running binary, so they can't drift from the code."],
        related: &["architecture", "status"],
    },
];

/// A score at or above this means we matched the feature NAME (exact or
/// substring) — confident enough to return that one doc. Below it, we only had
/// weak synopsis-word overlap, so we disambiguate instead of guessing.
const STRONG_MATCH: u32 = 200;

fn est_tokens(doc: &HelpDoc) -> usize {
    let mut chars = doc.synopsis.len() + doc.syntax.len();
    for p in doc.params { chars += p.name.len() + p.desc.len(); }
    for e in doc.examples { chars += e.len(); }
    for g in doc.gotchas { chars += g.len(); }
    for r in doc.related { chars += r.len(); }
    chars / 4
}

fn doc_json(doc: &HelpDoc) -> Value {
    json!({
        "feature": doc.feature,
        "owner": OWNER,
        "version": version(),
        "synopsis": doc.synopsis,
        "syntax": doc.syntax,
        "params": doc.params,
        "examples": doc.examples,
        "gotchas": doc.gotchas,
        "related": doc.related,
        "help_tokens": est_tokens(doc),
    })
}

/// Score a doc against a lowercased query. Feature-name matches dominate;
/// synopsis-word overlap only breaks ties. `group` is deliberately excluded
/// (identical across a group, it would make every sibling tie on the group word).
fn score(doc: &HelpDoc, q: &str) -> u32 {
    let feat = doc.feature.to_lowercase();
    if feat == q { return 1000; }
    let mut s = 0u32;
    if feat.contains(q) || q.contains(&feat) { s += STRONG_MATCH; }
    let syn = doc.synopsis.to_lowercase();
    for tok in q.split(|c: char| !c.is_alphanumeric()).filter(|t| t.len() > 2) {
        if feat.contains(tok) { s += 100; }
        if syn.contains(tok) { s += 20; }
    }
    s
}

/// Grouped catalog for the no-arg overview — every feature.
pub fn catalog() -> Value {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for doc in REGISTRY {
        groups.entry(doc.group).or_default().push(json!({
            "feature": doc.feature,
            "synopsis": doc.synopsis,
        }));
    }
    json!({
        "owner": OWNER,
        "version": version(),
        "usage": "Call `help <feature>` for exact syntax, examples, and gotchas before using a feature.",
        "groups": groups,
        "feature_count": REGISTRY.len(),
    })
}

/// The response for the `help` tool / CLI. `topic` None or empty -> catalog.
pub fn respond(topic: Option<&str>) -> Value {
    let q = topic.map(|t| t.trim().to_lowercase()).unwrap_or_default();
    if q.is_empty() {
        return catalog();
    }
    let mut ranked: Vec<(&HelpDoc, u32)> =
        REGISTRY.iter().map(|d| (d, score(d, &q))).filter(|(_, s)| *s > 0).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.feature.cmp(b.0.feature)));

    match ranked.first() {
        Some((doc, s)) if *s >= STRONG_MATCH => {
            let mut out = doc_json(doc);
            let also: Vec<&str> = ranked.iter().skip(1).take(4).map(|(d, _)| d.feature).collect();
            if let Some(obj) = out.as_object_mut() {
                obj.insert("also".into(), json!(also));
            }
            out
        }
        Some(_) => json!({
            "query": q,
            "owner": OWNER,
            "matches": ranked.iter().take(6).map(|(d, _)| json!({
                "feature": d.feature,
                "synopsis": d.synopsis,
            })).collect::<Vec<_>>(),
            "hint": "No exact feature match — `help <feature>` for one of these, or `help` for the full catalog.",
        }),
        None => json!({
            "not_found": q,
            "owner": OWNER,
            "did_you_mean": REGISTRY.iter().map(|d| d.feature).collect::<Vec<_>>(),
            "hint": "No asd feature matched. It may be a `ctx` (CTXone) feature — try `ctx help <topic>`.",
        }),
    }
}

/// The lightweight manifest this binary publishes to the shared help index.
pub fn manifest() -> Value {
    json!({
        "tool": OWNER,
        "version": version(),
        "features": REGISTRY.iter().map(|d| json!({
            "feature": d.feature,
            "synopsis": d.synopsis,
            "group": d.group,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_feature_name_wins() {
        let v = respond(Some("impact"));
        assert_eq!(v["feature"], "impact");
        assert_eq!(v["owner"], "asd");
        assert!(v["help_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn strong_name_match_returns_single_doc() {
        let v = respond(Some("prepare change"));
        assert_eq!(v["feature"], "prepare_change");
    }

    #[test]
    fn weak_phrase_disambiguates_not_guesses() {
        let v = respond(Some("understand the code"));
        assert!(v.get("feature").is_none(), "should not guess a single doc: {v}");
        assert!(v["matches"].is_array() || v["not_found"].is_string(), "got {v}");
    }

    #[test]
    fn empty_topic_returns_catalog() {
        let v = respond(None);
        assert_eq!(v["feature_count"].as_u64().unwrap() as usize, REGISTRY.len());
        assert!(v["groups"].is_object());
    }

    #[test]
    fn manifest_lists_every_feature() {
        let m = manifest();
        assert_eq!(m["tool"], "asd");
        assert_eq!(m["features"].as_array().unwrap().len(), REGISTRY.len());
    }
}
