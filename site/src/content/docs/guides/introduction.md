---
title: Introduction
description: What AgentStateDeveloper is, the problem it solves, and why it overlays git instead of replacing it.
---

AgentStateDeveloper (`asd`) is **the audit layer for agent-authored code**. It sits
beside your git repository and records the context that diffs can't carry:
decisions, assumptions, hazards, declared and observed side effects, the call
graph, policy gates, approvals, and an append-only event stream.

Everything it writes is keyed to symbols in your source tree — not line numbers —
so entries survive edits, renames, and refactors.

## The problem

Git was designed around text-delta review by humans minutes-to-hours per author
per day. Agents author hundreds of changes per session. Three things break:

- **Review bandwidth doesn't scale.** Humans can't keep up with agent throughput,
  so intent evaporates.
- **Diffs are the wrong unit.** A rename refactor shows up as 200 deletes plus
  200 adds; a behavior change can hide inside a one-character tweak.
- **Commit messages rot.** Prose next to code has no schema, no linkage to the
  symbol it describes, and no way to be superseded cleanly.

The consequence: agent-authored code lands without a machine-readable audit of
*why it got that shape*, *what effects it introduced*, or *who attested to it*.
That is the audit gap.

## What ASD adds

ASD overlays git with seven primitives, all keyed to the symbol (function,
method, class) they describe:

1. **Semantic index** — tree-sitter-parsed symbols with canonical, stable ids.
2. **Decision ledger** — structured entries (`decision`, `assumption`,
   `constraint`, `rationale`, `hazard`, `tradeoff`) that can be superseded,
   approved, rejected, or withdrawn.
3. **Effect declarations** — 17 standardized effect categories, declared per
   symbol, verified by static inference + runtime tracing.
4. **Call graph** — intra- and cross-module call edges with transitive effect
   propagation.
5. **Runtime tracer** — observed-syscall evidence that flips verification from
   `unverified` to `ok`/`mismatch`.
6. **Policy gate** — JSON rule file enforcing `allow` / `deny` / `require_approval`
   on every mutation, stamped into the ledger as `matched_policy`.
7. **Audit event stream** — one JSONL event per mutation across CLI, HTTP, and
   MCP, SIEM-ingest-ready.

See [Core Concepts](/guides/concepts) for the per-primitive detail.

## Overlay, not replacement

ASD does not want to replace git, your review tool, or your CI. The `.asd/v1/`
sidecar directory sits inside your repository. On a fresh clone, `asd init`
followed by `asd hydrate` restores the audit layer without any registry call.
Live state lives in a local ASG (AgentStateGraph) repository; derivable state
(the semantic index, transitive effect caches) is rebuilt on demand from source.

Reviewers still read PRs on GitHub. Humans still merge. What changes is that
when a reviewer asks "why did you delete this early-return?" the answer lives
in a superseded ledger entry attached to the symbol, not in a Slack thread
that has since rolled off the retention window.

## Who it is for

- **Solo developers** running agents at high velocity who want their own paper
  trail without waiting for the enterprise platform to ship.
- **Teams** adopting agent coding but finding their review workflow drowning —
  ASD gives reviewers a symbol-level view of what changed and why, plus a
  policy gate that stops risky writes at the source.
- **Security and compliance** who need an immutable, structured audit of every
  consequential action an agent took, without asking the agents' tool vendors
  to emit custom telemetry.

## What's included

A single Rust workspace ships:

- `asd` — CLI with `init`, `index`, `read`, `ledger`, `policy`, `verify-effects`,
  `trace`, `sync`, `hydrate`, `audit` subcommands.
- `asd-mcp` — stdio MCP server exposing 14 tools to coding agents.
- `asd-serve` — HTTP server (axum) hosting the JSON API and static Lens UI.
- Nine language adapters (tree-sitter based): Python, TypeScript, Rust, Go,
  Java, C#, Ruby, Kotlin, and Swift.
- A Python runtime tracer (`tools/asd_tracer.py`) that ingests observed
  syscalls back into the graph.
- A SvelteKit review UI (Lens) that reads the same HTTP API.
- File-backed policy gate with ratification workflow.
- JSONL audit stream.

BSL-1.1 licensed; converts to Apache-2.0 after four years.

## Next

Walk through the [Quick Start](/guides/quickstart) for a five-minute path from
clone to a working ledger entry on the included sample Python repository.
