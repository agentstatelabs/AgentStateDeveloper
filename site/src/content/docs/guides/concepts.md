---
title: Core Concepts
description: The seven primitives ASD composes — semantic index, decision ledger, effect declarations, call graph, runtime tracer, policy gate, ratification workflow, and audit event stream.
---

ASD is made of seven primitives. Each has a well-defined ASG path, a CLI /
MCP / HTTP surface, and an audit event shape. Understanding them in isolation
is enough to understand everything else.

## 1. Semantic index

**What.** Symbol-level model of your codebase — every function, method, class
parsed by a language adapter (tree-sitter based; nine languages supported).
Each symbol gets a `symbol_id` (canonical, stable across content edits) and a
`symbol_fp` (content fingerprint, changes on every edit).

**Why.** Ledger entries, effects, traces, and call edges all hang off
`symbol_id` rather than line numbers, so they survive refactors without
re-linking.

**Where.**
- `/asd/v1/index/by-qname/<qname>` — fast qname lookup
- `/asd/v1/code/<language>/<canonical-path>/<symbol-fp>` — symbol node
- `/asd/v1/index/callees/<symbol_id>` and `/asd/v1/index/callers/<symbol_id>`

**Example.**

```bash
asd index .
```

```json
{ "files": 4, "symbols": 13, "effects": 13, "edges": 5,
  "intra_module_edges": 3, "cross_module_edges": 2, "transitive_updates": 3 }
```

## 2. Decision ledger

**What.** Append-only log of structured notes keyed to a symbol. Six kinds:
`decision`, `assumption`, `constraint`, `rationale`, `hazard`, `tradeoff`.
Edits are not mutations — they're **supersedes**. The original stays in the
history; the default list view filters it out.

**Why.** Replaces commit-message prose with a schema-ful, searchable,
reviewer-auditable record. Supersede kills plan rot without losing context.

**Where.** `/asd/v1/ledger/<symbol_id>/<entry_id>`

**Example.**

```bash
asd ledger append payments.charge_card \
  --kind decision \
  --summary "reject amounts over 10000 at the boundary, not inside the DB driver"
```

```json
{ "entry_id": "led_abc...", "matched_policy": null, "status": "allowed" }
```

See the [Policy guide](/guides/policy) for how entries are gated and the
[Audit guide](/guides/audit) for the events each mutation emits.

## 3. Effect declarations

**What.** Per-symbol record of what external effects a symbol has. Seventeen
standardized categories:

| Category | Meaning |
|---|---|
| `io.fs.read` / `io.fs.write` | filesystem read / write |
| `io.net.in` / `io.net.out`   | inbound / outbound network |
| `io.db.read` / `io.db.write` | database read / write |
| `state.global.read` / `state.global.write` | module-global mutation |
| `state.process`              | process-local mutation of shared refs |
| `env.read`                   | reads an environment variable |
| `time.read` / `time.sleep`   | wallclock read / blocking sleep |
| `random`                     | cryptographic or pseudo-random source |
| `proc.spawn`                 | subprocess / `os.exec*` |
| `throw`                      | language-level raise / throw |
| `log`                        | structured logging |
| `pure`                       | explicit "none of the above" |

Each `Effect` carries optional `qualifiers` (paths, hosts, tables, vars) and a
`note` field with the matching source line. Effects are **declared**,
**transitive** (propagated from callees), and **verified** — the
`verification` block reports `unverified`, `ok`, or `mismatch`.

**Why.** Answers "what's the blast radius of this function?" before you run it.
Transitive propagation means asking a high-level driver what it touches
produces a list of every side-effect category reached via any call edge.

**Where.** `/asd/v1/effects/<symbol_id>`

**Example.**

```bash
asd read payments.charge_card
```

```json
{
  "effects": {
    "declared": [
      { "effect": "log",         "note": "log.info(...)" },
      { "effect": "io.db.write", "note": "db.execute(\"INSERT...\")" },
      { "effect": "throw",       "note": "raise ValueError(...)" }
    ],
    "transitive": [],
    "verification": { "by": "static-checker", "status": "unverified" }
  }
}
```

## 4. Call graph (intra + cross-module)

**What.** For every symbol, the set of symbols it calls (`callees`) and the
set that call it (`callers`). Each language adapter resolves call sites using
language-appropriate heuristics — import aliases, qualified names, method
receivers, and intra-class dispatch.

**Why.** Drives transitive effect propagation and lets reviewers walk a
proposed change outward — who calls this? what does it call?

**Where.**
- `/asd/v1/index/callees/<symbol_id>` — outbound edges
- `/asd/v1/index/callers/<symbol_id>` — inbound edges

**Example.**

```bash
asd read driver.main
```

```json
{
  "effects": {
    "declared": [],
    "transitive": [
      { "effect": "io.db.write", "via": ["sym_payments_charge_card"] },
      { "effect": "log",         "via": ["sym_payments_charge_card"] },
      { "effect": "throw",       "via": ["sym_payments_charge_card"] }
    ]
  }
}
```

## 5. Runtime tracer

**What.** A `sys.settrace`-based Python tracer (`tools/asd_tracer.py`). It
monkey-patches a pragmatic slice of stdlib entry points (open/print/logging/
time/urllib/os.environ/subprocess/random) and records per-`qname` the effects
it observed. Invocation via `asd trace -- <cmd...>`.

