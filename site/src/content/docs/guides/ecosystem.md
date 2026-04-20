---
title: "Ecosystem: ASG, CTXone, ASD"
description: How AgentStateDeveloper relates to AgentStateGraph (the substrate) and CTXone (project memory). Peer products on a shared backbone.
---

AgentStateDeveloper is one of three products from AgentStateLabs. All three
sit on the same substrate (AgentStateGraph) but serve distinct queries.
Understanding which product to reach for is a matter of *which question is
being asked*.

## The three products

| Product | Role | Answers | URL |
|---|---|---|---|
| **AgentStateGraph (ASG)** | Substrate | "store this content-addressed versioned state for me" | [agentstategraph.dev](https://agentstategraph.dev) |
| **CTXone** | Project / session memory | "why did we decide X on this project?" | [ctxone.dev](https://ctxone.dev) |
| **AgentStateDeveloper (ASD)** | Code-level context | "what effects does `payments.charge_card` have? what's its ledger say?" | [agentstatedeveloper.dev](https://agentstatedeveloper.dev) |

## AgentStateGraph: the substrate

ASG is a content-addressed, versioned, branchable state store. Merkle-DAG
underneath, with intent chains, authority + confidence metadata, and
branch/merge semantics. Both ASD and CTXone persist their data inside ASG
repositories.

ASD uses a local SQLite-backed ASG by default. The same binaries can point
at a Postgres-backed ASG for multi-tenant deployments with no schema change.

## CTXone: project memory

CTXone is code-agnostic. It answers questions at the scope of a project or
session: "why did we switch from Redis to Postgres last quarter?" "What were
the constraints that drove the auth design?" Its MCP tool surface — `remember`,
`recall`, `why_did_we` — operates on project-level memories rather than
symbols.

## ASD: code memory

ASD is code-specific. Its primitives — semantic index, ledger entries,
effect declarations, call graph, policy, audit — are all keyed to symbols in
a source tree. An ASD ledger entry is attached to `payments.charge_card`, not
to a project memory id.

## Why peers, not parent/child

The temptation is to frame ASD as "the coding extension of CTXone." It isn't.
The two have different durability, different identity models, and different
consumers:

- CTXone memories outlive any specific commit or file. They survive refactors
  because they were never tied to a line of code.
- ASD entries are anchored to symbols. They survive content edits through
  `symbol_id` rebinding, but they disappear if the symbol is deleted
  (surfaced as "orphaned" rather than silently dropped).

Treating them as peers keeps both models simple. Cross-citation is
straightforward: an ASD ledger entry can point at a CTXone memory via
`{ "type": "ctxone", "id": "mem_..." }` in its `evidence` array, and a
CTXone `why_did_we` response can point at an ASD ledger entry.

## When to reach for which

| Question | Product |
|---|---|
| "Why does this project authenticate against Auth0?" | CTXone |
| "What effects does `refund_payment` have?" | ASD |
| "Who approved the last tradeoff ledger entry on `charge_card`?" | ASD |
| "Show me every agent-authored decision in this session" | CTXone |
| "Which symbols call `open(...)` anywhere in the repo?" | ASD |
| "What did we decide about refactoring the billing module last sprint?" | CTXone |
| "What does the policy gate say about `asd.effect.declare.broadens`?" | ASD |

When a question is about the *code unit*, it's ASD. When it's about the
*project decision*, it's CTXone.

## Shared substrate, independent evolution

Because both products sit on ASG, their persistence, branching, and content
addressing are consistent. But the MCP tool surfaces, HTTP APIs, and CLI
binaries are independent. Adopt one without the other. A team using
CTXone today can add ASD later by pointing `asd` at the same ASG or a
separate one — ASD doesn't require CTXone to work, and CTXone doesn't
require ASD.
