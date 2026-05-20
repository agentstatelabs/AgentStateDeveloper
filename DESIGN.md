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

