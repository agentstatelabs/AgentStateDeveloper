# ASD Architecture — the mental model

How AgentStateDeveloper is put together, and why. Read this when you want to
understand *why* a command behaves the way it does, or before changing the
internals. For the "how do I use it" version, see
[WALKTHROUGH.md](WALKTHROUGH.md); for the command surface,
[FEATURES.md](FEATURES.md).

---

## The one big idea

Agent-authored code accumulates faster than the *reasoning behind it*. A
diff shows what changed; it never shows the invariant the author was
protecting, the hazard they discovered, or the effect a function reaches.
ASD is an **overlay** that attaches that reasoning to the code and makes it
queryable — by the same agents that write the code, and by the humans
reviewing it.

Everything else follows from one constraint: **the reasoning has to travel
with the code.** That's why the durable part is checked into git, and why
the expensive derived part is regenerable and gitignored.

---

## Layered structure

ASD is a Rust workspace. From the bottom up:

```
                 asd (CLI)      asd-mcp (stdio)     asd-serve (HTTP + Lens)
                    │                │                      │
                    └──────── surfaces over the same core ──┘
                                     │
              agentstatedeveloper-core  ── index · ledger · effects ·
                                            call graph · candidates ·
                                            calibration · recipes
                                     │
        per-language crates: python · typescript · rust · go · java ·
                             csharp · ruby · kotlin · swift   (tree-sitter
                             parsing + effect inference)
                                     │
                 AgentStateGraph (agentstategraph-*): content-addressed
                 graph + SQLite storage + policy primitive
```

- **AgentStateGraph (ASG)** is the substrate — a content-addressed graph
  with a SQLite storage backend and a policy primitive. It's the same
  primitive **CTXone** builds its memory graph on, which is why the two
  products feel like a suite rather than two unrelated tools.
- **`agentstatedeveloper-core`** turns that generic graph into *code
  context*: the semantic index, the decision ledger, effect declarations,
  the call graph, candidate scoring, calibration, and recipes.
- **Per-language crates** wrap tree-sitter grammars and know how to extract
  symbols, call edges, and inferred effects for each of the 9 languages.
  A cross-language **conformance** crate holds them to one contract (see
  the conformance matrix).
- **Surfaces** (`cli`, `mcp`, `serve`) are thin — they parse input and call
  core. `audit-pro`, `ratify`, and `pro` are the commercial crates that
  layer governance and ratification on top without forking the core.

---

## Two locations, one namespace

This is the distinction that avoids the most surprise:

| Location | What's in it | In git? | Authoritative for |
|---|---|---|---|
| `.asd-state.db` | Live SQLite ASG: index, call graph, FTS, full ledger, traces | No (gitignored) | Everything at runtime |
| `.asd/conclusions/*.jsonl` | Compact committed subset: decisions, classifications, mappings, hazards, recipes, follow-ups, agent thinking | **Yes** | What a fresh clone inherits |
| `.asd/v1/` (legacy) | Older verbose mirror | No | Vestigial; local debug only |

**The principle:** the committed sidecar carries *judgment*; everything else
is *regenerable* from source via `asd index .`. Judgment is the only thing
you can't recompute, so it's the only thing in git. The sidecar is
kilobytes because it grows with decisions, not with code size.

The git hooks (`asd init`) keep the two in sync automatically: pre-commit
exports the sidecar; post-merge / post-checkout import it and reindex.

---

## How indexing works

`asd index .` walks the source tree, and for each recognized file:

1. Parses it with the language's **tree-sitter** grammar.
2. Extracts every function / method / class as a **symbol** with a stable
   qualified name (`payments.chargeCard`).
3. Records **call-graph edges** (who calls whom), intra- and cross-module.
4. Infers and attaches **effects** (`io.net.out`, `io.db.write`, …).
5. Writes an **FTS** index for concept search.

It's idempotent and re-runnable. Unrecognized files are skipped (counted in
the summary, listed under `--verbose`). Effects propagate *transitively*
along the call graph, so a caller inherits the real-world reach of
everything it calls — that's what makes `asd impact` and `effects_of`
meaningful.

---

## Two axes that point in opposite directions

A recurring source of bugs (and a thing to internalize before touching
scoring code): ASD has two label axes that run in **opposite** directions.

- **Uncertainty axis** (`uncertainty.level`): `low` = LOW uncertainty =
  HIGH confidence. `critical` = highest uncertainty. A `low` bucket at a 95%
  pass rate is *well* calibrated.
- **Quality axis** (`result_bucket`): `core` / `strong` = good result;
  `noisy` / `weak` = bad. A `noisy` bucket at 95% *is* miscalibrated.

Any new label scheme has to declare which axis it's on. See the calibration
notes in `CLAUDE.md` for the field-data arc that established this.

---

## Multi-stage filtering

`prepare-change` filters candidates through three sequential stages, and the
score distribution looks different at each one:

1. **Symbol candidates** from `find_candidates` — FTS + hybrid boost,
   pre-aggregation.
2. **`file_scores`** — one entry per file (top contributing symbol).
3. **Post-recipe-split `likely_edit_files`** — filtered against recipe
   membership; layer/feedback-demoted files drop to `reference_only`.

**The agent only ever sees stage 3.** Any noise-floor or cliff tuning has to
be done against the distribution at the stage the user actually sees —
cutting at an earlier stage leaves the visible problem intact. This is
documented at length in `CLAUDE.md`.

---

## The five dimensions

`asd scorecard` benchmarks ASD across the five dimensions the whole system
is organized around:

- **Truth** — does the index reflect the real code?
- **Feedback** — does search improve from verdicts?
- **Change** — is pre-edit blast-radius analysis accurate?
- **Uncertainty** — does ASD know when *not* to be trusted?
- **Workflow** — is the task loop (orient → change → close) evidenced?

`asd trust` is the runtime version of this: a single rollup answering "can I
rely on ASD for the task in front of me right now?"

---

## The three integration layers (LLM-facing)

ASD meets the agent at three increasing levels of proactivity:

1. **MCP tools** — capability. The agent *can* call ASD.
2. **Always-on block** (`asd mcp instructions`) — awareness. The agent knows
   the tools exist and the house rules.
3. **Agent Skill** (`asd skill`) — behavior. The agent knows *when* to reach
   for ASD.

`ASD_FORMAT=brief` projects the high-volume read tools down to load-bearing
fields (60–80% fewer tokens) — a deliberate concession to the context
window being the scarcest resource in the loop.

---

## Where the suite fits

ASD is per-developer, single-repo, and local. Its cross-repo and team story
is **[CTXone](https://github.com/ctxone/ctxone)** — same AgentStateGraph
substrate, but holding shared team memory and plans instead of one repo's
code context. See [CTXONE_INTEGRATION.md](CTXONE_INTEGRATION.md).

---

## See also

- [FEATURES.md](FEATURES.md) — the command surface
- [WALKTHROUGH.md](WALKTHROUGH.md) — the daily loop
- [mcp-cli-mapping.md](mcp-cli-mapping.md) — the CLI↔MCP naming reference
- `CLAUDE.md` (repo root) — the load-bearing internals notes
