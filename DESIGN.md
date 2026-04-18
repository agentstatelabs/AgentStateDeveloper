# ASC — Design Sketch (non-policy pieces)

Sketch of the parts of AgentStateCode (ASC) that we can commit to
independently of the policy crate. When the policy discussion lands, we
integrate against the hooks listed in [Deferred to policy](#deferred-to-policy).

## Scope of this doc

- ASG path convention ASC uses
- Symbol identity model (how ledger entries survive edits/renames)
- Decision ledger schema
- Effect declaration schema
- MCP tool surface (read + write)
- Freshness / lifecycle

Out of scope: per-language adapter internals, verifier implementations, UI.

## Relationship to CTXone and ASG

- **ASG** = substrate. ASC stores everything in ASG repos. No new storage.
- **CTXone** = project/session memory (code-agnostic). Peer, not parent.
- **ASC** = code-level context. New MCP tool family. Can cross-cite CTXone
  facts; CTXone's `why_did_we` can cite ASC ledger entries.

## Solo vs. enterprise

Everything in this doc is the **core** layer — works for a solo dev on a
laptop. Policy (who can write, what requires attestation, merge gates) is
an overlay that enterprises adopt without changing the schemas below.
The core is designed so policy attaches via hooks, not rewrites.

## ASG path convention

One ASG repo per target codebase. Paths under that repo:

```
/asc/v1/
  code/<lang>/<canonical-path>/<symbol-fp>        # symbol node (current content)
  index/
    by-qname/<qualified-name>                      # qname  → symbol-id
    callers/<symbol-id>                            # inbound edges
    callees/<symbol-id>                            # outbound edges
    effects-rev/<effect>/<symbol-id>               # reverse index for "what writes to fs?"
  ledger/<symbol-id>/<entry-id>                    # decision ledger entries
  effects/<symbol-id>                              # declared + verified effects
  traces/<symbol-id>/<trace-id>                    # execution evidence
  meta/
    files/<canonical-path>                         # last-parsed hash, mtime, parser version
    schema-version                                 # for upgrades
```

- `v1` prefix lets us evolve the schema without corrupting old repos.
- `canonical-path` = repo-relative, forward-slash, no symlinks.
- `symbol-fp` = fingerprint of current symbol content (hash). Changes on every edit.
- `symbol-id` = **canonical, stable** identity (see next section).

ASG's content-addressing handles dedup for repeated content automatically.

## Symbol identity

Ledger/effects/traces must survive code edits and file moves. Two-level identity:

- **`symbol-id`** — canonical, stable across content changes. Computed from
  `(qualified-name, kind, initial-declaration-fingerprint)` and registered
  once per symbol. Renames update `qualified-name` but keep the same id
  via a rebind step on the next index pass.
- **`symbol-fp`** — hash of current symbol content. Changes every edit. Used
  as a version tag on the `code/` node.

Ledger, effects, and traces key on `symbol-id`. Blame on content keys on
`symbol-fp`. ASG's branching handles speculative renames naturally: the
rebind happens on the branch, and merges resolve or surface conflict.

When a symbol is deleted, its ledger/effects/traces become **orphaned**,
not deleted. Orphaned entries surface to review (`ledger_find kind=orphaned`)
so context isn't silently lost.

## Decision ledger

Append-only. Edits are **supersedes**, not mutations. This is the rot-proof
mechanism — entries are timestamped and linked, new decisions replace old
without erasing the trail.

### Schema

```json
{
  "entry_id": "asg-assigned",
  "symbol_id": "stable id of the symbol this attaches to",
  "span": {
    "file": "canonical-path",
    "start": [line, col],
    "end": [line, col],
    "captured_at_fp": "symbol-fp when written"
  },
  "kind": "decision | assumption | constraint | rationale | hazard | tradeoff",
  "summary": "one-line, <120 chars",
  "body": "optional, markdown, <2KB",
  "evidence": [
    { "type": "trace",  "id": "trace-id" },
    { "type": "ledger", "id": "entry-id" },
    { "type": "test",   "qname": "module::test_name" },
    { "type": "ctxone", "id": "memory-id" },
    { "type": "external", "url": "https://..." }
  ],
  "author": { "kind": "agent | human", "id": "agent-run-id or user-id" },
  "confidence": 0.0,
  "supersedes": ["entry-id", "..."],
  "created_at": "2026-04-17T00:00:00Z",
  "tags": ["perf", "security", ...]
}
```

### Entry kinds

- **`decision`** — "we chose X over Y." Load-bearing choice.
- **`assumption`** — "this relies on <external fact>." Surfaces if the fact changes.
- **`constraint`** — "must preserve property P." Reader must not violate.
- **`rationale`** — "why the code is shaped this way." Explanation, not a rule.
- **`hazard`** — "this is dangerous because..." Warning to future editors.
- **`tradeoff`** — "chose A; cost is B." Explicit acknowledgment of downsides.

Kinds matter because policy (when added) can key rules on them ("every
security-tagged hazard requires attestation before merge").

### Supersede, not edit

When a decision changes, you write a new entry with the old entry's id in
`supersedes`. The old entry stays visible in history. `ledger_get` returns
only non-superseded entries by default; `--include-superseded` shows the
chain. This is how we kill plan rot without losing context.

### Orphaning rules

- Symbol deleted → entries marked orphaned, not deleted.
- Symbol renamed → entries follow via `symbol-id` rebind, no action needed.
- Symbol signature change incompatible with entry constraint → entry
  surfaces to review (does the constraint still apply?).

## Effect declarations

Per-symbol declaration of what the symbol does externally. Declared effects
are advisory until verified.

### Effect vocabulary (v1)

Small, standardized, extensible:

- `io.fs.read` / `io.fs.write`
- `io.net.in` / `io.net.out`
- `io.db.read` / `io.db.write`
- `state.global.read` / `state.global.write`
- `state.process` (process-local mutation of shared refs)
- `env.read`
- `time.read` / `time.sleep`
- `random`
- `proc.spawn`
- `throw` (or language equivalent)
- `log`
- `pure` — explicit "does none of the above"

Each effect may carry qualifiers (paths, hosts, tables) for blast-radius
reasoning.

### Schema

```json
{
  "symbol_id": "...",
  "declared": [
    { "effect": "io.fs.write", "qualifiers": { "paths": ["logs/**"] }, "note": "structured log output" },
    { "effect": "io.net.out",  "qualifiers": { "hosts": ["api.example.com"] } }
  ],
  "transitive": [
    { "effect": "io.net.out", "via": ["symbol-id-of-callee"], "qualifiers": {...} }
  ],
  "verification": {
    "by": "static-checker | runtime-tracer | test-observed",
    "at": "2026-04-17T00:00:00Z",
    "status": "ok | mismatch | unverified",
    "mismatches": [
      { "kind": "undeclared", "effect": "io.fs.write", "detected_in": "..." }
    ]
  },
  "confidence": 0.0
}
```

### Verification sources

All three are evidence, with differing confidence:

1. **Static checker** — language-specific, parses call graph + known-effect
   stdlib annotations. Strongest when it succeeds, narrow in reach.
2. **Runtime tracer** — wraps syscalls/stdlib during test runs, records
   observed effects per symbol. Broad reach but depends on test coverage.
3. **Test-observed** — test suite asserts "this symbol should have these
   effects and no others." Authored assertion, strongest semantic check.

`verification.status = mismatch` is the signal that unlocks the audit-layer
use case: "this function does more than it declared."

## MCP tool surface

~12 tools. Lean enough that an agent can learn the whole surface in one prompt.

### Read

| Tool | Purpose |
|---|---|
| `code_query` | find symbols by name/kind/file/tag/effect |
| `code_read` | fetch symbol source with declared effects + top ledger entries inline (primary "read a function" path) |
| `callers_of` | inbound call edges for a symbol |
| `callees_of` | outbound call edges |
| `effects_of` | declared + transitive effects, with verification status |
| `ledger_get` | entries for a symbol (non-superseded by default) |
| `ledger_find` | search ledger by kind, tag, author, date, free-text |
| `traces_of` | execution evidence for a symbol |

### Write

| Tool | Purpose |
|---|---|
| `ledger_append` | new entry (symbol_id, kind, summary required) |
| `ledger_supersede` | write new entry that supersedes one or more existing |
| `effect_declare` | set/replace declared effects for a symbol; triggers re-verify |

### Admin

| Tool | Purpose |
|---|---|
| `verify_effects` | run checker against declared effects; returns mismatches |
| `reindex` | force re-parse of file or symbol (normally automatic) |
| `health` | indexer status, last-sync per file, orphaned-entry count |

`code_read` is the highest-leverage tool: it returns source + inline
effects + most recent ledger entries in one call. That's the replacement
for "agent reads the file and guesses."

## Freshness / lifecycle

- **Index:** regenerated on file-change (fs watcher in server; single-shot
  `reindex` for CLI). Symbol node content is hashed; unchanged symbols
  keep their id and ledger links untouched.
- **Effects:** re-verified on symbol content change. Results written back
  with new `verification.at`. Mismatches raised as diagnostics.
- **Traces:** appended on test run. Not auto-pruned; agent or admin can
  prune old traces.
- **Ledger:** never auto-mutated. Only agent/human writes. Orphaned entries
  surface but aren't deleted.

Freshness contract: `code_read` returns current content with a warning
flag if the indexer is behind, so agents know when results may be stale.

## Git roundtrip & reconstruction

ASG is the system of record for live state, but the code lives in git and
gets cloned/reviewed/CI'd on machines that may not have ASG access. The
design has to survive a fresh `git clone` on a cold machine without
losing the load-bearing context.

### Three-tier storage split

**In git (travels with the code):**
- Source code
- `.asc/v1/effects/<qname>.json` — declared effects per symbol
- `.asc/v1/ledger/<qname>/<entry-id>.json` — non-superseded ledger entries
- `.asc/v1/rebinds/<timestamp>-<id>.json` — rename/move records that
  preserve canonical `symbol-id` across git's text-diff view
- `.asc/v1/meta/schema-version`

**In ASG (local, or pulled from registry):**
- Live authoring state (speculative branches, per-edit intent/confidence/authority)
- Traces (large, regenerable from test runs)
- Transitive-effect caches (derivable)
- Pre-summarization supersede chains

**Never persisted (always rebuilt):**
- Semantic index (symbols, call graph) — reparsed from source on every `reindex`
- Effect verification results — rerun on demand

### Reconstruction contract

A fresh clone running `asc init && asc reindex` with no registry access
rebuilds:

1. Reparse source → fresh semantic index + symbol fingerprints
2. Read `.asc/` → hydrate effect declarations + ledger entries
3. Replay `rebinds/` in commit order → preserve canonical `symbol-id` across renames
4. Rerun verifier → fresh effect verification status

Lost without a registry:
- Per-edit intent/confidence/authority inside a commit
- Speculative branches that didn't land
- Supersede chains prior to their summarization into ledger entries

With an opt-in ASG registry:
- Commits carry an `ASC-Commit: <asg-commit-id>` trailer
- `asc pull-meta` fetches the associated ASG commit(s) for full fidelity
- Full authoring history restored

### Merge semantics

`.asc/` is structured as one-file-per-entry deliberately: concurrent
agent work on different symbols produces zero conflicts, and supersede
never mutates existing files (only writes new ones). Ledger and effect
merges collapse to "union the files"; only same-symbol same-field
effect edits can conflict, and those are rare and resolvable.

### Rename handling

- **ASC-aware rename** (agent uses an ASC tool to rename): rebind record
  is written and committed, canonical id flows through git cleanly.
- **Out-of-band rename** (someone edits text directly): next `asc reindex`
  sees a new qname with no rebind record. Heuristic matcher (file
  identity + signature + content similarity) proposes a rebind and asks
  agent/human to confirm. Unconfirmed → new canonical id, old marked orphaned.

The honest trade: **structure survives git if ASC is in the loop on
structural edits.** Non-structural edits (body changes, docstring fixes)
always preserve canonical id via the fingerprint formula. Only
out-of-band renames/moves degrade — and they degrade gracefully (data
isn't lost, linkage is).

### Why this matters for positioning

This is the "overlay on git, not replacement" strategy made concrete.
Nothing ASC does requires the team to stop using git, GitHub, or their
existing review tooling. The `.asc/` directory becomes just another set
of tracked files; agents see ASG's full fidelity; humans reviewing on
GitHub get a semantic summary via commit trailers and the `.asc/` diff.

## Deferred to policy

These hooks exist in the schema but enforce nothing in core:

- **Author gate:** who may append a given ledger `kind`. Core accepts any
  author; policy can restrict (e.g., `hazard` requires human author).
- **Required declarations:** which symbols must have effect declarations.
  Core makes declarations optional; policy can require them for symbols
  matching a pattern (e.g., anything in `payments/`).
- **Merge gate:** blocks merge when ledger/effect state fails policy
  rules. Core doesn't block anything; policy wraps `verify_effects` and
  returns pass/fail.
- **Attestation:** a second author must sign specific entries. Core has
  the `author` field; policy adds `attestations: [...]` enforcement.
- **Redaction / access control:** sensitive symbols visible only to
  authorized agents. Core returns everything; policy filters.

Policy hooks ride on top of the core schemas — no schema changes required
when policy lands, only enforcement code.

## Open questions (resolve with policy doc)

1. Does policy live in ASG itself or as a wrapper MCP server? (Affects
   whether `ledger_append` calls route through policy before hitting ASG.)
2. Is attestation a first-class entry kind or a metadata field on existing
   entries?
3. Are merge gates enforced in the ASG commit path or at a higher-level
   review step? (Branching model suggests the former.)
4. Does the policy crate model *code* policy specifically, or is it
   generic ASG policy that we map onto ASC's schema?

## What to build first

Ordering per MVP discussion (C5: context silos, work validation, blast radius):

1. **Effect manifest + checker** — blast radius answer. Python + TypeScript
   adapters, static checker for obvious effects, runtime tracer for the rest.
2. **Decision ledger** — append-only, supersede, orphan surfacing. MCP tools
   `ledger_append`, `ledger_get`, `ledger_find`, `ledger_supersede`.
3. **Semantic index** — tree-sitter, qname resolution, call edges. Tools
   `code_query`, `code_read`, `callers_of`, `callees_of`.

Contracts and execution traces are phase 2.