**Why.** Flips each touched symbol's `verification.by` to `runtime-tracer` and
surfaces `mismatch` diagnostics when the declared effects and the observed
effects disagree — the audit signal that unlocks "this function does more
than it said it did."

**Where.** Raw traces at `/asd/v1/traces/<symbol_id>/<trace_id>`; verification
results roll into `/asd/v1/effects/<symbol_id>`.

**Example.**

```bash
asd trace -- python _driver.py
```

```json
{
  "exit_code": 0,
  "traced_qnames": 4,
  "updates": [
    { "qname": "payments.charge_card", "status": "ok", "mismatches": [] },
    { "qname": "payments.get_balance", "status": "mismatch",
      "mismatches": [{ "kind": "undeclared", "effect": "io.net.out", ... }] }
  ]
}
```

Honest limits: single-process, single-thread. Subprocess children and worker
threads aren't instrumented; only the parent's `Popen` is recorded as
`proc.spawn`. See [the Python guide](/guides/python) for full scope.

## 6. Policy gate

**What.** JSON rule file loaded once at engine open. Each rule has a `path`,
`version`, `match_action`, optional `agent_id` pin, and one of three terminal
shapes: `Allow`, `Deny`, `RequireApproval` (with an `approvers` list). First
match wins. Actions use a dotted namespace: `asd.ledger.append.hazard`,
`asd.effect.declare.broadens`, etc.

**Why.** Consequential writes stamp `matched_policy: <path>@<version>` onto
the resulting ledger / effect record. That gives you two things: a machine-
readable attestation trail, and a knob — change the rules in the file, the
behavior changes on next process restart. No code change.

**Where.** `<policy-file>.json` (off the graph, pointed to by `--policy` or
`ASD_POLICY`). Evaluation is synchronous; the result is surfaced in the
ledger entry's `matched_policy` field and in the audit event.

**Example.** A rule from `examples/policies.json`:

```json
{
  "path": "/policies/code/hazard-requires-human",
  "version": 1,
  "match_action": "asd.ledger.append.hazard",
  "require_approval": ["human"],
  "reason": "hazard entries are load-bearing; a human must attest"
}
```

See [Policy & Ratification](/guides/policy) for the complete walk-through
and [Policy File Schema](/reference/policy-schema) for the exhaustive JSON
shape.

## 7. Ratification workflow

**What.** When a rule returns `RequireApproval`, ASD writes the entry with
`awaiting-approval` and `approver:<label>` tags. Four terminal verbs close
the loop:

- **`approve`** — flips to `approved`, stamps `approved-by:<id>` and
  `approved-at:<timestamp>`.
- **`reject`** — flips to `rejected`, stamps `rejected-by:<id>`,
  `rejected-at:<timestamp>`, appends a required `reason` to the entry body.
- **`withdraw`** — original author retracts the pending entry. Flips to
  `withdrawn`.
- **`supersede`** — new entry replaces one or more prior entries (any status);
  the prior entries stay visible with `--include-superseded`.

All four are idempotent — re-approving an already-approved entry returns
`already-approved` rather than erroring.

**Example.**

```bash
asd ledger approve led_f5e4... --approver alice --approver-kind human \
  --message "verified boundary in test_payments.py"
```

```json
{ "status": "approved", "entry_id": "led_f5e4...",
  "tags": ["approver:human", "approved", "approved-by:alice", "approved-at:..."] }
```

## 8. Audit event stream

**What.** Every mutation — ledger append/approve/reject/withdraw/supersede
plus effect_declare — emits one structured JSONL event to the configured
sink. Same shape across CLI, HTTP, and MCP entry points. Enable with
`--audit-log <path>` (CLI) or `ASD_AUDIT_LOG` (daemon binaries).

**Why.** Post-incident forensics. SIEM integration (Splunk / Loki / Datadog
ingest JSONL natively). Proof-of-attestation.

**Where.** The file you point `--audit-log` at. One event per line.

**Example.**

```bash
asd --audit-log ./audit.jsonl audit tail --event-type ledger --limit 3
```

```json
{
  "count": 3,
  "events": [
    { "event_id": "evt_...", "event_type": "ledger.append",
      "actor_id": "alice", "actor_kind": "human",
      "outcome": "awaiting-approval",
      "matched_policy": "/policies/code/hazard-requires-human@1",
      "subject_id": "led_...", "secondary_id": "sym_...", ... }
  ]
}
```

See [Audit Log](/guides/audit) and [Audit Event Schema](/reference/audit-schema)
for the exhaustive event vocabulary.

## How the primitives compose

A typical agent turn:

1. Agent reads the symbol via `code_read` (MCP) → gets source + declared
   effects + recent ledger entries in one call.
2. Agent proposes a change; drafts a `decision` ledger entry via
   `ledger_append` → policy gate evaluates → entry may land `allowed`,
   `awaiting-approval`, or `denied`.
3. Audit event is written for the append.
4. A human reviewer picks up the `awaiting-approval` queue via
   `/api/v1/ledger?tag=awaiting-approval` and approves or rejects.
5. The approve / reject action emits its own audit event.
6. On next `asd index`, changed symbol bodies get new fingerprints; the
   ledger entries still attach because they key on `symbol_id`, not `symbol_fp`.

No single primitive is load-bearing on its own — it's the composition that
produces the audit overlay.
