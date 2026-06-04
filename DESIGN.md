# ASD — Design Sketch (non-policy pieces)

Sketch of the parts of AgentStateDeveloper (ASD) that we can commit to
independently of the policy crate. When the policy discussion lands, we
integrate against the hooks listed in [Deferred to policy](#deferred-to-policy).

## Scope of this doc

- ASG path convention ASD uses
- Symbol identity model (how ledger entries survive edits/renames)
- Decision ledger schema
- Effect declaration schema
- MCP tool surface (read + write)
- Freshness / lifecycle

Out of scope: per-language adapter internals, verifier implementations, UI.

## Relationship to CTXone and ASG

- **ASG** = substrate. ASD stores everything in ASG repos. No new storage.
- **CTXone** = project/session memory (code-agnostic). Peer, not parent.
- **ASD** = code-level context. New MCP tool family. Can cross-cite CTXone
  facts; CTXone's `why_did_we` can cite ASD ledger entries.

## OSS / Team / Enterprise

Everything in this doc is the **core** layer — works for a solo dev on a
laptop. Policy (who can write, what requires attestation, merge gates) is
an overlay that enterprises adopt without changing the schemas below.
The core is designed so policy attaches via hooks, not rewrites.

- **OSS** — `asd`, `asd-mcp`, `asd-serve` binaries. SQLite-backed, permissive
  policy (`NoPolicyMatch → Allow`), full ledger/effects/index, audit log tailing.
  Audit log verification and ratification (approve/reject/withdraw) return
  upgrade prompts at runtime.
- **Team** — `asd-pro`, `asd-pro-mcp`, `asd-pro-serve` binaries (commercial).
  Adds hash-chained JSONL audit log (`JsonlFileSink`), `audit verify`, and the
  ratification tier (approve/reject/withdraw via `agentstategraph-tasks`).
- **Enterprise** — Team + Postgres-backed ASG, multi-tenancy, registry for
  cross-machine authoring history, SIEM audit export, SSO/RBAC.

**Dual-binary model:** OSS and commercial features ship in separate binaries.
This prevents paid features from being extracted from the open binary at rest —
the OSS binary has no code paths for Team/Enterprise behavior; it only has
runtime stubs that surface upgrade messages.

## ASG path convention

One ASG repo per target codebase. Paths under that repo:

```
/asd/v1/
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
- `.asd/v1/effects/<qname>.json` — declared effects per symbol
- `.asd/v1/ledger/<qname>/<entry-id>.json` — non-superseded ledger entries
- `.asd/v1/rebinds/<timestamp>-<id>.json` — rename/move records that
  preserve canonical `symbol-id` across git's text-diff view
- `.asd/v1/meta/schema-version`

**In ASG (local, or pulled from registry):**
- Live authoring state (speculative branches, per-edit intent/confidence/authority)
- Traces (large, regenerable from test runs)
- Transitive-effect caches (derivable)
- Pre-summarization supersede chains

**Never persisted (always rebuilt):**
- Semantic index (symbols, call graph) — reparsed from source on every `reindex`
- Effect verification results — rerun on demand

### Reconstruction contract

A fresh clone running `asd init && asd reindex` with no registry access
rebuilds:

1. Reparse source → fresh semantic index + symbol fingerprints
2. Read `.asd/` → hydrate effect declarations + ledger entries
3. Replay `rebinds/` in commit order → preserve canonical `symbol-id` across renames
4. Rerun verifier → fresh effect verification status

Lost without a registry:
- Per-edit intent/confidence/authority inside a commit
- Speculative branches that didn't land
- Supersede chains prior to their summarization into ledger entries

With an opt-in ASG registry:
- Commits carry an `ASD-Commit: <asg-commit-id>` trailer
- `asd pull-meta` fetches the associated ASG commit(s) for full fidelity
- Full authoring history restored

### Merge semantics

`.asd/` is structured as one-file-per-entry deliberately: concurrent
agent work on different symbols produces zero conflicts, and supersede
never mutates existing files (only writes new ones). Ledger and effect
merges collapse to "union the files"; only same-symbol same-field
effect edits can conflict, and those are rare and resolvable.

### Rename handling

- **ASD-aware rename** (agent uses an ASD tool to rename): rebind record
  is written and committed, canonical id flows through git cleanly.
- **Out-of-band rename** (someone edits text directly): next `asd reindex`
  sees a new qname with no rebind record. Heuristic matcher (file
  identity + signature + content similarity) proposes a rebind and asks
  agent/human to confirm. Unconfirmed → new canonical id, old marked orphaned.

The honest trade: **structure survives git if ASD is in the loop on
structural edits.** Non-structural edits (body changes, docstring fixes)
always preserve canonical id via the fingerprint formula. Only
out-of-band renames/moves degrade — and they degrade gracefully (data
isn't lost, linkage is).

### Why this matters for positioning

This is the "overlay on git, not replacement" strategy made concrete.
Nothing ASD does requires the team to stop using git, GitHub, or their
existing review tooling. The `.asd/` directory becomes just another set
of tracked files; agents see ASG's full fidelity; humans reviewing on
GitHub get a semantic summary via commit trailers and the `.asd/` diff.

## Policy integration (via `agentstategraph-policy`)

The policy layer ships as the sibling crate `agentstategraph-policy`.
ASD is a **consumer** of that crate, not a definer. As of M18 this
integration is live: `PolicyStoreGate` (in `agentstatedeveloper-core::policy`)
wraps `PolicyStore`, imports JSON policy rules into an isolated in-memory
ASG repo at startup, and delegates all evaluation to the real policy engine.
`FilePolicyGate` is retained for unit tests and backward compatibility but
is no longer the production path.

### Call-site pattern

Before consequential writes, ASD tools call `policy_evaluate(situation,
proposed_action, agent_id)` and branch on the returned `Decision`:

- **Allow** → proceed; stamp `matched_policy: <path>@<version>` into the
  resulting ledger/effect record for audit
- **Deny** → return error with `reason`; record the denial in the ledger
  as a `hazard` so the attempt is visible
- **RequireApproval** → write a proposal, return "awaiting approval" to
  the agent; `approvers` drives who can ratify via `policy_ratify`
- **NoPolicyMatch** → fail-safe deny in production; solo-dev config can
  flip this to allow

### ASD action taxonomy

Formalized in `agentstatedeveloper-core::policy::actions`. Constants used by
every `PolicyGate::evaluate` call site:

- `asd.ledger.append.<kind>` — e.g., `asd.ledger.append.hazard` (via `ledger_append_action(kind)`)
- `asd.ledger.supersede`
- `asd.ledger.approve` / `asd.ledger.reject` / `asd.ledger.withdraw`
- `asd.effect.declare` / `asd.effect.declare.broadens` (when declared set widens)
- `asd.code.read` / `asd.code.commit`
- `asd.merge.branch_to_main`
- `asd.rename.symbol` / `asd.rename.file`

`Situation.qualifiers` is populated per call site so policy selectors can key
on `symbol_id`, `file`, `language`, `kind`, `qname`, and `entry_id`.

### Which ASD concerns map to which policy fields

| ASD concern | POLICY_V1 mechanism |
|---|---|
| Who can append a `hazard` ledger entry | `require_approval` on `asd.ledger.append.hazard`, `approvers: ["human"]` |
| Symbols that must have declared effects | `deny` on `asd.code.commit` with `situation_selector` matching path pattern and missing-declaration condition |
| Merge blocked on failing effect verification | `require_approval` on `asd.merge.branch_to_main` gated by `verify_effects` result in situation |
| Second-agent attestation | `require_approval` with `approvers: ["senior_agent", "human"]` + multi-agent ratification (POLICY_V1 §10.1) |
| Sensitive-symbol redaction | `deny` on `asd.code.read` for specific path prefixes, keyed to requester's agent_id |

### What is live as of M18

- `PolicyStoreGate` is the production path; `--policy` / `ASD_POLICY` loads
  it at startup via `Engine::load_policy_file`.
- Action taxonomy is formalized and wired at all write call sites.
- `Situation.qualifiers` carries `symbol_id`, `file`, `language`, `kind`,
  `qname`, and `entry_id` where available.
- `approve`, `reject`, and `withdraw` evaluate `LEDGER_APPROVE` /
  `LEDGER_REJECT` / `LEDGER_WITHDRAW` before touching the store.

### Enforcement honesty (per POLICY_V1 §11)

ASD enforcement is soft. A misbehaving agent can ignore a `Deny` — the
value is (a) machine-readable boundary, (b) audit trail, (c) deterrent.
Hard enforcement would require OS/FS/git-server-level controls outside
ASD's scope. The enterprise tier composes ASD's soft layer with infra-
level enforcement (CI gates, git hooks, OPA-backed merge bots).

### Open questions specific to ASD (not resolved by POLICY_V1)

1. **Action-taxonomy publication.** POLICY_V1 defers a standard taxonomy.
   Should ASD publish `asd.*` actions as a reference vocabulary for other
   code-facing consumers, or keep them private until a broader convention
   emerges?
2. **Per-file vs per-symbol policy scope.** POLICY_V1 selectors operate on
   `situation` strings. ASD symbols are hierarchical; do we build a
   selector helper for symbol-path matching or keep that as a consumer
   concern?
3. **Policy evaluation on cold clones.** A fresh clone without ASG access
   has no policies loaded. Do we fail-deny everything, or let core run
   unpoliced (solo-dev default) and surface that state in `asd health`?

## What to build first

Ordering per MVP discussion (C5: context silos, work validation, blast radius):

1. **Effect manifest + checker** — blast radius answer. Python + TypeScript
   adapters, static checker for obvious effects, runtime tracer for the rest.
2. **Decision ledger** — append-only, supersede, orphan surfacing. MCP tools
   `ledger_append`, `ledger_get`, `ledger_find`, `ledger_supersede`.
3. **Semantic index** — tree-sitter, qname resolution, call edges. Tools
   `code_query`, `code_read`, `callers_of`, `callees_of`.

Contracts and execution traces are phase 2.

## Build shape

### Tiers

See [OSS / Team / Enterprise](#oss--team--enterprise) above for the full
tier breakdown. The MCP tool surface is identical across tiers — policy
and the installed sink/ratify implementation determine per-call behavior,
not feature flags.

### License

BSL-1.1, matching CTXone. Licensor: AgentStateLabs, LLC. Change License:
Apache-2.0 after four years. Copy the CTXone LICENSE text verbatim,
swapping "CTXone" for "AgentStateDeveloper."

### Directory layout

```
AgentStateDeveloper/
├── crates/
│   ├── agentstatedeveloper-core/       # traits + ASG-backed default impls, PolicyGate
│   ├── agentstatedeveloper-python/     # Python language adapter
│   ├── agentstatedeveloper-typescript/ # TypeScript language adapter
│   ├── agentstatedeveloper-mcp/        # OSS MCP + HTTP server (asd-mcp, asd-serve)
│   ├── agentstatedeveloper-cli/        # OSS CLI (asd); also a library for asd-pro
│   ├── agentstatedeveloper-audit-pro/  # Enterprise: JsonlFileSink + verify_chain
│   ├── agentstatedeveloper-ratify/     # Team: RatifyOpsImpl (approve/reject/withdraw)
│   └── agentstatedeveloper-pro/        # Commercial binaries: asd-pro, asd-pro-mcp, asd-pro-serve
├── site/                               # agentstatedeveloper.dev (Astro)
├── examples/sample-py-repo/
└── Cargo.toml                          # workspace root
```

Follows the same pattern as `/Apps/stategraph/` and `/Apps/CTXone/`.

### Milestones (shipped through M17)

- **M1–M4** — core + Python adapter + CLI + MCP stub, Lens + HTTP server,
  real MCP server via `rmcp`, intra-module call graph + transitive effects.
- **M5** — cross-module edge resolution (Python imports).
- **M6–M9** — policy gate (FilePolicyGate), ledger supersede, ledger approve/
  reject/withdraw (Team tier stubs).
- **M10–M11** — sync/hydrate sidecar, TypeScript adapter.
- **M12** — audit-log event stream (CLI + MCP + HTTP).
- **M13** — marketing + docs site (agentstatedeveloper.dev, Astro).
- **M14** — audit tail parity across HTTP, MCP, CLI.
- **M15** — hash-chained audit log (blake3, prev_event_hash, tamper-evident).
- **M16** — Lens verify badge + live audit streaming + SPA routing fix.
- **M17** — OSS/commercial tier split: dual-binary model, `agentstatedeveloper-
  audit-pro` (Enterprise), `agentstatedeveloper-ratify` (Team),
  `agentstatedeveloper-pro` binaries; `RatifyOps` trait in core; upgrade
  prompts in OSS binaries; `/pricing` page.
- **M18 (in progress)** — `agentstategraph-policy` integration: `PolicyStoreGate`
  replaces `FilePolicyGate` as production path; action taxonomy formalized;
  `Situation` qualifiers enriched; policy evaluation wired into
  approve/reject/withdraw.

## Plan B — compact conclusion sidecar (in design)

Plan B redesigns the committed sidecar around *conclusions* — the expensive
LLM-formed facts that are hard to reproduce — and drops everything that is
derivable from source. Driven by the ExampleFlow field report showing the
`.asd/v1/` sidecar reaching 75 MB.

### Reframe from the t-001 audit

The "75 MB sidecar" problem is **not** a ledger problem. Ledger entries
already live in the ASG repo under `/asd/v1/ledger/{symbol_id}/{entry_id}`
and are small. The bloat lives in derived subtrees the indexer writes:

- `symbols/` — full Symbol JSON × N
- `effects/` — EffectDecl JSON × N
- `code/` — source snapshots
- `index/{by-qname,callers,callees,effects-rev}/` — derived call/effect graph

All four are regenerable from source via `asd index .`. Plan A t-003
already gitignored `.asd/v1/`. Plan B's job is to add a *committed* shape
for the part that is **not** regenerable (the ledger conclusions).

### Six conclusion classes vs current LedgerKind

| Class | Coverage today | Action |
|---|---|---|
| 1. Decisions | Decision, Rationale, Constraint, Assumption, Tradeoff, Invariant | reuse |
| 2. Classifications | Ownership, Concept (partial — no role/intent enum) | optional `role: Option<String>` on LedgerEntry |
| 3. Mappings (legacy → new coverage) | none | **new** `LedgerKind::Mapping` |
| 4. Hazards | Hazard, KnownBug | reuse |
| 5. Validation recipes | ValidationScenario, Proof, Evidence | optional `command: Option<String>` field |
| 6. Follow-ups | none | **new** `LedgerKind::FollowUp` |

Net schema delta: **2 new LedgerKind variants + 2 optional LedgerEntry
fields**. Nothing else changes in core's ledger model.

### Committed shape: `.asd/conclusions/*.jsonl`

One file per class, each line a compact JSON object:

```
.asd/
  conclusions/
    decisions.jsonl        # Decision | Rationale | Constraint | Assumption | Tradeoff | Invariant
    classifications.jsonl  # Ownership | Concept (+ role field)
    mappings.jsonl         # new Mapping kind
    hazards.jsonl          # Hazard | KnownBug
    recipes.jsonl          # ValidationScenario | Proof (with command field)
    followups.jsonl        # new FollowUp kind
  cache/                   # gitignored — index/, symbols/, effects/, code/ go here
```

Each JSONL line carries the minimal anchoring needed for human review and
round-trip:

```json
{"id":"led_…","symbol":"App.SongPlayers.tests","kind":"decision",
 "summary":"SongPlayers tests must stay out of default AudioEngine",
 "author":"ctx_user@…","created":"2026-05-19T16:54:00Z",
 "evidence":[{"kind":"ctxone","value":"sg_8ad836e175f3"}]}
```

Field order is fixed so re-export is byte-stable (t-004 acceptance).

### Round-trip

- `asd conclusions export` — reads ledger, writes the 6 JSONL files
- `asd conclusions import` — reads JSONL, upserts to ledger by entry_id
  (idempotent; supersedes carry through)
- `asd sidecar migrate` — drains existing `.asd/v1/ledger/` into JSONL,
  then prints `git rm -r --cached .asd/v1` for the user to drop the bloat

Hook flow after Plan B: `pre-commit` → `asd conclusions export` (kilobyte
diff); `post-merge`/`post-checkout` → `asd conclusions import` (replaces
the heavy `asd hydrate` flow).

### Acceptance

ExampleFlow sidecar drops from 75 MB → < 500 KB (t-008 probe).

## Plan C — semantic-layer moat (in design)

Plan C makes ASD remember the **expensive task-specific understanding** the
LLM forms — role tags, decisions-that-shape-ranking, change-intent recipes
— so a new session doesn't re-derive the same project mental model every
turn. Plan A built trust; Plan B built durable storage; Plan C builds the
defining feature.

### Audit takeaways (t-001 research)

The existing ranking pipeline lives in `search_fts.rs` and `candidates.rs`
and already does a lot: BM25 baseline, hybrid boost (path/name/phrase),
ledger count-boost (Ownership/Invariant/Hazard), tier penalty, feedback
verdicts (`Useful` ±1.5, `Noisy`/`WrongLayer` → `NEG_INFINITY`), and
file-scope feedback via glob. **What is missing:** Decision/Constraint
ledger entries are *flagged at index time but never consumed* — they're
passive notes, not active constraints. And the `role` field added on
`LedgerEntry` in Plan B t-002 is stored, exported, round-tripped, but
**never read for ranking**. Plan C closes both gaps.

### First-class role-tag vocabulary (t-002)

| Tag | Applies to (entry kinds) | Meaning |
|---|---|---|
| **fast-test** | ValidationScenario, Proof, test-tier symbols | Lightweight; safe in tight feedback loops |
| **diagnostic-test** | ValidationScenario, Proof | Debug/instrumentation; not part of main CI |
| **fixture-path** | test-tier symbols, Concept | Shared fixture; multiple tests depend on it |
| **stale-api** | Decision, Hazard | Deprecated interface; migration tracked |
| **package-boundary** | Ownership, Invariant | Cross-package facade; changes need coordination |
| **replacement-coverage** | Mapping | Legacy coverage handled by newer code |
| **performance-critical** | Invariant, Decision | Hot path; changes need perf measurement |
| **audit-pending** | Decision, Constraint, Assumption | Not yet reviewed; pending validation |

Implemented as a `RoleTag` enum in core with `as_str()` and `from_str()`
helpers. The `LedgerEntry.role` field stays `Option<String>` (so
unknown/free-form tags don't break old data), but the CLI / MCP / API
layers validate against the enum at write time and emit a warning for
unknown tags. Tagged tests in core lock the enum to the design.

### Decisions-as-constraints (t-003)

Recommendation from t-001: **ride on top of `apply_feedback_adjustments`
as synthetic verdicts** — do not add a new ranking stage. Constraint and
Decision ledger entries with a penalty role (e.g. `stale-api`,
`audit-pending`) get pre-processed at index time into a
`constraint_penalties: HashMap<sym_id, Vec<(role, optional_scope_glob)>>`
side-channel. During candidate scoring, the existing verdict loop applies
the same `NEG_INFINITY` suppression it uses for `WrongLayer` feedback.

Concrete shape:
- A `Constraint`/`Decision` entry whose `role = "stale-api"` makes its
  symbol behave like a `WrongLayer` verdict for queries that don't
  explicitly include `--include-stale` or scope-narrow to that file.
- An entry with `role = "package-boundary"` doesn't penalize directly,
  but adds a **boost** to other symbols in the same package — surfacing
  the inside-the-boundary alternatives first.
- Entry body MAY carry a `scope: ["path/glob"]` JSON field; if present
  the penalty/boost only applies to queries whose `paths_filter` is a
  subset of the scope. Otherwise it applies globally.

This keeps the machinery minimal: one new helper in core
(`build_constraint_penalties` from ledger walk), one new SQLite UNINDEXED
column (`constraint_roles` delimited string for fast hydration), and one
new branch in the existing per-symbol verdict loop.

### Change-intent recipes (t-004)

Recipes are structured outputs that replace raw symbol lists for known
task families. Shared `Recipe` schema in core:

```rust
struct Recipe {
    intent: String,                // "migrate-stale-tests"
    actions: Vec<RecipeAction>,    // ordered steps
}
struct RecipeAction {
    kind: ActionKind,              // Move | Delete | Run | Gate | KeepAsCovered
    file: String,
    reason: String,                // one-line "why this file"
    command: Option<String>,       // e.g. "swift test --filter X"
}
```

First recipe: `classify-test-migration-candidates`. Given a query, walks
test-tier symbols in scope, applies role-tag filters
(`fast-test`/`diagnostic-test`/`fixture-path`/`stale-api`), looks up
`Mapping` ledger entries for replacement-coverage links, and returns the
structured Recipe instead of a flat symbol list. CLI: `asd recipe
classify-test-migration`. MCP: `recipe_classify_test_migration`. The
shape sets the pattern for future recipes (Plan C+).

### Verdict feedback loop (t-005)

Today's `FeedbackVerdict` has 4 variants
(`Useful`/`Noisy`/`Missing`/`WrongLayer`). Plan C extends to a richer set
that drives both ranking AND classification accrual:

- `Useful` (existing) — boost stays
- `Noisy` (existing) — suppression stays
- `AlreadyCovered` (new) — implies a `Mapping` ledger entry from the
  noisy symbol to whatever covers it; surface as a follow-up prompt
- `DiagnosticOnly` (new) — implies a `Classification` entry with
  `role = "diagnostic-test"`; affects future queries via t-003
- `WrongLayer` (existing) — suppression stays, role tag stays free-form

Surface: `asd feedback verdict --query <q> --symbol <s> --verdict
already_covered --covered-by <other-sym>`. Each verdict can optionally
auto-write a ledger entry (Mapping or Classification), so the same
gesture that fixes today's query also accrues durable conclusions for
tomorrow.

### CTX task state → ASD ranking bias (t-006)

Read `CTX_ACTIVE_TASK` env var (or `.asd/cache/active-task.json` written
by an integrating hook). If present, parse a scope hint (file globs)
from the task description and apply a **soft +1.0 boost** to candidates
matching that scope — not a hard filter (the task's recorded scope may
be wrong; agent should still see alternatives). Side effect: every
ledger write during an active task auto-tags the entry with
`ctx:task:<id>` so future Plan C constraints can scope by task too.

### Initial-read `asd map` (t-007)

One-shot command: walk the indexed project, classify package boundaries
(directories with `__init__.py` / `Cargo.toml` / `package.json`), tag
test files with `fast-test` vs `diagnostic-test` heuristics, identify
entry-points-by-layer, and write the results as Classification and
Concept ledger entries. The output rides Plan B's conclusions export
into committed JSONL so the next clone gets the same project mental
model without re-running `asd map`.

## Plan G — agent thinking layer (in design)

Plan G captures the LLM's *thinking*, not just its conclusions. The
existing ledger covers what was decided (Decision/Constraint/Rationale)
and what's classified (Ownership/Concept/Mapping). It does not capture:

- **Hypotheses** — "I think X causes Y, confidence 0.6, evidence so far"
- **Mental models** — "the audio pipeline flows input → preprocess → mix"
  spanning multiple symbols
- **Failed attempts** — "I tried approach X; failed because Y; pivoting"
- **Open questions** — "what does magic constant 4096 actually mean?"

Without these, every new session re-derives the same understanding the
last session built. Plan G makes that thinking durable.

### Schema delta (t-002)

Four new `LedgerKind` variants:

| Variant | Notes |
|---|---|
| `Hypothesis` | uses existing `LedgerEntry.confidence: Option<f64>` (0.0–1.0). Plain `summary` is the claim; `body` MAY carry evidence anchors. |
| `MentalModel` | multi-symbol structural model. `body` JSON: `{"symbols": ["q1","q2"], "diagram": "optional ascii"}`. `summary` names the model. |
| `FailedAttempt` | negative evidence. `body` JSON: `{"tried": "X", "because": "Y"}`. Anchor on the symbol the attempt targeted. |
| `OpenQuestion` | known unknown blocking confident action. `summary` is the question; `body` MAY hold partial findings. |

New `ConclusionClass::Thinking` so the export pipeline buckets these
into `.asd/conclusions/thinking.jsonl` — visible to humans, round-trippable
via Plan B import.

Confidence threshold (`prior_thinking` surfacing): default 0.3 — anything
lower is too speculative to surface unprompted. Override via env or flag
(future).

### Write surface (t-003)

```sh
asd think speculate <qname> --conf 0.65 --summary "X causes Y because Z"
asd think model "audio-pipeline" --symbols pkg.a,pkg.b,pkg.c --summary "input → mix → out"
asd think failed <qname> --tried "rebind to symbol_id" --because "ledger had stale id"
asd think question <qname> --q "what does magic 4096 mean here?"
asd think list [--kind hypothesis] [--symbol q]
```

MCP equivalents: `think_speculate / think_model / think_failed /
think_question / think_list`.

Entry IDs are deterministic blake3 of `intent + content` so re-running
the initial-read bootstrap doesn't duplicate.

### Initial-read prompt (t-004)

Ships as `docs/initial-read-prompt.md`. Agent reads the prompt → runs
their own LLM evaluation against the indexed codebase → writes results
back via the commands above. ASD doesn't make LLM calls; it provides
the structure and the writeback surface.

Prompt sections:
1. Architecture — subsystems and how they connect
2. Hot spots — files likely touched often
3. Implicit constraints — invariants not written down
4. Open questions — magic numbers, unexplained patterns
5. Hypotheses with confidence levels
6. Failed-path warnings

Worked example: a real evaluation of `examples/sample-py-repo`.

### Bootstrap entry point (t-005)

`asd think bootstrap` prints the prompt path + a starter checklist of
commands the agent will run. `--check` scans existing thinking entries
and reports gaps (no hypotheses yet, no mental model named X).

### Auto-surface in prepare-change + context-for (t-006)

Both handlers emit a new `prior_thinking` JSON section:

```json
{
  "prior_thinking": {
    "hypotheses": [{"summary":"…","confidence":0.7,"qname":"…"}],
    "mental_models": [{"name":"…","symbols":[…]}],
    "open_questions": [{"q":"…","qname":"…"}],
    "failed_attempts": [{"tried":"…","because":"…","qname":"…"}]
  }
}
```

Hypotheses below `confidence < 0.3` excluded by default. `--no-thinking`
opts out for callers that don't want the noise.

### ctx:task auto-tagging (t-007)

Every `asd think *` write picks up `CTX_ACTIVE_TASK.task_id` (same
helper as Plan E t-011) and appends `ctx:task:<id>` + `source:asd-think`
tags. Lets future queries filter "show me everything I was thinking on
task T-027".

### Acceptance (t-008)

End-to-end fixture test: seed → `asd prepare-change` returns
`prior_thinking` with the seeded entries. Worked example in
`docs/initial-read-prompt.md` evaluates `examples/sample-py-repo`.
CHANGELOG entry documenting the commands + LedgerKind delta + new
response field.

### Implementation order

t-001 (this doc) → t-002 (schema delta) → t-004 (prompt template, can
run parallel) → t-003 (CLI/MCP surface) → t-006 (auto-surface) →
t-005 (bootstrap entry point) → t-007 (provenance tags) → t-008
(acceptance).

### Acceptance probes (t-008)

Add to `examples/exampleflow-probes.toml`:
- A recorded `role=stale-api` Constraint on a symbol demotes it on
  unrelated queries (decisions-as-constraints works).
- A `Noisy` verdict on `(query, symbol)` persists across runs.
- `recipe_classify_test_migration` returns role-tagged JSON, not a flat
  list.
- `asd map` writes ≥1 Classification entry per source-tier package.

Acceptance: ExampleFlow queries that previously surfaced
`AudioEngine` files for `SongPlayers` test work no longer do, because
the relevant Constraint actively demotes them.


---

## Plan H — Plan F revival (3 dormant tasks made actionable)

The three Plan F tasks that landed in "dormant — waiting on external
signal" status. Reframed here with concrete trigger conditions and
acceptance gates so they're picked up the moment the signal arrives
rather than sitting in DEFERRED limbo forever.

### t-001 — Index-time denorm of penalty tuple

**Status:** dormant. **Trigger:** measurable perf regression on
penalty application (>20ms per query on ExampleProj DB) OR ledger row
count > 50k.

Today `apply_constraint_penalties` does a per-candidate ledger walk.
Once denorm helps, materialize `(symbol_id, penalty_score, penalty_role)`
into `asd_symbols_meta` at index time, and have penalty application
read the table instead.

**Acceptance:** bench harness in `crates/agentstatedeveloper-cli/
benches/penalty.rs`; ExampleProj DB query latency drops by ≥30% on
queries that hit ≥100 candidates.

### t-003 — Crucible re-run validation

**Status:** dormant. **Trigger:** next Crucible-style real-work
session by Craig (use of `asd prepare-change` on Crucible repo with
seeded ledger).

Re-run the M20 Crucible field test against current 1.0.23 (was
0.9.19 then). Verify the 5 noted improvements still hold; capture
new gaps in `MEMORY/project_crucible_followup.md`.

**Acceptance:** memory file written; any new pain becomes Plan J
tasks (not new code in this task).

### t-004 — ExampleFlow sidecar-size validation

**Status:** dormant. **Trigger:** new ExampleFlow repo state
captured (post-Plan G writes for thinking entries).

The Plan B promise was 75 MB → 500 KB sidecar. Verify the promise
still holds with Plan G's 4 new LedgerKinds + Plan E's
`asd_symbols_meta`. Add a probe to `tools/sidecar-size-check.sh`.

**Acceptance:** `du -sb .asd/conclusions/` on ExampleFlow < 1 MB.
Probe wired into CI as a soft gate (warns on regression, doesn't
fail).

### t-005 — Full prepare_change orchestration extract

**Status:** dormant. **Trigger:** the next feature that requires
parallel edits to both CLI `commands/prepare_change.rs` and MCP
`mcp_server.rs::prepare_change` handler.

Today both call sites duplicate the orchestration (file scoring,
recipe assembly, brief gating). Extract into `core::prepare_change::
orchestrate(engine, params) -> Value`. Both surfaces become thin
adapters.

**Acceptance:** CLI + MCP handlers each shrink by ≥70%; identical
JSON output verified by golden test against a fixed seed.

---

## Plan I — DEFERRED.md backlog cleanup (exhaustive)

Every entry in DEFERRED.md gets a task. Some are real shippable work;
some are doc refreshes; some are explicit "still deferred, keep
deferred" decisions documented for the next maintainer. The intent
isn't to ship all of these — it's to convert the backlog from
"things I'd want to remember" into "things with a known disposition."

### t-001 — Refresh DEFERRED.md against current reality

Audit every entry; mark resolved items, update stale claims (the
languages line says "Python only" when 10 adapters now ship; the
`.asd/` sidecar entry says "never implemented" but Plan A shipped
it). Reset "Last synced" stamp.

### Tracer

- **t-002** — Document async/await coverage limits in tracer + a
  `--no-trace-async` flag that errors loudly instead of silently
  missing.
- **t-003** — Multi-thread tracer: install `sys.settrace` on every
  thread spawned during a `--with-threads` run. Acceptance: tracer
  catches effects from a `Thread.start()` body.
- **t-004** — Subprocess child instrumentation: emit a `proc.spawn`
  enrichment record carrying the child PID + cmdline so external
  audit can correlate. Deferred until needed; document as such.

### Static effect inference (Python)

- **t-005** — Strip comments + string literals before substring
  match. Acceptance: a function with `# os.open` in a comment no
  longer infers `fs.read`.
- **t-006** — Proper SQL classifier for `.execute()` — use
  `sqlparse` (already-pulled dep) or a tiny CTE-aware tokenizer.

### Call graph (Python)

- **t-007** — Resolve `from foo import *` via FTS lookup of foo's
  exports.
- **t-008** — Relative imports (`from . import x`, `from ..pkg`).
- **t-009** — Function-body imports.
- **t-010** — Module-scope call sites (top-level work).
- **t-011** — Multi-segment module imports (`import foo.bar.baz`).
- **t-012** — Document dynamic-dispatch as permanently out-of-scope;
  add a callsite note when the indexer sees `getattr(...)` patterns
  to warn the agent.

### Policy

- **t-013** — Build `agentstategraph-policy` properly (replace
  interim `FilePolicyGate`).
- **t-014** — Selector DSL: paths, timestamps, qualifiers. Migrate
  existing rules.
- **t-015** — Hot-reload via inotify/kqueue.
- **t-016** — Policy coverage over `asd trace` ingest.
- **t-017** — Policy coverage over `asd index` writes.
- **t-018** — Policy coverage over merge surface (when merge ships).
- **t-019** — Policy coverage over rename.

### Ratification (M9 gaps)

- **t-020** — `asd ledger reject <entry>` action.
- **t-021** — `asd ledger revoke <entry>` for approved entries.
- **t-022** — Approval rationale: `--message` on approve + an
  `approval-note:<text>` first-class field.
- **t-023** — Cryptographic signing of approvals (ed25519). Ship
  behind `--require-signed` flag.
- **t-024** — `asd ledger supersede <old> <new>` surface across
  CLI / MCP / HTTP (schema already supports it).

### HTTP / MCP

- **t-025** — Reverse `symbol_id → qname` index (drop O(N) scan).
- **t-026** — `ledger_find` pagination + composite index by
  `(symbol_id, kind, created_at)`.
- **t-027** — API-key auth on HTTP + MCP. Enforce by default; the
  `--insecure-localhost` flag opts out.
- **t-028** — `health.symbol_count` reports total artifact count
  (symbols + ledger entries + effects), not just indexed-qnames.

### Lens UI

- **t-029** — Reject + withdraw-approval buttons (pairs with t-020 /
  t-021).
- **t-030** — Cross-module graph visualization (render edges as a
  graph, not a flat list).
- **t-031** — Effect-distribution overview route (top-N by blast
  radius).
- **t-032** — `effect_declare` UI (so humans can edit effects
  without raw MCP calls).
- **t-033** — Policy authoring UI (POLICY_V1 proposal/ratify UX).
- **t-034** — "Who approved what, when" timeline view.

### Languages

- **t-035** — Refresh DEFERRED's languages line (it's wrong — 10
  adapters ship). Rolled into t-001.

### Enterprise scaffolding

- **t-036** — Registry server (cross-machine authoring-history pull).
- **t-037** — SIEM/Splunk/Datadog audit export connectors.
- **t-038** — Enterprise SSO/RBAC on symbols / ledger / policies.
- **t-039** — Admin UI for multi-tenant scoping.
- **t-040** — Postgres multi-tenant exercised end-to-end.

### Miscellaneous

- **t-041** — `asd index` summary reports dropped call edges
  (unresolved callees count + sample).
- **t-042** — Trace entries carry per-call duration + call-depth.
- **t-043** — Disk schema migration tool (`/asd/v1/` → `/asd/v2/`).
- **t-044** — Audit log rotation + retention policy.
- **t-045** — Real-time audit streaming (replace `since:<event_id>`
  polling).
- **t-046** — Lens verify-badge UI (backend works, not surfaced).

### OSS / commercial

- **t-047** — License-key / billing enforcement on `asd-pro` (M17
  t-013). Deferred until paying customers exist; track here so it
  doesn't fall through.

### Working-style (meta)

- **t-048** — Sandbox allowlist policy for sub-agents: relax
  `cargo` / `npm` permission so agents can self-verify; document
  what's safe to allow.

### Implementation order

t-001 (refresh DEFERRED) first — gives accurate context for the rest.
Then ratification cluster (t-020 → t-024) since it's the closest to
shippable. Then Python call-graph cluster (t-007 → t-012). Everything
else is opportunistic.

---

## Plan J — Field-eval wishlist consolidation (M20–M27)

Every M20–M25 field-evaluation memory flagged real pain that never
became a plan. Pull each one into an actionable task here. Many will
slot into existing surfaces (search, prepare-change, feedback);
some need new code.

### From M20

- **t-001** — **RESOLVED 1.0.51** (commit `866b840`, shipped by a
  parallel agent). Both CLI and MCP `prepare_change` now walk
  direct callers of each candidate, collect their Invariant ledger
  entries, dedupe via `seen_inv`, and surface them tagged
  `from_caller: true`. Integration test
  `invariants_from_callers.rs::invariant_on_direct_caller_surfaces_with_from_caller_tag`
  seeds an indirect topology (caller's qname tokens are
  intentionally disjoint from the query and the candidate so the
  caller can't sneak in via FTS or ledger-anchor — only the
  Plan J propagation path can surface its invariant).
- **t-002** — Test-gap detection: when `prepare_change` finds an
  impl with no test in `affected_tests`, surface a "missing test"
  warning in `safe_change_recipe.manually_validate`.

### From M21

- **t-003** — `ExampleFlowViewModel "other"` mis-bucketing fix in
  `file_role` classifier. Add a `viewmodel/` path pattern.
- **t-004** — Broad-search miss diagnosis: when `search` returns
  <3 hits, run a fallback that drops `intent_focus` and
  re-scores; surface "broadened search because…" in the response.
- **t-005** — Symbol-count mismatch between `asd status` (canonical)
  and `asd health` (artifact count). Reconcile.

### From M22

- **t-006** — View file discovery — already partially landed in M28
  (+2.0 boost for view/viewmodel on view queries). Field-test
  whether the boost is enough or needs to be promoted into
  `file_role = "view"` as its own bucket.
- **t-007** — Precise test suggestions: when `proposed_test_path`
  fires, also emit a stub of the recommended `def test_X()` body
  shape (per language adapter).
- **t-008** — Live hydrate regression test: `asd hydrate --verify`
  on a real-world sidecar in CI to catch round-trip drift.

### From M23

- **t-009** — qname collision fix: when two symbols share a qname
  across language adapters (e.g. `pkg.Model` in both Python and
  Swift), prefer the one matching the query's language hint.

### From M24

- **t-010** — Ledger-anchor regression test on `find_candidates`
  (the M24 work added anchoring; lock the behavior in CI).

### From M25 (most urgent — Craig's real-work notes)

- **t-011** — Scoping / exclusion polish: per-query negative globs
  (`--exclude 'tests/**'`), language exclusions
  (`--exclude-lang swift`), named exclude sets in `.asd/scopes.toml`.
- **t-012** — **RESOLVED 1.0.46**. The `match_reasons` array (Plan
  A / M26 era) already provides per-hit "why this result" signals
  on every search and investigate response (CLI + MCP):
  `name:foo` / `file:foo` / `sig:foo` / `doc:foo` /
  `invariant-attached:N` / `ledger:N hazard[s]` / `ownership:abc` /
  `recent-edit`. The M25 wishlist used different vocabulary
  (`+intent_focus`, `-wrong_layer_penalty`) but the substance is
  the same: a per-hit array of ranking-relevant signals. Locked
  with 8 unit tests on `explain_match` covering name/file/doc match
  precedence, recent-edit, invariant count, hazard count + plural,
  ownership-overlap, and empty-input cases. If future field-eval
  shows the descriptive vocabulary needs arithmetic deltas
  (e.g. `+1.5 ownership_boost`), that's a separate task — the
  current signals are descriptively complete.
- **t-013** — More test-scenario coverage: extend
  `validation_scenarios` to surface on `impact` (not just
  `context_for`).
- **t-014** — False-positive feedback handling: `asd feedback mark
  --verdict false-positive` already exists but doesn't decay over
  time. Add a `--ttl` so old verdicts auto-expire after N days.

### From M26 (uncertainty model rollout)

- **t-015** — **RESOLVED 1.0.68** (kernel + field-validated
  semantics + cohort-split via t-019 precision probes).

  Shipped surface:
  - `core::calibration::compute_calibration(observations)
    → CalibrationReport { buckets, total, overall_pass_rate }`,
    pure (no I/O, no clock). Bucket grouping via BTreeMap for
    stable sorted output. Advisory strings fire only when
    sample ≥ 5 AND observed rate diverges from expected by >15pp.
  - `ProbeResult.calibration_signal: Option<String>` populated
    at probe-execution time from `uncertainty.level` regardless
    of pass/fail (1.0.65 fix — original 1.0.59 wiring only
    harvested from debug_payload, which is None on the pass
    path, so all-passing runs produced empty buckets).
  - Wired into `asd probe run --json` as a top-level
    `calibration` block.

  Bucket-semantics table (corrected in 1.0.68 after the
  inverted-axis bug surfaced):
    Uncertainty axis (low=high-confidence, critical=low-confidence):
      low 95% | medium 70% | high 45% | critical 20%
    Quality axis (core=good result, noisy=bad):
      core/strong/relevant 90% | partial/peripheral 65% |
      weak/noisy 25%

  **Four-round arc** (the most expensive lesson of the session):

    1.0.59  Synthetic only — 10 unit tests pass against the
            (wrong) bucket-semantics assumption. Kernel ships.
    1.0.65  First ExampleProj run produces empty calibration block.
            Root cause: debug_payload is Some(_) only on probe
            FAILURE; the calibration harvester read from there,
            so an all-passing run produced no observations.
            Fix: capture uncertainty.level into
            ProbeResult.calibration_signal at execution time,
            independent of pass/fail.
    1.0.66  Second run produces a confidently-wrong single-cause
            "threshold too strict" advisory. Softened the wording
            to enumerate three competing causes (too-strict
            threshold, too-lenient probes, label-semantics
            mismatch) and recommend tightening probes before
            retuning thresholds.
    1.0.67  Added t-019 precision-mode probes (`qname_rank_eq
            exact=1` alongside lenient `qname_rank_lte max=5`).
            Probes shipped — same advisory still fires, but now
            the cohort split (lenient + precision both pass)
            rules out the lenient-probe explanation.
    1.0.68  Traced the remaining mystery into
            core::candidates::compute_uncertainty and found the
            actual threshold ladder: `low` = LOW UNCERTAINTY =
            high confidence, `critical` = highest uncertainty.
            Inverted from the calibration table's assumption.
            Fix: split the table into two explicit axes
            (uncertainty + quality) with opposite directions.
            New regression `exampleproj_field_signal_now_well_
            calibrated` locks the corrected semantics.

  **Postmortem — guideline pinned for future predictors:**
  Never write a calibration table for a label scheme without
  first staring at the predictor's actual threshold ladder. The
  synthetic unit tests will pass against the wrong table because
  they encode the same wrong assumption — only real-world
  distributions tied to the real predictor can expose the
  inversion. Codified in project CLAUDE.md.

### From M27 (feedback loop rollout)

- **t-016** — **RESOLVED 1.0.70**. Pure decay helpers in
  `core::feedback::{decay_factor, decay_for_entry}` with
  `DEFAULT_FEEDBACK_HALF_LIFE_DAYS = 90.0` (one quarter — survives
  near-full weight then meaningfully fades). Wired into both
  `apply_feedback_adjustments` (per-symbol path) and
  `apply_file_scope_feedback` (file-glob path) by multiplying the
  `+1.5` Useful boost by `decay_for_entry(created_at, now,
  half_life)`. Suppression verdicts (Noisy / WrongLayer /
  AlreadyCovered / DiagnosticOnly) deliberately do NOT decay —
  they're explicit "this is wrong" signals; agents can use Plan J
  t-014's `--ttl-days` for soft expiry on negatives. Tuple shape
  for `flat_verdicts` / `flat_file_scope_verdicts` widened to
  4 elements (added `created_at: DateTime<Utc>`); all 6 call sites
  across CLI + MCP updated. Coverage: 9 pure decay unit tests in
  `feedback.rs::plan_j_t016_decay_tests` (zero age, half-life,
  multiples, clock-skew defense, sub-day resolution, disabled-by-
  zero-half-life) + 2 integration tests in
  `tests/feedback_decay_integration.rs` (fresh vs 9-month-old
  end-to-end, negative-verdict-does-not-decay regression). 367 →
  377 lib tests passing.

### Discovered during t-004 implementation (2026-06-03)

- **t-017** — `--paths` (and probably `--scope`, `--exclude-path`,
  `--exclude-set`, `--exclude-lang`) are no-ops for **results** in
  `asd search`'s FTS hot path. They populate `FtsFilters` and gate
  the `scope_narrowed` advisory flag, but the FTS SQL only filters
  by `kind` + `language`. The `apply_paths_filter` logic in
  `core::candidates` runs from `find_candidates` (used by
  `prepare_change`, `investigate`, etc.) but not from
  `commands/search.rs`. Result: an agent typing
  `asd search "auth" --paths "src/api/**"` sees ALL `auth` matches
  across the repo, with only the `scope_narrowed: true` flag as a
  hint that the filter was registered. Fix: thread
  `apply_paths_filter` + `apply_exclude_paths_filter` +
  `apply_exclude_languages_filter` + `apply_exclude_terms_filter`
  through the post-FTS rerank stage in search.rs. Should also be
  covered by an integration test that asserts the filtered set is
  a proper subset of the unfiltered set. Once fixed, t-004's
  broadener gains a much bigger payoff window (the most common
  user-invoked narrowing actually works).

- **t-018** — Verify shell-command examples in user-facing output.
  Triggered by two consecutive field-test catches on ExampleProj in
  the same session: `asd think bootstrap` told users to run
  `asd reindex` (no such command — was `asd index` until 1.0.62
  added the alias), and earlier `commands/think.rs:283` used a
  CWD-relative path to docs/initial-read-prompt.md that failed
  from any non-AgentStateDeveloper checkout (fixed in 1.0.61 via
  `include_str!`). Pattern: command examples baked into help
  text, JSON output, bootstrap checklists, error advisories,
  README, DESIGN.md, and `asd --help long_about` can silently
  drift from reality. Add a `tests/help_examples_resolve.rs`
  integration test that:
  1. Runs `asd think bootstrap`, `asd --help`, `asd init`,
     `asd onboard`, and any other surfaces that print shell
     commands as guidance.
  2. Extracts anything matching `` `asd <subcommand>...` `` from
     stdout.
  3. For each, runs `asd <subcommand> --help` and asserts exit
     code 0. (Full execution would have side effects; --help is
     a cheap, side-effect-free sanity check that the subcommand
     and its top-level flags resolve.)
  4. Fails the build with a list of broken references when any
     example doesn't resolve.
  Bonus: extend to scan DESIGN.md and CHANGELOG.md backtick-quoted
  command examples too — those drift even faster than runtime
  output. The doc/code drift detection is the larger value;
  catching it at CI time means agents stop hitting "tip: some
  similar subcommands exist" errors that erode trust in the
  documentation.

- **t-019** — Precision-mode probe assertions to disambiguate
  calibration signal. Triggered by 1.0.65 ExampleProj field run:
  `asd probe run --json | jq .calibration` showed 7 of 9
  uncertainty-bearing probes in the `low` bucket with 100% pass
  rate (75pp over the bucket's expected midpoint). The advisory
  fires, but the cause is ambiguous — `low` could mean (a) the
  threshold is too strict, (b) the probes are too lenient
  (`qname_rank_lte max_rank=5` passes when the right symbol is
  anywhere in the top 5, not just at rank 1), or (c) the bucket
  label semantics describe within-result uncertainty rather than
  an expected failure rate, in which case 100% pass is consistent
  with `low` and the threshold is correct. Without precision
  probes, we can't distinguish. Add:
  1. New assertion kind `qname_rank_eq { fragment, exact_rank }`
     — probe fails unless the matching symbol is at the EXACT
     specified rank (typically 1).
  2. Update `asd probe bootstrap` to generate one precision
     probe per ranking probe (matching `qname_rank_eq` with
     `exact_rank: 1` for the top symbol).
  3. Tag the new probes `precision` so they can be filtered with
     `asd probe run --tag precision`.
  4. Once the same `low` cohort shows MIXED pass rates (lenient
     probes pass + precision probes fail), the original
     "threshold too strict" advisory becomes actionable for
     retuning `compute_uncertainty`.
  Until then, the kernel correctly reports "we don't know which
  cause" — the multi-line advisory text reworded in 1.0.66
  enumerates all three explicitly.

### Implementation order

M25 cluster first (t-011 → t-014) — they're current pain, not
historical. Then test-related items (t-002 / t-007 / t-013). Then
the rest opportunistically.


---

## Plan K — sidecar canonicalization

**The principle (single rule that decides everything):**

> The sidecar carries **judgment** — anything an agent or human had
> to decide, classify, hypothesize, approve, or otherwise commit
> mental effort to. Anything mechanically derivable from source
> stays in the regenerable SQLite cache and is `.gitignore`d.

**The onboarding story this enables (north star):**

A new developer clones the repo. Their agent reads `.asd/conclusions/
*.jsonl` directly as context — the prior team's judgment, mental
models, decisions, hazards, hypotheses. The agent now has the
expensive-to-rederive knowledge. They run `asd onboard` (or `asd
init && asd index . && asd hydrate`), which rebuilds the mechanical
layer from source. They're caught up — no apprenticeship, no
re-derivation, no `.md` plan rot. The agent comes online inheriting
the prior agent's understanding.

For this story to work the sidecar must:
1. Be readable as plain JSONL by any agent without ASD installed
   (no opaque blob, no symbol_id-only references the reader can't
   resolve).
2. Stay compact enough that reading it doesn't blow the agent's
   context window (per-shard target: ≤ 200 KB on ExampleFlow-scale
   projects).
3. Survive concurrent edits without spurious conflicts (judgment
   conflicts are meaningful and worth surfacing; ordering noise
   isn't).

### Task table

| # | Task | Why | Acceptance |
|---|---|---|---|
| t-001 | Sort-on-write inside each `.jsonl` shard | Highest-leverage conflict reduction. Two devs writing to the same class no longer produce a textual conflict per entry; git's line merger handles independent insertions. | `asd sync` produces byte-identical output regardless of write order. Test: insert entries in two orders, hash the file, assert equal. |
| t-002 | Effect sync filter — **RESOLVED 1.0.37 (non-issue under current architecture)**. Audit: the committed sidecar (`.asd/conclusions/*.jsonl`) doesn't include effects at all — `ConclusionClass::all()` has 7 buckets (decisions, classifications, mappings, hazards, recipes, followups, thinking) and zero of them route to `EffectDecl`. Statically-inferred effects regenerate via `asd index` and live in SQLite (`.asd-state.db`, gitignored) plus the vestigial `.asd/v1/effects/` cache (also gitignored, per t-009). No commit-path noise to filter. Future consideration: if effects ever ship in the committed sidecar (e.g., to carry runtime-tracer-verified ones across clones), then this filter becomes real and the EffectDecl.verification.by = StaticChecker tag is what to discriminate on. Not in current scope. | Verified: `ConclusionClass::all()` enumeration excludes effects; `gather_buckets` walks ledger entries only. |
| t-003 | Confidence-floor filter in sync (Plan G thinking) | Low-confidence speculation is closer to scratch than to durable judgment. Keep it locally for `asd think list`; don't ship the noise. | `asd think speculate --conf 0.1 …` writes to ledger; subsequent `asd sync` does NOT include that entry in `.asd/conclusions/thinking.jsonl`. |
| t-004 | Self-describing entries — **RESOLVED 1.0.38 (already done)**. Audit: `ExportRecord` already carries `id`, `kind`, `qname`, `file`, `summary`, `body` (where present), `role`, `command`, `tags`, `evidence`, `supersedes`, `author`, `created_at` all inline. No opaque `symbol_id` reference in the export shape. The L meta-lesson applied: the original Plan K t-001 draft inherited DEFERRED.md's wishlist wording without checking — this was already done back in Plan B t-004 when ExportRecord was designed. New regression test `exported_entries_are_self_describing` locks in the property: parses a serialized line, asserts id/kind/qname/summary are present and non-empty, asserts no `symbol_id` field leak. | Test confirms qname grep-by works; structural fields present and non-empty; no symbol_id leak. |
| t-005 | `asd onboard` one-shot for new clones | Today's boot order is `init → index → hydrate`. A new dev shouldn't have to know that. One command, right order, idempotent. | `asd onboard` on a fresh clone: installs hooks, indexes the project, hydrates committed sidecar into SQLite. Re-running is a no-op. CHANGELOG entry documents the onboarding story. |
| t-006 | `asd think bootstrap --existing` mode | When sidecar already has thinking entries (new dev joining a project that's already been mapped), bootstrap should *summarize what's there* instead of pushing the agent through the initial-read prompt again. | Detects ≥1 MentalModel or ≥3 Hypotheses in the ledger → prints a "Inherited thinking from prior session(s)" summary block before the checklist. With `--check`, distinguishes "you" gaps from "team" gaps. |
| t-007 | Optional per-package sharding under `.asd/config.toml` | One-shard-per-class is fine for ExampleFlow; monorepos with two teams editing the same class will see false conflicts. Opt-in finer granularity. | `.asd/config.toml` key `sidecar.shard_by = "package"` produces `.asd/conclusions/decisions/<package-key>.jsonl`. Default unchanged. `asd hydrate` reads either layout transparently. |
| t-008 | `asd sync --check-budget` + CI gate | "Compact" becomes enforced, not aspirational. Pairs with Plan H t-004 (ExampleFlow size validation). | Threshold configurable in `.asd/config.toml` (default 1 MB total, 200 KB per shard). Exits non-zero when exceeded. CI surfaces as a soft gate (warns, doesn't fail) with `--soft`. |
| t-009 | Audit `.asd/v1/` legacy + clarify storage layout — **DONE 1.0.35** (scope adjusted from "purge" to "audit + document"). Audit found: (a) `/asd/v1/` is BOTH the SQLite tree prefix (alive, `ASD_PATH_PREFIX`) AND a vestigial on-disk directory; (b) `.asd/v1/` on-disk is gitignored, `sync_to_dir`/`hydrate_from_dir` still write/read it for local debug; (c) README incorrectly told users to `git add .asd/v1/`. Fixed: rewrote README "Git-native sidecar" section, added DESIGN.md "Storage layout" canonical reference, updated DEFERRED.md. Did NOT delete `sync_to_dir`/`hydrate_from_dir` — they're still used by `asd sync`/`asd hydrate` for local-debug workflows and removal would break those without user benefit. | README has no stale `.asd/v1/` instructions; DESIGN.md has a single canonical storage-layout table; the SQLite-prefix-vs-on-disk distinction is explicit in docs. |
| t-010 | Document the principle in `DESIGN.md` + emit lint warning on violations | The principle is only useful if new code respects it. A future contributor adding a new LedgerKind or artifact needs a single rule to test against. | New `DESIGN.md` section "Sidecar inclusion rule" with the boundary table from this plan. `asd repair` learns to detect & warn on regenerable artifacts that leaked into `.asd/conclusions/`. |

### Implementation order

t-009 first (audit reality before changing it). Then t-001 (sort) and
t-002 (effect filter) — biggest immediate wins, no schema impact.
Then t-004 (self-describing entries — needed for the onboarding
story to work). Then t-005 + t-006 (onboarding surface). Then t-003,
t-007, t-008. t-010 lands last as the canonical documentation of
what shipped.

### Acceptance — the onboarding scenario as the integration test

End-to-end test in `crates/agentstatedeveloper-cli/tests/
plan_k_onboard.rs`:

1. Set up a fixture repo with committed `.asd/conclusions/*.jsonl`
   containing 1 MentalModel, 3 Hypotheses (conf ≥ 0.3), 2 Decisions,
   1 KnownBug.
2. Simulate a "fresh clone" (wipe `.asd/cache/` and SQLite).
3. Run `asd onboard`.
4. Assert: `asd think bootstrap --existing` reports the inherited
   thinking. `asd context-for <qname>` returns the seeded
   `prior_thinking`. `asd impact <qname>` includes the seeded
   `KnownBug` in hazards.
5. The agent reading the raw sidecar (no ASD process) can answer
   "what does this project's prior team think the architecture is?"
   from `.asd/conclusions/thinking.jsonl` alone.

When all five hold, the onboarding north star is real.


---

## Plan L — execution slice from Plan I

Plan I enumerated 48 deferred items. Most are intentionally cut from
near-term work (enterprise scaffolding without customers, policy
work gated on POLICY_V1, Lens redesign, perf-at-scale we don't
have). This plan is the 10-task subset that's **pressing, bounded,
and self-contained** today.

Plan I stays as the canonical backlog (so nothing's lost). Plan L
is what we actually burn through.

### Task table

| # | Plan I | Title | Theme | Effort |
|---|---|---|---|---|
| t-001 | I/t-001 | Refresh DEFERRED.md against reality | Doc accuracy | XS |
| t-002 | I/t-005 | Strip comments + string literals before static effect inference | Python accuracy | S |
| t-003 | I/t-008 | Resolve relative imports (`from . import x`, `from ..pkg`) — **already done in Plan D t-004**; closed with double-dot end-to-end regression test | Python accuracy | XS |
| t-004 | I/t-009 | Resolve function-body / conditional imports | Python accuracy | M |
| t-005 | I/t-012 | Document dynamic-dispatch as out-of-scope + `getattr` callsite warning | Python accuracy | S |
| t-006 | I/t-041 | `asd index` summary reports dropped (unresolved) call edges | Diagnostic | S |
| t-007 | I/t-020 | `asd ledger reject <entry>` action — **already done**; closed retroactively (already shipped across core/CLI/MCP with ratify integration tests) | Ratification | XS |
| t-008 | I/t-022 | Approval rationale: `--message` on approve — **already done** end-to-end (CLI + MCP wired; ratify appends "Approver note" to body; test `approve_with_message_appends_to_body`) | Ratification | XS |
| t-009 | I/t-024 | `asd ledger supersede <old> <new>` surface across CLI / MCP / HTTP — **already done**; closed retroactively (already shipped, HTTP routes via `asd-pro-serve` reusing `build_router`) | Ratification | XS |
| t-010 | I/t-028 | `health.symbol_count` reports total artifact count | Diagnostic | XS |

### Wave ordering

**Wave 1 — Baseline (1 task)**
- t-001: Refresh DEFERRED.md. Lands first so subsequent waves have
  accurate context. Resolved items get archived; stale claims (the
  "Python only" languages line; the ".asd/ sidecar never
  implemented" line) get corrected; new "Last synced" stamp.

**Wave 2 — Python accuracy cluster (5 tasks)**
- t-002 → t-006. All sit in `agentstatedeveloper-python` and
  `agentstatedeveloper-core/effects`. Shared test fixture work pays
  off across all five.
- Order within wave: t-005 (cheap doc + warning) → t-006
  (diagnostic surface) → t-002 (effect false-positive fix) → t-003
  (relative imports) → t-004 (function-body imports). Easier to
  harder.

**Wave 3 — Ratification surface completion (3 tasks)**
- t-007 → t-009. All sit in `core::ledger` + matching CLI / MCP /
  HTTP surfaces. Coherent: today you can append + approve; after
  this wave you can also reject, annotate, and supersede.
- Order: t-008 (`--message` is the foundation — supersede and
  reject can both use the same rationale field) → t-007 (reject) →
  t-009 (supersede surface).

**Wave 4 — Diagnostic accuracy (1 task)**
- t-010: standalone cleanup. Land last; or land opportunistically
  alongside any wave touching `core::health`.

### Acceptance per task

| # | Acceptance |
|---|---|
| t-001 | DEFERRED.md "Last synced" updated; every entry has correct disposition (resolved / still-deferred / superseded). No factually wrong claims remain. |
| t-002 | Python fixture: function body `# os.open(...)` in a comment no longer infers `fs.read`. Existing inference tests still pass. |
| t-003 | Python fixture: `from . import sibling` + `from ..pkg import x` produce call edges. Both single-dot and double-dot covered. |
| t-004 | Python fixture: a function with a body-local `import requests` produces a `net.out` effect on that function. |
| t-005 | DESIGN.md gains a "Python adapter — known limits" section listing dynamic-dispatch patterns. `asd index` emits one `dynamic-dispatch-warning` line per `getattr(<obj>, …)` pattern at module load. |
| t-006 | `asd index .` final summary line includes `dropped_call_edges: <N>` and `sample_unresolved: [...]` (top 3). Same field surfaces in MCP `health` response. |
| t-007 | `asd ledger reject <entry-id> --reason "..."` rejects an awaiting entry. Status flips to `Rejected`; tag `rejected-by:<author>` + `reject-reason:<text>` appended. MCP `ledger_reject` mirrors. |
| t-008 | `asd ledger approve <entry-id> --message "looks correct"` writes `approval_note` field on the entry. CLI `asd ledger get` displays it. Both CLI and MCP surfaces accept the flag. |
| t-009 | `asd ledger supersede <old-id> <new-id>` writes `supersedes: [old-id]` on the new entry + marks old as `Superseded`. Available in CLI, MCP, HTTP. Schema already exists; this is surface only. |
| t-010 | `asd health` JSON `symbol_count` returns `{symbols: N, ledger_entries: M, effects: K}` instead of the bare integer. Backward compat: bare `symbol_count` field keeps working, new fields added. |

### Implementation order across waves

Wave 1 → Wave 2 → Wave 3 → Wave 4. Each task within a wave gets its
own commit + version bump (1.0.25 → 1.0.34 if we ship all ten).


---

## Python adapter — known limits

Things the Python adapter intentionally does not resolve in the call
graph. Listed here so agents/humans treat missing edges as known
rather than as bugs. The `asd index` summary surfaces a one-line
warning per detected dynamic-dispatch site (Plan L t-005).

### Dynamic dispatch (out-of-scope by design)

- **`getattr(obj, name)(args)`** — runtime attribute lookup feeding
  a call. The callee name isn't known until execution. Surfaced by
  the indexer when detected.
- **`getattr(obj, name)` without trailing `(...)`** — a read, not a
  dispatch. Resolved by the property/attribute pass; not flagged.
- **`__getattr__` / `__getattribute__` method definitions** — the
  class promises to resolve unknown attributes at runtime. Any call
  to an attribute not statically defined on the class may dispatch
  through this hook. Surfaced by the indexer.
- **Callback-by-argument** — `def run(callback): callback()`. The
  callee is whatever's passed in at the call site. Not flagged
  individually (would noise up the warning stream); covered by the
  "anything that takes a callable parameter" disclaimer.
- **Computed method dispatch via dictionaries** — `handlers[kind]()`.
  Same shape as callback-by-argument.
- **Metaclasses / dynamic class generation** — `type(name, bases,
  body)` and friends. Out-of-scope; modules built this way won't
  appear in the index at all.

### Star imports (out-of-scope until needed)

- **`from foo import *`** — would require fetching `foo`'s exports
  via FTS and binding each one. Skipped today (Plan I t-007 holds
  the option to do this if it becomes a pain point).

### Static effect inference (caveats)

- **F-string interpolation contents** are masked along with the
  literal (Plan L t-002). An effect inside `f"{requests.get(url)}"`
  won't be inferred. Conservative trade — false-negatives over
  false-positives.
- **SQL classification on `.execute(...)`** uses a prefix match.
  CTEs (`WITH …`) classify as `IoDbRead` (correct for SELECT-with-
  CTE, wrong for INSERT-with-CTE). Plan I t-006 has the upgrade.


---

## Storage layout — what lives where (1.0.35)

ASD uses one in-SQLite namespace and two on-disk locations. They serve
different purposes; the table below is the canonical reference. (Plan
K t-009 audit clarification — earlier docs conflated the SQLite path
prefix `/asd/v1/` with the on-disk directory `.asd/v1/`, which is a
different thing.)

| Where | What | Tracked? | Purpose | Notes |
|-------|------|----------|---------|-------|
| **SQLite tree `/asd/v1/...`** (in `.asd-state.db`) | Index, call graph, ledger, effects, traces, FTS5, search docs, symbol meta | n/a (local DB, gitignored) | Authoritative runtime state | `ASD_PATH_PREFIX = "/asd/v1"`. The `v1` here is the SQLite tree namespace, NOT an on-disk version. |
| **`.asd/conclusions/*.jsonl`** | Compact subset by ConclusionClass (decisions, classifications, mappings, hazards, recipes, followups, thinking) | **Yes** (committed) | What a fresh clone inherits as judgment | Plan B compact format. Round-tripped via `asd conclusions export/import`. Pre-commit hook writes this; post-merge/post-checkout hooks import it. |
| **`.asd/v1/`** (on-disk directory) | Verbose mirror written by `sync_to_dir` | No (gitignored since Plan B) | Vestigial local-debug artifact | Still written by `asd sync`, still readable by `asd hydrate`. Not on the commit path. Kept because the codepath is harmless and some local workflows still use it. Can be removed entirely if it ever becomes confusing — Plan K t-009 chose to keep it. |
| **`.asd/cache/`** | Misc derived state (e.g., `active-task.json`) | No (gitignored) | Per-session ephemeral | Don't commit. |
| **`.asd/scratch/`** | Working notes scoped to a symbol, with promote-to-ledger path | No (gitignored) | Per-developer scratch | Plan A scratch shipped local-only on purpose. |

**Rule of thumb when adding a new artifact:**
- Is it derivable from source by `asd index`? → SQLite tree, gitignored
- Is it the kind of judgment an agent / human had to commit mental effort to? → `.asd/conclusions/`
- Is it per-session / per-developer? → `.asd/cache/` or `.asd/scratch/`

When in doubt, default to **regenerable + gitignored**. The principle is:
sidecar = judgment; everything mechanical is regenerable; conflicts
in the committed sidecar are meaningful (someone made a different
judgment) and worth surfacing.


---

## Sidecar inclusion rule (Plan K t-010)

A single rule for contributors adding new LedgerKinds, artifacts, or
output paths. Test every new piece of data against this:

> The committed sidecar (`.asd/conclusions/*.jsonl`) carries
> **judgment** — anything an agent or human had to decide,
> classify, hypothesize, approve, or otherwise commit mental effort
> to. Anything mechanically derivable from source stays in the
> regenerable SQLite cache and is gitignored.

### How to decide

| If your new artifact is… | …then it goes in | …because |
|---|---|---|
| A LedgerKind whose `conclusion_class()` routes to one of the 7 ConclusionClass buckets | `.asd/conclusions/<stem>.jsonl` (committed) | It's judgment. New dev should inherit it. |
| A statically-derivable index, FTS table, or denorm cache | SQLite tree under `/asd/v1/...` (gitignored via `.asd-state.db`) | `asd index` regenerates it from source. |
| A per-session ephemeral (active task, hot cache) | `.asd/cache/` (gitignored) | Doesn't survive a fresh clone. |
| A personal/working note that may promote-to-ledger later | `.asd/scratch/` (gitignored) | Local-only by design. |
| A runtime trace (expensive to regenerate but reproducible by re-running) | SQLite, NOT committed sidecar | Re-runnable. |

### How the rule is enforced

- **At sync time**: `asd conclusions export` only walks ledger
  entries, never indexes or FTS. The export schema (`ExportRecord`)
  has no field for derived artifacts.
- **Plan K t-002 (effects)**: even though `EffectDecl` has a
  schema-level way to ship verified effects, the current
  ConclusionClass enum deliberately doesn't bucket them — keeping
  the committed sidecar judgment-only.
- **Plan K t-003 (confidence floor)**: Hypothesis entries below
  `DEFAULT_CONFIDENCE_FLOOR` are filtered at sync time. A weak
  speculation isn't durable enough to count as team judgment.
- **Plan K t-010 (this section)**: `asd repair` walks
  `.asd/conclusions/` and warns on anything that doesn't match the
  expected `<known-class-stem>.jsonl` or `<known-class-stem>/*.jsonl`
  layout. Catches accidental leakage of regenerable artifacts.

### What `asd repair` detects (sidecar lint)

| issue kind | trigger |
|---|---|
| `sidecar_unknown_file` | top-level file isn't `<known-class-stem>.jsonl` (e.g. `effects.jsonl` snuck in) |
| `sidecar_unknown_dir` | subdirectory name isn't a known class stem (per-package layout only recognizes `<stem>/`) |
| `sidecar_wrong_extension` | a file isn't `.jsonl` (e.g. someone committed `notes.md` to `.asd/conclusions/`) |

None of these are auto-fixed — they need human review (the file
might be intentional, or might be a new judgment class waiting on a
schema extension). Surfaced as `Warn`, not `Error`, so they don't
block `asd repair --fix` from running the auto-fixable ASG-side
corrections.

### Adding a new judgment class (workflow)

If you're shipping a new kind of judgment that should travel with
commits:

1. Add the variant to `LedgerKind` in `core::schema`.
2. Route it via `conclusion_class()` to an existing or new
   `ConclusionClass` variant. New class → also add to
   `ConclusionClass::all()` and `filename_stem()`.
3. The export/import pipeline picks it up automatically — no
   conclusions_export changes needed.
4. The `asd repair` lint picks it up automatically — your new
   class's `filename_stem()` joins the known-stems set.
5. Document it in CHANGELOG and (if user-visible) README.

If you're shipping a new derived artifact, it goes in SQLite or
`.asd/cache/`, never `.asd/conclusions/`. The lint will warn you if
you forget.

