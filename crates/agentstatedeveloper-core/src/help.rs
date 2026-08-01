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
        HelpParam {
            name: $name,
            required: $req,
            desc: $desc,
        }
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
            p!(
                "--top",
                false,
                "How many packages/hotspots to list (default 12)."
            ),
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
        gotchas: &[
            "Writes Ownership ledger entries — this is a read/orient step that also records structure.",
        ],
        related: &["architecture", "status"],
    },
    HelpDoc {
        feature: "status",
        group: "orient",
        synopsis: "Workspace index health: trust score, dirty files, concept gaps, staleness.",
        syntax: "asd status [--agent]",
        params: &[p!("--agent", false, "Machine-readable JSON output.")],
        examples: &["asd status"],
        gotchas: &[
            "For a single go/no-go reliability number use `trust`; to refresh a stale index use `index`/`reindex`.",
        ],
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
        params: &[p!(
            "query",
            true,
            "Free-text description of the change you intend to make."
        )],
        examples: &["asd prepare-change \"add rate limiting to the login endpoint\""],
        gotchas: &[
            "Call this FIRST before any non-trivial change — it bundles the understand+impact+checklist steps.",
        ],
        related: &["investigate", "impact", "checklist"],
    },
    HelpDoc {
        feature: "investigate",
        group: "understand",
        synopsis: "Feature archaeology in one pass: entry points + call chains, effects, invariants, hazards.",
        syntax: "asd investigate <query> [--agent]",
        params: &[p!(
            "query",
            true,
            "Feature/behavior to trace through the codebase."
        )],
        examples: &["asd investigate \"how does session token refresh work\""],
        gotchas: &[
            "Understand before touching. For blast radius of ONE known symbol use `impact` instead.",
        ],
        related: &["prepare_change", "impact", "context_for"],
    },
    HelpDoc {
        feature: "impact",
        group: "understand",
        synopsis: "Blast-radius for ONE known symbol before editing: transitive callers, effects, invariants, tests, git touches.",
        syntax: "asd impact <symbol> [--agent]",
        params: &[p!(
            "symbol",
            true,
            "Fully-qualified symbol name to analyze."
        )],
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
        params: &[p!(
            "qnames",
            true,
            "Comma-separated fully-qualified symbol names."
        )],
        examples: &["asd context-for myapp::db::pool,myapp::db::migrate"],
        gotchas: &["For a whole-feature trace rather than named symbols, use `investigate`."],
        related: &["impact", "investigate", "references"],
    },
    HelpDoc {
        feature: "since",
        group: "understand",
        synopsis: "Symbols changed since a commit + combined blast radius (PR review).",
        syntax: "asd since <ref> [--agent]",
        params: &[p!(
            "ref",
            true,
            "Git ref/commit to diff from (e.g. main, a SHA)."
        )],
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
        gotchas: &[
            "Heavier/ranked. For a lighter BM25 concept scan use `code_search`; for exact identifier hits use `references`.",
        ],
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
        params: &[p!(
            "identifier",
            true,
            "Exact identifier to find every occurrence of."
        )],
        examples: &["asd references verify_token"],
        gotchas: &[
            "Exact-match, not ranked concept search — use `search`/`code_search` for fuzzy concepts.",
        ],
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
        gotchas: &[
            "Run after making a change to record proof; pairs with `since` to know what HEAD touched.",
        ],
        related: &["since", "scorecard"],
    },
    // ---- meta -------------------------------------------------------------
    HelpDoc {
        feature: "help",
        group: "meta",
        synopsis: "Get exact syntax, examples, and gotchas for a feature on demand — call before using one you're unsure of.",
        syntax: "asd help [topic]",
        params: &[p!(
            "topic",
            false,
            "Feature name or a phrase; omit to list the whole catalog."
        )],
        examples: &["asd help", "asd help impact", "asd help \"blast radius\""],
        gotchas: &[
            "Docs are version-pinned to the running binary, so they can't drift from the code.",
        ],
        related: &["architecture", "status"],
    },
    // ---- code -------------------------------------------------------------
    HelpDoc {
        feature: "code_query",
        group: "code",
        synopsis: "Look up indexed symbols by exact structured filters (name_contains, kind, language), AND-combined, no ranking.",
        syntax: "asd search <query> [--kind <k>] [--language <l>] [--limit <n=50>]   (MCP tool: code_query)",
        params: &[
            p!("name_contains", false, "Substring match on qualified name."),
            p!(
                "kind",
                false,
                "Symbol kind: module, function, method, class, variable."
            ),
            p!("language", false, "Language filter (e.g. \"python\")."),
            p!("limit", false, "Max results (default 50)."),
        ],
        examples: &["asd search \"pool\""],
        gotchas: &[
            "No relevance ranking — for concept discovery use `code_search`; for exact identifiers use `references`.",
        ],
        related: &["code_search", "references", "search"],
    },
    HelpDoc {
        feature: "code_read",
        group: "code",
        synopsis: "Read one symbol by qname: returns its signature, effects, and ledger decisions.",
        syntax: "asd read <qname>",
        params: &[p!("qname", true, "Fully-qualified symbol name.")],
        examples: &["asd read myapp::auth::verify_token"],
        gotchas: &[
            "For DEEP context (callers/callees, covering tests, token-budgeted) use `context_for`.",
        ],
        related: &["context_for", "effects", "ledger_get"],
    },
    HelpDoc {
        feature: "callers",
        group: "code",
        synopsis: "List symbols that call the given symbol (inbound call edges, intra-module).",
        syntax: "asd callers <qname>",
        params: &[p!("qname", true, "Fully-qualified symbol name.")],
        examples: &["asd callers myapp::auth::verify_token"],
        gotchas: &[
            "Intra-module edges only; for full blast radius with effects+tests use `impact`.",
        ],
        related: &["callees", "impact", "references"],
    },
    HelpDoc {
        feature: "callees",
        group: "code",
        synopsis: "List symbols called by the given symbol (outbound call edges, intra-module).",
        syntax: "asd callees <qname>",
        params: &[p!("qname", true, "Fully-qualified symbol name.")],
        examples: &["asd callees myapp::auth::login"],
        gotchas: &["Intra-module edges only; misses dynamic dispatch and cross-language calls."],
        related: &["callers", "impact"],
    },
    HelpDoc {
        feature: "dead_code",
        group: "code",
        synopsis: "Candidate dead code: functions/methods with no inbound call edges. NOT definitive.",
        syntax: "asd dead-code [--limit <n=50>] [--include-tests] [--agent]",
        params: &[
            p!("limit", false, "Max candidates to list (default 50)."),
            p!(
                "include_tests",
                false,
                "Include test functions (excluded by default)."
            ),
        ],
        examples: &["asd dead-code"],
        gotchas: &[
            "Static graph misses reflection, framework callbacks, cross-repo API — confirm with `references` before deleting.",
        ],
        related: &["references", "callers", "endpoints"],
    },
    HelpDoc {
        feature: "endpoints",
        group: "code",
        synopsis: "List cross-service endpoints (HTTP routes/clients, pub-sub) plus in-repo matched client↔route edges.",
        syntax: "asd endpoints [--export] [--agent]",
        params: &[],
        examples: &["asd endpoints"],
        gotchas: &[
            "An unmatched client edge is a candidate contract-drift signal; cross-repo matching is Team-tier.",
        ],
        related: &["dead_code", "architecture"],
    },
    // ---- effects ----------------------------------------------------------
    HelpDoc {
        feature: "effects",
        group: "effects",
        synopsis: "Declared + transitively-inherited side effects for a symbol (I/O, network, fs, logging, mutation).",
        syntax: "effects {qname}  (MCP tool — no CLI verb)",
        params: &[p!("qname", true, "Fully-qualified symbol name.")],
        examples: &["effects {qname: \"myapp::db::pool\"}"],
        gotchas: &[
            "To check declared vs actual use `verify_effects`; to change them use `effect_declare`.",
        ],
        related: &["verify_effects", "effect_declare", "code_read"],
    },
    HelpDoc {
        feature: "effect_declare",
        group: "effects",
        synopsis: "Overwrite the declared effects list for a symbol; routes through the policy gate.",
        syntax: "effect_declare {qname, declared}  (MCP tool — no CLI verb)",
        params: &[
            p!("qname", true, "Fully-qualified symbol name."),
            p!(
                "declared",
                true,
                "List of Effect objects: { effect, qualifiers, note }."
            ),
            p!(
                "author_id",
                false,
                "Author id surfaced to the policy gate (default asd-mcp)."
            ),
        ],
        examples: &[
            "effect_declare {qname: \"myapp::io::write\", declared: [{effect: \"filesystem\"}]}",
        ],
        gotchas: &[
            "Broadening (new effect categories) uses the asd.effect.declare.broadens policy action — may be denied or flagged.",
        ],
        related: &["effects", "verify_effects"],
    },
    HelpDoc {
        feature: "verify_effects",
        group: "effects",
        synopsis: "Compare declared effects against what the static checker infers from source: ok/mismatch/unverified.",
        syntax: "asd verify-effects <qname> [--write]",
        params: &[
            p!("qname", true, "Fully-qualified symbol name."),
            p!(
                "write",
                false,
                "Persist verification result to the store (default false)."
            ),
        ],
        examples: &["asd verify-effects myapp::io::write"],
        gotchas: &[
            "unverified = source unreadable or no adapter for that language. Run before trusting `impact`'s effect set.",
        ],
        related: &["effects", "effect_declare", "impact"],
    },
    HelpDoc {
        feature: "traces",
        group: "effects",
        synopsis: "Return execution trace records stored for a symbol (written by `asd trace`), newest-first.",
        syntax: "traces {qname}  (MCP tool — read side; write via `asd trace`)",
        params: &[
            p!("qname", true, "Fully-qualified symbol name."),
            p!("limit", false, "Max trace records (default 20)."),
        ],
        examples: &["traces {qname: \"myapp::pipeline::run\"}"],
        gotchas: &[
            "Traces are produced by `asd trace` (Python only, sys.settrace); no CLI read verb.",
        ],
        related: &["effects", "verify_effects"],
    },
    // ---- ledger -----------------------------------------------------------
    HelpDoc {
        feature: "ledger_get",
        group: "ledger",
        synopsis: "List ledger entries for a symbol, newest first; superseded entries omitted by default.",
        syntax: "ledger_get {qname}  (MCP tool — CLI reads go through `asd read` / `asd list`)",
        params: &[
            p!("qname", true, "Fully-qualified symbol name."),
            p!(
                "include_superseded",
                false,
                "Include superseded entries (default false)."
            ),
        ],
        examples: &["ledger_get {qname: \"myapp::auth::verify_token\"}"],
        gotchas: &[
            "There is no ledger-get CLI verb — the CLI reads a symbol's ledger via `asd read <qname>`.",
        ],
        related: &["ledger_find", "ledger_append", "code_read"],
    },
    HelpDoc {
        feature: "ledger_find",
        group: "ledger",
        synopsis: "Search ledger entries across ALL symbols by kind/tag/author_id, without knowing the anchoring symbol.",
        syntax: "ledger_find {kind?, tag?, author_id?}  (MCP tool — no CLI verb)",
        params: &[
            p!(
                "kind",
                false,
                "Ledger kind (decision, assumption, constraint, rationale, hazard, tradeoff)."
            ),
            p!("tag", false, "Tag that must be present on the entry."),
            p!("author_id", false, "Filter by author id."),
            p!("limit", false, "Max results (default 50)."),
        ],
        examples: &["ledger_find {kind: \"hazard\"}"],
        gotchas: &[
            "To read ONE symbol's ledger use `ledger_get`; CLI reads go through `asd list`.",
        ],
        related: &["ledger_get", "ledger_append"],
    },
    HelpDoc {
        feature: "ledger_append",
        group: "ledger",
        synopsis: "Append a ledger entry to a symbol; routes through the policy gate (may deny/allow/await-approval).",
        syntax: "asd ledger append <qname> --kind <k> --summary <s> [--body ...] [--tags ...]",
        params: &[
            p!("qname", true, "Symbol the entry attaches to."),
            p!(
                "kind",
                true,
                "Ledger entry kind (decision, hazard, rationale, ...)."
            ),
            p!("summary", true, "One-line summary."),
            p!("body", false, "Free-form body (markdown ok)."),
            p!("tags", false, "Optional tags."),
        ],
        examples: &[
            "asd ledger append myapp::db::pool --kind decision --summary \"pin pool size to 8\"",
        ],
        gotchas: &[
            "Returns { entry_id, matched_policy, status }; status may be awaiting-approval — see `ledger_approve`.",
        ],
        related: &["ledger_supersede", "ledger_approve", "ledger_get"],
    },
    HelpDoc {
        feature: "ledger_approve",
        group: "ledger",
        synopsis: "Approve a ledger entry tagged `awaiting-approval`; records approved-by/approved-at.",
        syntax: "asd ledger approve <entry_id> --approver <id>",
        params: &[
            p!("entry_id", true, "Entry id from a prior ledger_append."),
            p!(
                "approver",
                true,
                "Approver identifier (recorded as approved-by:<id>)."
            ),
            p!(
                "approver_kind",
                false,
                "Must match an approver:* tag on the entry (default human)."
            ),
            p!(
                "message",
                false,
                "Optional approver rationale appended to the body."
            ),
        ],
        examples: &["asd ledger approve led-abc123 --approver alice"],
        gotchas: &["Approver kind/id must match one of the entry's original approver:* tags."],
        related: &["ledger_reject", "ledger_withdraw", "ledger_append"],
    },
    HelpDoc {
        feature: "ledger_reject",
        group: "ledger",
        synopsis: "Reject an awaiting-approval entry; records rejected-by/rejected-at with a required reason.",
        syntax: "asd ledger reject <entry_id> --reviewer <id> --reason <text>",
        params: &[
            p!("entry_id", true, "Entry id to reject."),
            p!(
                "reviewer",
                true,
                "Reviewer id (recorded as rejected-by:<id>)."
            ),
            p!(
                "reviewer_kind",
                false,
                "Same approver-match rule as approve (default human)."
            ),
            p!(
                "reason",
                true,
                "Rejection reason (required); appended to the entry body."
            ),
        ],
        examples: &[
            "asd ledger reject led-abc123 --reviewer bob --reason \"superseded by new design\"",
        ],
        gotchas: &[
            "Same approver-match rule as `ledger_approve` — reviewer must match an approver:* tag.",
        ],
        related: &["ledger_approve", "ledger_withdraw"],
    },
    HelpDoc {
        feature: "ledger_withdraw",
        group: "ledger",
        synopsis: "Withdraw an awaiting-approval entry; must be called by the original author.",
        syntax: "asd ledger withdraw <entry_id> --author-id <id>",
        params: &[
            p!("entry_id", true, "Entry id to withdraw."),
            p!(
                "author_id",
                true,
                "Must match the original entry.author.id."
            ),
        ],
        examples: &["asd ledger withdraw led-abc123 --author-id asd-mcp"],
        gotchas: &["Only the original author may withdraw; flips awaiting-approval → withdrawn."],
        related: &["ledger_approve", "ledger_reject"],
    },
    HelpDoc {
        feature: "ledger_supersede",
        group: "ledger",
        synopsis: "Append a new ledger entry that supersedes one or more existing entries for a symbol.",
        syntax: "asd ledger supersede <qname> --supersede <ids> --kind <k> --summary <s>",
        params: &[
            p!("qname", true, "Symbol the new entry attaches to."),
            p!("supersedes", true, "Entry ids superseded by the new entry."),
            p!("kind", true, "Ledger kind for the new entry."),
            p!("summary", true, "One-line summary."),
            p!("body", false, "Optional body."),
        ],
        examples: &[
            "asd ledger supersede myapp::db::pool --supersede led-old --kind decision --summary \"raise pool to 16\"",
        ],
        gotchas: &[
            "Superseded entries are filtered out of default `ledger_get` results but remain retrievable.",
        ],
        related: &["ledger_append", "ledger_get"],
    },
    HelpDoc {
        feature: "ledger_rebind",
        group: "ledger",
        synopsis: "Record a rename/move: rebind old symbol_id to the new one and re-parent all its ledger entries.",
        syntax: "asd ledger rebind --from <symbol_id> --to <qname>",
        params: &[
            p!(
                "from_symbol_id",
                true,
                "symbol_id of the old symbol to re-parent from."
            ),
            p!(
                "to_qname",
                true,
                "Qname of the new symbol to resolve and bind to."
            ),
            p!(
                "agent_id",
                false,
                "Agent/user performing the rebind (default asd-mcp)."
            ),
        ],
        examples: &["asd ledger rebind --from sym-old --to myapp::auth::verify"],
        gotchas: &[
            "Run this whenever you rename a function/class so its ledger history isn't orphaned.",
        ],
        related: &["ledger_append", "ledger_get"],
    },
    HelpDoc {
        feature: "annotate_commit",
        group: "ledger",
        synopsis: "Derive ledger annotations (decisions, invariants, hazards, proofs) from a git commit; dry-run by default.",
        syntax: "asd annotate-commit [<sha=HEAD>] [--write] [--author <id>]",
        params: &[
            p!("sha", false, "Git commit SHA (default HEAD)."),
            p!(
                "write",
                false,
                "Actually persist entries (default false — dry-run)."
            ),
            p!("author", false, "Author id (defaults to git user.name)."),
        ],
        examples: &["asd annotate-commit", "asd annotate-commit HEAD --write"],
        gotchas: &[
            "Dry-run by default — pass --write to persist. Reads changed files + commit message to resolve symbols.",
        ],
        related: &["task_close", "ledger_append"],
    },
    // ---- invariant --------------------------------------------------------
    HelpDoc {
        feature: "invariant_add",
        group: "invariant",
        synopsis: "Record an invariant that must hold at a symbol (shortcut for ledger_append kind=invariant).",
        syntax: "asd invariant add <qname> <summary>",
        params: &[
            p!("qname", true, "Fully-qualified symbol name."),
            p!("summary", true, "One-line invariant summary."),
            p!(
                "author_id",
                false,
                "Author identifier (default asd-mcp-agent)."
            ),
        ],
        examples: &["asd invariant add myapp::db::pool \"pool size must stay <= 16\""],
        gotchas: &["Invariants surface in investigate, checklist, and prepare_change outputs."],
        related: &["invariant_list", "ledger_append"],
    },
    HelpDoc {
        feature: "invariant_list",
        group: "invariant",
        synopsis: "List invariants recorded against symbols; pass qname to filter to one symbol.",
        syntax: "asd invariant list [<qname>]",
        params: &[p!(
            "qname",
            false,
            "Filter to one symbol; omit to list all."
        )],
        examples: &["asd invariant list"],
        gotchas: &[
            "Invariants are ledger entries of kind=invariant — also visible via `ledger_get`.",
        ],
        related: &["invariant_add", "ledger_get"],
    },
    // ---- feedback ---------------------------------------------------------
    HelpDoc {
        feature: "feedback_mark",
        group: "feedback",
        synopsis: "Record a verdict on a search result (useful/noisy/missing/wrong_layer/already_covered/diagnostic_only).",
        syntax: "asd feedback mark <query> <qname> <verdict>",
        params: &[
            p!("query", true, "The search query that produced this result."),
            p!("qname", true, "Symbol being rated."),
            p!(
                "verdict",
                true,
                "useful | noisy | missing | wrong_layer | already_covered | diagnostic_only."
            ),
            p!(
                "covered_by",
                false,
                "For already_covered: qname that covers this one (writes a Mapping entry)."
            ),
            p!(
                "ttl_days",
                false,
                "Optional expiry in days after which the mark stops influencing ranking."
            ),
        ],
        examples: &["asd feedback mark \"rate limiter\" myapp::mw::limit useful"],
        gotchas: &["Verdicts persist as score adjustments applied to future searches."],
        related: &["feedback_list", "feedback_promote", "search"],
    },
    HelpDoc {
        feature: "feedback_promote",
        group: "feedback",
        synopsis: "Designate a symbol as canonical source-of-truth for a concept (writes Ownership entry, 3x ranking boost).",
        syntax: "asd feedback promote-as-truth <qname> --concept <text>",
        params: &[
            p!("qname", true, "Symbol to promote."),
            p!(
                "concept",
                true,
                "The domain concept this symbol owns (e.g. \"login rate limiting\")."
            ),
            p!(
                "author_id",
                false,
                "Author identifier (default asd-mcp-agent)."
            ),
        ],
        examples: &[
            "asd feedback promote-as-truth myapp::mw::limit --concept \"login rate limiting\"",
        ],
        gotchas: &["MCP tool is feedback_promote; the CLI verb is `feedback promote-as-truth`."],
        related: &["feedback_mark", "feedback_list"],
    },
    HelpDoc {
        feature: "feedback_list",
        group: "feedback",
        synopsis: "List recorded feedback verdicts; pass qname to filter to one symbol.",
        syntax: "asd feedback list [<qname>]",
        params: &[p!(
            "qname",
            false,
            "Filter to one symbol; omit to list all."
        )],
        examples: &["asd feedback list"],
        gotchas: &["Use to audit search-quality signals before trusting or overriding rankings."],
        related: &["feedback_mark", "feedback_promote"],
    },
    // ---- scratch ----------------------------------------------------------
    HelpDoc {
        feature: "scratch_write",
        group: "scratch",
        synopsis: "Write a local-only draft scratch note scoped to a symbol/workflow/session; no policy gate.",
        syntax: "asd scratch write <content> [--symbol <qname>] [--workflow <name>] [--planning]",
        params: &[
            p!("content", true, "Working notes content (markdown ok)."),
            p!("symbol", false, "Symbol to attach the note to."),
            p!("workflow", false, "Named investigation context."),
            p!(
                "ttl_hours",
                false,
                "Time-to-live in hours; no expiry when unset."
            ),
            p!(
                "planning",
                false,
                "Mark as pre-implementation notes; adds 'planning' tag, symbol need not exist yet."
            ),
        ],
        examples: &[
            "asd scratch write \"suspect cache is drift source\" --symbol myapp::cache::get",
        ],
        gotchas: &[
            "Scratch is local-only and NOT synced by `asd sync`. Promote durable notes with `scratch_promote`.",
        ],
        related: &["scratch_list", "scratch_promote", "scratch_read"],
    },
    HelpDoc {
        feature: "scratch_list",
        group: "scratch",
        synopsis: "List scratch entries (default: draft, non-expired); filter by symbol/workflow/session/status.",
        syntax: "asd scratch list [--symbol <q>] [--workflow <w>] [--status <s>]",
        params: &[
            p!("symbol", false, "Filter by symbol qname."),
            p!("workflow", false, "Filter by workflow name."),
            p!("session", false, "Filter by session/agent_id."),
            p!(
                "status",
                false,
                "draft | promoted | discarded (default draft)."
            ),
        ],
        examples: &["asd scratch list"],
        gotchas: &["Pass status=null (omit filter) to see all statuses, not just drafts."],
        related: &["scratch_write", "scratch_read", "scratch_clean"],
    },
    HelpDoc {
        feature: "scratch_read",
        group: "scratch",
        synopsis: "Read a single scratch entry by scratch_id; returns the full ScratchEntry.",
        syntax: "asd scratch read <scratch_id>",
        params: &[p!("scratch_id", true, "Scratch entry id (scr_…).")],
        examples: &["asd scratch read scr_abc123"],
        gotchas: &["Get the id from `scratch_list`."],
        related: &["scratch_list", "scratch_update"],
    },
    HelpDoc {
        feature: "scratch_update",
        group: "scratch",
        synopsis: "Replace the content of an existing draft scratch entry entirely.",
        syntax: "asd scratch update <scratch_id> <content>",
        params: &[
            p!("scratch_id", true, "Scratch entry id to update."),
            p!(
                "content",
                true,
                "Replacement content (replaces previous entirely)."
            ),
        ],
        examples: &["asd scratch update scr_abc123 \"confirmed: cache TTL is the drift source\""],
        gotchas: &["Only draft entries are updatable; content is replaced, not appended."],
        related: &["scratch_write", "scratch_read"],
    },
    HelpDoc {
        feature: "scratch_discard",
        group: "scratch",
        synopsis: "Mark a scratch entry as discarded (soft-delete); purge later with scratch_clean.",
        syntax: "asd scratch discard <scratch_id>",
        params: &[p!("scratch_id", true, "Scratch entry id to discard.")],
        examples: &["asd scratch discard scr_abc123"],
        gotchas: &["Soft-delete only — `scratch_clean` permanently purges discarded entries."],
        related: &["scratch_clean", "scratch_list"],
    },
    HelpDoc {
        feature: "scratch_promote",
        group: "scratch",
        synopsis: "Promote a draft scratch entry to a durable ledger entry (goes through policy + audit).",
        syntax: "asd scratch promote <scratch_id> --kind <k>",
        params: &[
            p!("scratch_id", true, "Scratch entry id to promote."),
            p!(
                "kind",
                true,
                "Ledger kind (decision, hazard, invariant, proof, ...)."
            ),
            p!(
                "qname",
                false,
                "Symbol qname; required if the scratch entry has no symbol attached."
            ),
            p!(
                "summary",
                false,
                "One-line summary (defaults to first non-empty line of content)."
            ),
        ],
        examples: &["asd scratch promote scr_abc123 --kind decision --symbol myapp::cache::get"],
        gotchas: &[
            "Requires a symbol via --qname or the entry's existing symbol_id; returns { promoted_to, entry_id }.",
        ],
        related: &["scratch_write", "ledger_append"],
    },
    HelpDoc {
        feature: "scratch_clean",
        group: "scratch",
        synopsis: "Permanently delete scratch entries older than N hours matching a status filter; dry_run to preview.",
        syntax: "asd scratch clean --older-than <dur> [--status <list>] [--dry-run]",
        params: &[
            p!(
                "older_than_hours",
                true,
                "Delete entries older than this many hours."
            ),
            p!(
                "statuses",
                false,
                "Comma-separated statuses to clean (default \"discarded,promoted\")."
            ),
            p!(
                "dry_run",
                false,
                "Report what would be deleted without removing anything."
            ),
        ],
        examples: &["asd scratch clean --older-than 7d --dry-run"],
        gotchas: &[
            "Permanent delete — always dry-run first. MCP param is older_than_hours; the CLI takes a duration like 7d/24h. Defaults to discarded+promoted, sparing drafts.",
        ],
        related: &["scratch_discard", "scratch_list"],
    },
    // ---- think ------------------------------------------------------------
    HelpDoc {
        feature: "think_speculate",
        group: "think",
        synopsis: "Record a Hypothesis about a symbol — a hunch with a confidence in [0.0, 1.0].",
        syntax: "asd think speculate <qname> --conf <f> --summary <text>",
        params: &[
            p!("qname", true, "Symbol the hypothesis is about."),
            p!(
                "confidence",
                true,
                "Confidence in [0.0, 1.0]; below 0.3 is hidden from auto-surface by default."
            ),
            p!("summary", true, "One-line hunch."),
            p!("body", false, "Optional detail."),
        ],
        examples: &[
            "asd think speculate myapp::cache::get --conf 0.6 --summary \"probably the drift source\"",
        ],
        gotchas: &[
            "Marks below 0.3 are excluded from prepare_change/context_for prior-thinking. Idempotent per (qname, summary).",
        ],
        related: &["think_model", "think_list", "think_question"],
    },
    HelpDoc {
        feature: "think_model",
        group: "think",
        synopsis: "Record a MentalModel — a multi-symbol structural understanding spanning several qnames.",
        syntax: "asd think model <name> --symbols <csv> --summary <text>",
        params: &[
            p!("name", true, "Model name."),
            p!(
                "symbols",
                true,
                "Comma-separated qnames the model spans (first is anchor)."
            ),
            p!("summary", true, "The structural understanding."),
        ],
        examples: &[
            "asd think model \"auth flow\" --symbols myapp::auth::login,myapp::auth::verify --summary \"login calls verify then issues token\"",
        ],
        gotchas: &[
            "Anchored on the FIRST symbol; body carries the full list. Idempotent by (name, summary).",
        ],
        related: &["think_speculate", "think_list"],
    },
    HelpDoc {
        feature: "think_failed",
        group: "think",
        synopsis: "Record a FailedAttempt — what you tried and why it didn't work, so the next session doesn't re-tread it.",
        syntax: "asd think failed <qname> --tried <text> --because <text>",
        params: &[
            p!("qname", true, "Symbol the attempt concerned."),
            p!("tried", true, "What you tried."),
            p!("because", true, "Why it didn't work."),
        ],
        examples: &[
            "asd think failed myapp::cache::get --tried \"raise TTL\" --because \"drift persisted\"",
        ],
        gotchas: &["Idempotent by (qname, tried)."],
        related: &["think_question", "think_list"],
    },
    HelpDoc {
        feature: "think_question",
        group: "think",
        synopsis: "Record an OpenQuestion — a known unknown blocking confident action.",
        syntax: "asd think question <qname> --q <text>",
        params: &[
            p!("qname", true, "Symbol the question concerns."),
            p!("question", true, "The known unknown."),
        ],
        examples: &["asd think question myapp::cache::get --q \"is eviction concurrent-safe?\""],
        gotchas: &[
            "Be generous — every recorded question is one the next session won't re-ask. Idempotent by (qname, question).",
        ],
        related: &["think_failed", "think_list"],
    },
    HelpDoc {
        feature: "think_list",
        group: "think",
        synopsis: "List captured thinking (Hypothesis/MentalModel/FailedAttempt/OpenQuestion); filter by kind or symbol.",
        syntax: "asd think list [--kind <k>] [--symbol <qname>]",
        params: &[
            p!(
                "kind",
                false,
                "hypothesis | mental_model | failed_attempt | open_question."
            ),
            p!("symbol", false, "Filter to one symbol."),
        ],
        examples: &["asd think list"],
        gotchas: &[
            "Call when resuming work on an area to see what prior sessions concluded or got stuck on.",
        ],
        related: &[
            "think_speculate",
            "think_model",
            "think_failed",
            "think_question",
        ],
    },
    // ---- conclusions ------------------------------------------------------
    HelpDoc {
        feature: "conclusions_list",
        group: "conclusions",
        synopsis: "List ledger entries bucketed by the six conclusion classes (decisions, classifications, mappings, hazards, recipes, followups).",
        syntax: "asd conclusions list [--class <c>] [--symbol <qname>]",
        params: &[
            p!(
                "class",
                false,
                "One of: decisions | classifications | mappings | hazards | recipes | followups."
            ),
            p!("symbol", false, "Filter to one symbol qname."),
        ],
        examples: &["asd conclusions list"],
        gotchas: &["Audit what conclusions exist before exporting to .asd/conclusions/*.jsonl."],
        related: &["conclusions_export", "conclusions_import"],
    },
    HelpDoc {
        feature: "conclusions_export",
        group: "conclusions",
        synopsis: "Write all ledger conclusions to compact JSONL files (one per class) under .asd/conclusions/.",
        syntax: "asd conclusions export [--out <dir>]",
        params: &[p!(
            "out",
            false,
            "Output dir (default .asd/conclusions/ beside the db)."
        )],
        examples: &["asd conclusions export"],
        gotchas: &["Byte-stable when no new entries — safe to run from a pre-commit hook."],
        related: &["conclusions_import", "conclusions_list", "sync"],
    },
    HelpDoc {
        feature: "conclusions_import",
        group: "conclusions",
        synopsis: "Read .asd/conclusions/*.jsonl back into the local ledger; idempotent (keyed by entry_id).",
        syntax: "asd conclusions import [--in <dir>]",
        params: &[p!(
            "in",
            false,
            "Input dir with *.jsonl (default .asd/conclusions/)."
        )],
        examples: &["asd conclusions import"],
        gotchas: &[
            "Run after `git pull` or on a fresh clone to populate ASG with committed conclusions.",
        ],
        related: &["conclusions_export", "conclusions_list"],
    },
    // ---- recipes ----------------------------------------------------------
    HelpDoc {
        feature: "recipe_classify_test_migration",
        group: "recipes",
        synopsis: "Classify test-tier symbols matching a query into migration actions (Delete/Gate/Run/KeepAsCovered/Review).",
        syntax: "asd recipe classify-test-migration <query> [--limit <n=50>]",
        params: &[
            p!(
                "query",
                true,
                "Search query — finds candidate test symbols."
            ),
            p!("limit", false, "Max candidates to classify (default 50)."),
        ],
        examples: &["asd recipe classify-test-migration \"song player tests\""],
        gotchas: &["For stale test FILES that may need moving, use `recipe_migrate_stale_tests`."],
        related: &["recipe_migrate_stale_tests", "test_summary"],
    },
    HelpDoc {
        feature: "recipe_migrate_stale_tests",
        group: "recipes",
        synopsis: "Build a migration plan for stale test files; adds a Move action when a Mapping entry carries move_to.",
        syntax: "asd recipe migrate-stale-tests <query> [--limit <n=50>]",
        params: &[
            p!("query", true, "Search query — finds candidate test files."),
            p!("limit", false, "Max candidates (default 50)."),
        ],
        examples: &["asd recipe migrate-stale-tests \"song player tests\""],
        gotchas: &[
            "Same output shape as classify-test-migration plus Move; falls back to the classify tree without a move_to.",
        ],
        related: &["recipe_classify_test_migration"],
    },
    // ---- audit ------------------------------------------------------------
    HelpDoc {
        feature: "audit_tail",
        group: "audit",
        synopsis: "Read back audit events from the configured JSONL log; filter by event_type, actor, outcome, or since cursor.",
        syntax: "asd audit tail [--event-type <t>] [--actor <id>] [--outcome <o>] [--since <event_id>]",
        params: &[
            p!(
                "event_type",
                false,
                "Substring on event_type (e.g. \"ledger.\" for all ledger events)."
            ),
            p!(
                "since",
                false,
                "Return only events after this event_id (exclusive)."
            ),
            p!("actor", false, "Exact match on actor_id."),
            p!("outcome", false, "Exact match on outcome."),
            p!("limit", false, "Max events (default 200, max 1000)."),
        ],
        examples: &["asd audit tail --event-type ledger."],
        gotchas: &["Returns configured:false when ASD_AUDIT_LOG was not set at server startup."],
        related: &["audit_verify", "ledger_append"],
    },
    HelpDoc {
        feature: "audit_verify",
        group: "audit",
        synopsis: "Verify the hash-chain integrity of the configured audit log (Enterprise tier — requires asd-pro).",
        syntax: "asd audit verify",
        params: &[],
        examples: &["asd audit verify"],
        gotchas: &[
            "Commercial feature (Enterprise tier); needs asd-pro and a configured ASD_AUDIT_LOG.",
        ],
        related: &["audit_tail"],
    },
    // ---- quality ----------------------------------------------------------
    HelpDoc {
        feature: "health",
        group: "quality",
        synopsis: "Liveness check for the MCP server: status, db path, indexed symbol count, and total artifact counts.",
        syntax: "health  (MCP tool — no CLI verb)",
        params: &[],
        examples: &["health {}"],
        gotchas: &[
            "For index freshness/dirty files use `status`; for a go/no-go reliability score use `trust`.",
        ],
        related: &["status", "trust"],
    },
    HelpDoc {
        feature: "reindex",
        group: "quality",
        synopsis: "Re-parse a file or directory and refresh the symbol index, effects, and call graph.",
        syntax: "asd reindex <path>",
        params: &[p!(
            "path",
            true,
            "Absolute or relative path to a source file or directory."
        )],
        examples: &["asd reindex src/", "asd index src/"],
        gotchas: &[
            "Alias of `asd index`. After indexing, run `sync` so updated state travels with the next commit.",
        ],
        related: &["sync", "status"],
    },
    HelpDoc {
        feature: "sync",
        group: "quality",
        synopsis: "Flush live ASG state (symbols, effects, ledger, rebinds) into the .asd/v1/ sidecar for git commit.",
        syntax: "asd sync [--prune] [--dir <path>]",
        params: &[
            p!(
                "dir",
                false,
                "Project root to sync into (.asd/v1/ appended); defaults to the active db's dir."
            ),
            p!(
                "prune",
                false,
                "Also drop orphaned sidecar files for symbols that no longer exist."
            ),
        ],
        examples: &["asd sync", "asd sync --prune"],
        gotchas: &[
            "Run after `reindex` or edits so committed state stays current; the pre-commit hook runs sync --prune.",
        ],
        related: &["reindex", "conclusions_export"],
    },
    HelpDoc {
        feature: "test_summary",
        group: "quality",
        synopsis: "Summarize raw test-runner output into a compact failures-only report: { runner, passed, failed, failures[] }.",
        syntax: "cargo test 2>&1 | asd test-summary   (reads output on stdin)",
        params: &[p!(
            "output",
            true,
            "Raw test-runner output (cargo/pytest parsed precisely; others via generic scan)."
        )],
        examples: &["cargo test 2>&1 | asd test-summary"],
        gotchas: &[
            "MCP passes output as a param; the CLI reads it on stdin. Cargo and pytest are parsed precisely.",
        ],
        related: &["recipe_classify_test_migration", "scorecard"],
    },
    HelpDoc {
        feature: "scopes_list",
        group: "quality",
        synopsis: "List named scope aliases from .asd/scopes.toml — narrow noisy searches with --scope / --paths.",
        syntax: "asd scopes list",
        params: &[],
        examples: &["asd scopes list"],
        gotchas: &[
            "Scopes feed the --scope/--paths flags on search, prepare_change, investigate, impact, checklist, since.",
        ],
        related: &["search", "investigate"],
    },
    HelpDoc {
        feature: "scorecard",
        group: "quality",
        synopsis: "Five-dimension benchmark scorecard: truth, feedback, change, uncertainty, workflow — each 0-100.",
        syntax: "asd scorecard [--drill-down <dim>] [--scope <s>] [--paths <glob>]",
        params: &[
            p!("scope", false, "Named scope alias."),
            p!("paths", false, "Comma-separated glob patterns."),
            p!(
                "drill_down",
                false,
                "truth | feedback | change | uncertainty | workflow — show gap symbols."
            ),
            p!("limit", false, "Max symbols in drill-down (default 10)."),
        ],
        examples: &["asd scorecard", "asd scorecard --drill-down truth"],
        gotchas: &["Use drill_down to see which symbols are dragging a dimension down."],
        related: &["trust", "status", "test_summary"],
    },
];

