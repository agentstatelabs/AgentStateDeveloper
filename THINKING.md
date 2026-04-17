# Agent Programming Layer — Initial Thinking

## The problem, restated

Programming languages were designed for humans reading/writing text. Two costs when an agent is the primary author:

1. **Memory rot.** Agents externalize plans into `.md` files. Prose isn't verified, so it drifts from code the moment it's written. Every LLM call re-sends bloated, partially-wrong context because nothing is canonical.
2. **Context extraction tax.** Source code doesn't carry intent, invariants, effects, or decision history — the agent re-derives those from scratch on every task by reading files, running greps, and guessing.

ASG (AgentStateGraph, presumably) attacks this by making the *representation itself* machine-native. But most real codebases won't be ASG — they'll be Python, TypeScript, Go. So: **what layer(s) can sit alongside an ordinary language and give the agent the same benefits?**

## The core principle

> Context that isn't mechanically verified will rot.

Every useful layer here is either (a) **executable/checkable** (fails loudly on drift), or (b) **derived from truth** (regenerated on demand from code/tests/runs). Prose tucked into a file is neither, which is why `.md` plans rot.

Anything we add must obey this rule or it becomes another form of the problem.

## Candidate layers (ranked by what I'd build first)

### 1. Code-derived semantic index (the "freshness layer")
A queryable graph built from AST + static analysis + type info: call graph, data flow, effects, module boundaries, symbol definitions and references.

- **Anti-rot:** regenerated from source on every change. Cannot lie.
- **Agent benefit:** replaces "read 40 files to understand this module" with a targeted query. Dramatic context reduction per LLM call.
- **Prior art:** LSP, Sourcegraph, Glean, tree-sitter + custom indexer. None target agents; they target IDE UX.
- **Gap to fill:** shape the query surface for *agent* questions ("what calls this with untrusted input?", "what would break if I change this signature?", "what's the minimum surface to understand this change?").

### 2. Verifiable intent annotations (the "contract layer")
Structured (not prose) annotations attached to functions/modules: pre/post conditions, invariants, example inputs, effect declarations. Checked by tooling — either statically, via property tests, or at runtime in a check mode.

- **Anti-rot:** enforced. Drift → failing build.
- **Agent benefit:** intent is co-located with code and trustworthy. Agent doesn't have to guess from names + call sites.
- **Prior art:** refinement types, Design-by-Contract, Hypothesis/QuickCheck, JML. Mostly ignored by humans because the cost/benefit is bad for human authors. For *agents*, the cost is near zero — they'll happily write contracts.
- **Key insight:** this is the thing that flips from "unused by humans" to "essential for agents" because the writer changed.

### 3. Execution-trace context (the "evidence layer")
Capture concrete observed behavior from tests and runs: input/output pairs, state transitions, which branches fire, which invariants held. Store attached to the relevant symbol.

- **Anti-rot:** regenerated on every test run. Stale traces get replaced, not patched.
- **Agent benefit:** "what does this actually do" answered with evidence, not prose inference. Especially valuable for gnarly code where reading ≠ understanding.
- **Prior art:** time-travel debuggers (rr, Replay.io), observability traces. Not indexed for agents.

### 4. Decision ledger (the "why layer")
Append-only, structured record of design decisions keyed to code spans: *"this function uses a mutex because we observed a race in incident X."* New decisions supersede rather than mutate — ledger is diffable, not prose.

- **Anti-rot:** append-only + linked to code spans. When code moves, the ledger entry either follows (via symbol identity) or surfaces as orphaned and prompts review.
- **Agent benefit:** answers "why is it this way?" — the single biggest question the agent can't recover from code alone.
- **This is the closest cousin to today's `.md` plans**, but structured and span-linked instead of a free-text scroll.

### 5. Effect / capability manifest (the "blast radius layer")
Functions declare what they touch: IO, network, DB, env, time, randomness, shared state. Declared effects are checked (like Haskell's IO, or Rust's `unsafe`, but broader and required).

- **Anti-rot:** enforced by type system or linter.
- **Agent benefit:** the agent can reason about *safety of a change* without reading bodies. Critical for autonomous refactors — "can I move this call?" becomes a query, not a whole-codebase audit.

### 6. Test-as-spec (the "executable requirement" layer)
Property tests and example tables *are* the spec. No separate requirements doc. Tests live next to code and are the context the agent reads for "what should this do."

- **Anti-rot:** tests either pass or don't.
- **Not new** — BDD tried this for humans. Humans find it verbose. Agents don't mind verbose.

## What I think is the highest-leverage first build

Two layers compose into something genuinely useful:

**Semantic index (1) + Decision ledger (4).**

- The index gives the agent *what the code is* without re-reading.
- The ledger gives the agent *why it is that way* without relying on rotting prose.
- Together they replace the two largest context sinks in agent workflows.

Contracts (2) and effects (5) are more powerful but require tooling investment and language cooperation. Index + ledger can be built as external sidecars that work with any language.

## Key design questions I'd want your steer on

1. **Target:** a prototype on one language (Python or TypeScript) to prove the idea, or a language-agnostic spec first?
2. **Scope:** do we assume the agent is the sole author, or is this a human+agent collaboration layer? (Changes how prose-friendly the ledger needs to be.)
3. **Relationship to ASG:** is this meant to be a "bridge" that ASG-style benefits can be delivered to non-ASG codebases, or a genuinely independent direction?
4. **Freshness budget:** are we OK with a background indexer (eventually-consistent) or do we want every agent query to be strictly fresh?

## What this repo is not (yet)

No code. No architecture. This is a thinking doc meant to be torn apart. The next move is picking one of the layers above and sketching the concrete artifact — file format, query API, tooling surface — to see if it survives contact with a real example.
