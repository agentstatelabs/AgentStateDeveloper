# ASD + CTXone — the suite

ASD and **[CTXone](https://github.com/ctxone/ctxone)** are built as a suite:
usable separately, materially better together. This is the ASD-side view of
the pairing; CTXone's mirror doc is
[ASD_INTEGRATION.md](https://github.com/ctxone/ctxone/blob/main/docs/ASD_INTEGRATION.md).

---

## Two halves of one problem

An agent editing code needs two kinds of context, and they live at different
scopes:

| | **ASD** | **CTXone** |
|---|---|---|
| Question it answers | "What does *this code* do, and what will *this change* break?" | "What has the *team* decided, and what are we doing?" |
| Scope | One repo, one developer | Across people and repos |
| Unit | Symbols, effects, call graph, impact | Facts, decisions, plans, sessions |
| Where it lives | Beside the code, in git | In the shared Hub |
| Lifetime | Regenerable from source (+ committed judgment) | Durable team memory |

They share a substrate — both build on **AgentStateGraph** — so pairing them
is natural rather than bolted-on.

---

## The joint loop

The workflow the combined suite skill teaches an agent:

1. **Orient** with ASD — `asd architecture`, `asd prepare-change`,
   `asd impact` — to understand the code and the blast radius of the change.
2. **Recall** with CTXone — `ctx recall` / `ctx context` — to pull the
   team's prior decisions and the active plan for this work.
3. **Make the change**, guided by ASD's invariants and hazards.
4. **Record the durable decision into CTXone** so the whole team inherits
   it — not just the next person to clone this one repo.
5. **Close out** with `asd task-close`, which writes proof entries for the
   symbols touched by HEAD **and tags them with the CTX plan/task
   provenance** — so a change closed in code lines up with the plan it
   belongs to in CTXone.

The division of labor: **ASD carries the code-specific judgment that belongs
in the repo; CTXone carries the team-level judgment that belongs to
everyone.** A hazard on `payments.chargeCard` is an ASD ledger entry (it
travels with that file). "We standardized on idempotency keys for all
mutating endpoints" is a CTXone memory (it applies across every repo).

---

## Cross-install & onboarding

Installing either product offers the other:

- **One-time nudge.** The first time you install ASD, it fires a single,
  dismissable suggestion to add CTXone (and vice-versa). Suppress it with
  `--no-nudge`, or the env vars `ASD_NO_SUGGEST=1` / `CTX_NO_SUGGEST=1`. It's
  a nudge, not nagware — it fires once.
- **Bootstrap installs both.** `asd bootstrap` prints a paste-to-your-agent
  block that offers to set up CTXone in the same pass (`ctx bootstrap` does
  the reverse).
- **Combined suite skill.** When both `asd` and `ctx` are on PATH,
  `asd skill` (or `ctx skill`) installs a combined **ASD + CTXone** Agent
  Skill in addition to each product's own skill. This suite skill is what
  teaches the joint loop above.

### How the combined skill stays consistent

The two CLIs don't hard-code each other's content. Each can emit its own
skill specification as JSON (`asd skill --emit-spec` / `ctx skill
--emit-spec`); on install, each reads the sibling's spec and renders the
canonical suite skill from both. The render is **order-independent** (sorted
by slug), so you get a byte-identical, idempotent result no matter which
side you install from. That's why running `asd skill` then `ctx skill` (or
the reverse) converges to the same file.

---

## What runs where

- **ASD** is local and single-repo. `asd-mcp` connects your agent to one
  repo's `.asd-state.db`; the committed sidecar travels in that repo's git.
- **CTXone** is a Hub (local or shared). Its memory graph spans projects and
  is the natural home for the cross-repo, team-scale context that ASD
  deliberately keeps out of any single repo.

When you "go team," the shared piece is CTXone — not a standalone ASD
server. ASD's own Team/Enterprise editions add the cross-repo *code*
analysis (portfolio architecture, cross-repo impact); CTXone adds the shared
*memory*. Together they're the team story. See
[LICENSING.md](../LICENSING.md) for the edition breakdown.

---

## See also

- [WALKTHROUGH.md](WALKTHROUGH.md) — the daily loop, including the suite section
- [ARCHITECTURE.md](ARCHITECTURE.md) — the shared AgentStateGraph substrate
- CTXone's [ASD_INTEGRATION.md](https://github.com/ctxone/ctxone/blob/main/docs/ASD_INTEGRATION.md) — the mirror view
