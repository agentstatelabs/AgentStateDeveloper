# ASD Walkthrough — from install to daily use

This is the "sit down and actually use it" guide. It covers the startup
sequence, the daily loop, what happens under the covers, how ASD plugs into
your LLM, and how it works alongside CTXone. For a flat reference of every
command, see [FEATURES.md](FEATURES.md).

The one-sentence mental model: **ASD gives every function a decision
ledger, an effect declaration, and a call graph — all queryable by the
agent writing the code, all checked into git so they travel with every
clone.**

---

## 1. Startup sequence (once per repo)

```bash
# Install (macOS/Linux)
brew tap agentstatelabs/agentstatedeveloper && brew install asd
# or: curl -fsSL .../install.sh | sh

cd my-project
asd init          # create the db, update .gitignore, install git hooks
asd index .       # parse the source tree into the semantic index
asd mcp install   # register asd-mcp with your coding agents
asd skill         # install the Agent Skill (teaches the agent WHEN to use ASD)
```

Or let your agent do all of it — run `asd bootstrap` and paste the block it
prints into Claude Code / Cursor / Codex. The agent installs, indexes, and
connects ASD itself (and offers CTXone).

On a **fresh clone** of a repo that already uses ASD, it's one command:

```bash
asd onboard   # init → index → conclusions import, in the right order
```

That last step matters: `conclusions import` pulls the committed judgment
(decisions, hazards, invariants) out of `.asd/conclusions/*.jsonl` and back
into your local ledger, so you inherit the team's reasoning immediately.

---

## 2. What just happened under the covers

- **Parsing.** `asd index .` runs each source file through a **tree-sitter**
  grammar (one of 9 languages) and extracts every function, method, and
  class as a *symbol* with a stable qualified name (`payments.chargeCard`).
- **Storage.** Symbols, call-graph edges, effects, ledger entries, and an
  FTS index all live in a single local SQLite database, `.asd-state.db`
  (the ASG — Agent State Graph). It's gitignored because it's fully
  regenerable from source.
- **The call graph.** As it indexes, ASD records who-calls-whom edges, both
  within and across modules. This is what powers `callers`, `callees`, and
  `impact`.
- **Effects.** Each symbol gets an effect declaration (`io.net.out`,
  `io.db.write`, …) that propagates transitively along the call graph, so a
  caller inherits the real-world reach of everything it calls.
- **The git hooks.** `asd init` sets `core.hooksPath` to `.asd/hooks`. From
  then on, every `git commit` exports the compact sidecar; every `git pull`
  / `git checkout` imports it and reindexes. You never run these by hand.

Nothing here calls out to a network or an account. OSS ASD is entirely
local.

---

## 3. The daily loop

The rhythm ASD is built for, per unit of work:

**a. Orient (cold start on a task).**
```bash
asd architecture            # languages, layers, routes, hotspots
asd trust                   # is the index fresh enough to rely on?
asd search "charge card"    # find the relevant symbols by concept
```

**b. Scope the change before editing.**
```bash
asd prepare-change --intent "add idempotency key to card charges"
# → invariants to preserve, likely edit files, affected tests,
#   aggregated effects, recent git touches — one JSON package.
asd impact payments.chargeCard   # blast radius for a specific symbol
```
This is the highest-value moment: the agent sees the invariants and hazards
*before* it writes code, not after a test fails.

**c. Make the edit** (you or the agent), then **record the judgment** that
isn't obvious from the diff:
```bash
asd ledger append payments.chargeCard --kind hazard \
  --summary "fails silently above 10000 — caller must check return value" \
  --author-kind human --author-id alice@example.com
asd invariant add payments.chargeCard "amount must be > 0 and <= account limit"
```

**d. Close out.**
```bash
asd task-close --plan my-feature --task t-001   # proof entries for HEAD's symbols
git commit -m "feat: idempotent card charges"   # pre-commit hook exports the sidecar
```

Over time the ledger accretes the *why* behind the code — the decisions,
hazards, and invariants that a diff can never show — and every future agent
(or teammate, after a pull) inherits it.

---

## 4. How ASD plugs into your LLM

Three layers, increasing in how proactively the agent uses ASD:

1. **MCP tools** (`asd mcp install`). The agent can *call* ASD —
   `code_search`, `impact`, `ledger_append`, etc. — as structured tools.
   This is the capability layer.
2. **Always-on block** (`asd mcp instructions`). Injects a managed usage
   block into `AGENTS.md` / `CLAUDE.md` so the agent knows the tools exist
   and the house rules for using them. Idempotent.
3. **Agent Skill** (`asd skill`). A `SKILL.md` that teaches the agent *when*
   to reach for ASD — "before editing a symbol, run `impact`; record hazards
   you discover" — so the behavior is triggered, not just available.

Set `ASD_FORMAT=brief` once at agent start: read commands then project down
to load-bearing fields only (60–80% fewer tokens), which matters a lot
inside a context window.

---

## 5. What to expect over time

- **The index tracks reality** if you run `asd watch` (or rely on the
  post-merge/checkout hooks). `asd trust` tells you when it's drifted.
- **Judgment compounds.** The sidecar is small (kilobytes) and grows with
  decisions, not code size. A repo that's used ASD for months hands a cold
  agent a rich map on day one.
- **It's honest about uncertainty.** `asd trust` and the uncertainty axis
  (`low` = high confidence) tell you when *not* to lean on ASD — e.g. right
  after a big refactor before reindexing.

---

## 6. Using ASD with CTXone (the suite)

ASD is the **per-developer code-context** half. **[CTXone](https://github.com/ctxone/ctxone)**
is the **shared team memory** half. They're built to pair:

- **ASD** answers "what does *this code* do, and what will *this change*
  break?" — impact, invariants, effects, call graph. It lives beside your
  code and in git.
- **CTXone** answers "what has the *team* decided, and what are we doing?" —
  durable memory, plans, and decisions that travel across people and repos.

The joint loop: use ASD for the code specifics of a change, and record the
durable decision into CTXone so the whole team inherits it — not just the
next person to clone this one repo.

When both are installed:

- `asd skill` (or `ctx skill`) also installs a **combined suite skill** that
  teaches the agent the joint workflow.
- `asd bootstrap` offers to set up CTXone too (and vice-versa).
- Installing either one fires a one-time, dismissable nudge to add the other
  (suppress with `--no-nudge` / `ASD_NO_SUGGEST=1`).

`asd task-close` already tags its proof entries with CTX plan/task
provenance, so a change closed in ASD lines up with the plan it belongs to
in CTXone.

---

## Next steps

- [FEATURES.md](FEATURES.md) — every command and MCP tool.
- [mcp-cli-mapping.md](mcp-cli-mapping.md) — the CLI↔MCP naming reference.
- [LICENSING.md](../LICENSING.md) — OSS / Team / Enterprise editions.