/// A score at or above this means we matched the feature NAME (exact or
/// substring) — confident enough to return that one doc. Below it, we only had
/// weak synopsis-word overlap, so we disambiguate instead of guessing.
const STRONG_MATCH: u32 = 200;

fn est_tokens(doc: &HelpDoc) -> usize {
    let mut chars = doc.synopsis.len() + doc.syntax.len();
    for p in doc.params {
        chars += p.name.len() + p.desc.len();
    }
    for e in doc.examples {
        chars += e.len();
    }
    for g in doc.gotchas {
        chars += g.len();
    }
    for r in doc.related {
        chars += r.len();
    }
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
    if feat == q {
        return 1000;
    }
    let mut s = 0u32;
    if feat.contains(q) || q.contains(&feat) {
        s += STRONG_MATCH;
    }
    let syn = doc.synopsis.to_lowercase();
    for tok in q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
    {
        if feat.contains(tok) {
            s += 100;
        }
        if syn.contains(tok) {
            s += 20;
        }
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
    let mut ranked: Vec<(&HelpDoc, u32)> = REGISTRY
        .iter()
        .map(|d| (d, score(d, &q)))
        .filter(|(_, s)| *s > 0)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.feature.cmp(b.0.feature)));

    match ranked.first() {
        Some((doc, s)) if *s >= STRONG_MATCH => {
            let mut out = doc_json(doc);
            let also: Vec<&str> = ranked
                .iter()
                .skip(1)
                .take(4)
                .map(|(d, _)| d.feature)
                .collect();
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

/// Local resolve with a cross-binary proxy fallback: if `topic` doesn't match
/// any local feature, consult the shared index for other tools and ask the
/// owning binary directly (e.g. asd proxies an unknown topic to `ctx help`).
///
/// `allow_proxy` is false for the proxied child call (via `--no-proxy`), which
/// makes this collapse to a pure local `respond` — the single-hop loop guard.
/// A successful proxy returns the owner's doc annotated with `proxied_from`.
pub fn resolve(topic: Option<&str>, allow_proxy: bool) -> Value {
    let local = respond(topic);
    if !allow_proxy || local.get("not_found").is_none() {
        return local;
    }
    let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
        return local;
    };
    let Some(index) = read_index() else {
        return local;
    };
    let Some(tools) = index.get("tools").and_then(|v| v.as_object()) else {
        return local;
    };
    // Let the owning binary do its own matching — just ask each other tool.
    for tool in tools.keys().filter(|k| k.as_str() != OWNER) {
        let Some((bin, args)) = json_invocation(tool, t) else {
            continue;
        };
        let Ok(out) = std::process::Command::new(bin).args(&args).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        if let Ok(mut v) = serde_json::from_slice::<Value>(&out.stdout)
            && v.get("feature").is_some()
            && let Some(obj) = v.as_object_mut()
        {
            obj.insert("proxied_from".into(), json!(tool));
            return v;
        }
    }
    local
}

/// Read the shared cross-tool help index, if present.
fn read_index() -> Option<Value> {
    let path = if let Some(p) = std::env::var_os("AGENTSTATE_HELP_INDEX") {
        std::path::PathBuf::from(p)
    } else {
        std::path::PathBuf::from(std::env::var_os("HOME")?)
            .join(".config/agentstate/help-index.json")
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// How to invoke a given tool's `help` for JSON output, with proxy disabled so
/// the child never bounces back (single-hop guard). Returns None for unknown
/// tools (nothing to exec).
fn json_invocation(tool: &str, topic: &str) -> Option<(&'static str, Vec<String>)> {
    match tool {
        "ctx" => Some((
            "ctx",
            vec![
                "help".into(),
                topic.into(),
                "--format".into(),
                "json".into(),
                "--no-proxy".into(),
            ],
        )),
        "asd" => Some((
            "asd",
            vec![
                "help".into(),
                topic.into(),
                "--agent".into(),
                "--no-proxy".into(),
            ],
        )),
        _ => None,
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
        assert!(
            v.get("feature").is_none(),
            "should not guess a single doc: {v}"
        );
        assert!(
            v["matches"].is_array() || v["not_found"].is_string(),
            "got {v}"
        );
    }

    #[test]
    fn empty_topic_returns_catalog() {
        let v = respond(None);
        assert_eq!(
            v["feature_count"].as_u64().unwrap() as usize,
            REGISTRY.len()
        );
        assert!(v["groups"].is_object());
    }

    #[test]
    fn manifest_lists_every_feature() {
        let m = manifest();
        assert_eq!(m["tool"], "asd");
        assert_eq!(m["features"].as_array().unwrap().len(), REGISTRY.len());
    }
}
