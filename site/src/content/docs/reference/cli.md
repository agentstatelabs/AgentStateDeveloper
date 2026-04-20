---
title: CLI (`asd`)
description: Full reference for the asd binary — global flags, subcommands, and environment variables.
---

`asd` is the CLI that drives the AgentStateDeveloper engine. All subcommands
open a single SQLite-backed ASG repository, optionally load a policy file,
and optionally wire up the JSONL audit sink.

## Global flags

All flags are `global = true` — they can appear before or after a subcommand.

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--db <path>` | `ASD_DB` | `./.asd-state.db` | Path to the ASD SQLite database. |
| `--policy <path>` | `ASD_POLICY` | (none — permissive) | JSON policy file. When absent, every write is `Allow`. |
| `--audit-log <path>` | `ASD_AUDIT_LOG` | (none — discarded) | Append-only JSONL audit log. |

CLI flag takes precedence over env var; env var over default.

---

## `asd init`

Create or reuse an ASD repository. Stamps the ASD schema marker at
`/asd/v1/meta/schema-version`.

```bash
asd init
```

```text
initialized at ./.asd-state.db
```

No flags.

---

## `asd index <path>`

Walk a directory for source files with known extensions (`.py`, `.ts`,
`.tsx`, `.mts`, `.cts`), parse them, persist symbols, infer effects, extract
call edges, and propagate transitive effects.

Unknown extensions are skipped silently. `node_modules`, `.venv`, `.git`,
`__pycache__`, `dist`, `build`, `.next`, `.tox`, and `.mypy_cache` are
pruned from the walk.

```bash
asd index .
```

```json
{
  "files": 4,
  "symbols": 13,
  "effects": 13,
  "edges": 5,
  "intra_module_edges": 3,
  "cross_module_edges": 2,
  "transitive_updates": 3
}
```

Arguments:

- `<path>` — directory (recursively walked) or a single source file.

---

## `asd read <qname>`

Fetch a symbol by qname. Returns the symbol record, its effect declaration,
and up to 5 most recent ledger entries.

```bash
asd read payments.charge_card
```

```json
{
  "symbol": {
    "symbol_id": "sym_...",
    "qname": "payments.charge_card",
    "language": "python",
    "kind": "function",
    "file": "payments.py",
    "start": { "line": 26, "col": 1 },
    "end":   { "line": 32, "col": 49 },
    "signature": "def charge_card(user_id: str, amount: float)"
  },
  "effects": {
    "declared": [
      { "effect": "log", "note": "log.info(...)" },
      { "effect": "io.db.write", "note": "db.execute(\"INSERT INTO charges...\")" },
      { "effect": "throw", "note": "raise ValueError(...)" }
    ],
    "transitive": [],
    "verification": { "by": "static-checker", "at": "...", "status": "unverified", "mismatches": [] }
  },
  "ledger": []
}
```

---

## `asd ledger append <qname>`

Append a ledger entry to a symbol. Routes through the configured policy gate.

```bash
asd ledger append payments.charge_card \
  --kind hazard \
  --summary "boundary at 10000 is undocumented in signature" \
  --body ./notes.md \
  --author-kind human \
  --author-id alice@example.com
```

```json
{
  "entry_id": "led_a1b2c3d4...",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "status": "awaiting-approval"
}
```

Arguments / flags:

- `<qname>` — fully-qualified symbol name.
- `--kind <decision|assumption|constraint|rationale|hazard|tradeoff>` —
  required.
- `--summary <text>` — one-line, required.
- `--body <path>` — optional markdown / plain-text body.
- `--author-kind <agent|human>` — default `agent`.
- `--author-id <id>` — default `asd-cli-user`.

`status` values: `allowed`, `awaiting-approval`, `denied` (deny also exits
non-zero), `no-policy-match`.

---

## `asd ledger approve <entry_id>`

Approve an `awaiting-approval` entry.

```bash
asd ledger approve led_a1b2... \
  --approver alice@example.com \
  --approver-kind human \
  --message "verified boundary in test_payments.py"
```

```json
{
  "status": "approved",
  "entry_id": "led_a1b2...",
  "symbol_id": "sym_...",
  "tags": ["approver:human", "approved", "approved-by:alice@example.com", "approved-at:..."]
}
```

Flags:

- `--approver <id>` — required. Stamped as `approved-by:<id>`.
- `--approver-kind <label>` — default `human`. Must match an `approver:*`
  tag on the pending entry, unless `--approver` itself matches directly.
- `--message <text>` — optional rationale appended to the entry body.

Idempotent — re-approving returns `status: "already-approved"`.

---

## `asd ledger reject <entry_id>`

Reject an `awaiting-approval` entry.

```bash
asd ledger reject led_a1b2... \
  --reviewer alice@example.com \
  --reviewer-kind human \
  --reason "boundary is not enforced at the claimed line"
```

```json
{
  "status": "rejected",
  "entry_id": "led_a1b2...",
  "symbol_id": "sym_...",
  "tags": ["approver:human", "rejected", "rejected-by:alice@example.com", "rejected-at:..."]
}
```

Flags:

- `--reviewer <id>` — required.
- `--reviewer-kind <label>` — default `human`. Same approver-match rule as
  `approve`.
- `--reason <text>` — required. Appended to the entry body.

Idempotent.

---

## `asd ledger withdraw <entry_id>`

Original author retracts a pending entry.

```bash
asd ledger withdraw led_a1b2... --author-id review-bot
```

```json
{
  "status": "withdrawn",
  "entry_id": "led_a1b2...",
  "symbol_id": "sym_...",
  "tags": ["approver:human", "withdrawn"]
}
```

Flags:

- `--author-id <id>` — required. Must match `entry.author.id`.

Idempotent.

---

## `asd ledger supersede <qname>`

Write a new entry that supersedes one or more existing entries on the same
symbol.

```bash
asd ledger supersede payments.charge_card \
  --supersede led_a1b2... \
  --supersede led_c3d4... \
  --kind decision \
  --summary "replaces earlier hazard with concrete mitigation plan" \
  --body ./mitigation.md \
  --author-kind human \
  --author-id alice@example.com
```

```json
{
  "status": "superseded",
  "entry_id": "led_f5e6...",
  "symbol_id": "sym_...",
  "supersedes": ["led_a1b2...", "led_c3d4..."]
}
```

Flags:

- `--supersede <entry_id>` — repeatable; at least one required.
- `--kind <...>` — required.
- `--summary <text>` — required.
- `--body <path>` — optional.
- `--author-kind` / `--author-id` — same as `append`.

The superseded entries stay in storage; they're filtered from default
`ledger_get` views but visible with `--include-superseded` on the MCP tool.

---

## `asd verify-effects <qname>`

M1 placeholder. Returns the declared effect set with `status: "unverified"`.
A dedicated static checker is a future milestone; today, runtime verification
runs through `asd trace`.

```bash
asd verify-effects payments.charge_card
```

```json
{
  "qname": "payments.charge_card",
  "symbol_id": "sym_...",
  "status": "unverified",
  "declared": [
    { "effect": "log", "note": "..." },
    { "effect": "io.db.write", "note": "..." },
    { "effect": "throw", "note": "..." }
  ]
}
```

---

## `asd trace -- <cmd...>`

Run a Python program under the ASD runtime tracer and ingest the observed
effects. Updates each touched symbol's `verification` block.

```bash
asd trace -- python _driver.py
asd trace --out ./my-trace.json -- python -m pytest tests/
```

Flags:

- `--out <path>` — tracer report output path. Default `.asd-trace.json`.
- `--` — separator; everything after is the command to trace.

Output:

```json
{
  "exit_code": 0,
  "report_path": ".asd-trace.json",
  "traced_qnames": 4,
  "updates": [
    { "qname": "payments.charge_card", "status": "ok", "mismatches": [] },
    { "qname": "payments.get_balance", "status": "mismatch",
      "mismatches": [
        { "kind": "undeclared", "effect": "io.net.out", "detected_in": "payments.get_balance" }
      ]
    }
  ]
}
```

The traced program's exit code is propagated — CI can key on `asd trace`'s
exit status.

`asd trace` locates `tools/asd_tracer.py` at `./tools/asd_tracer.py`, at
`<binary_dir>/../../tools/asd_tracer.py`, or by walking up from cwd. Run it
from the repository root if the tracer isn't found.

---

## `asd policy list`

Requires `--policy <path>` / `ASD_POLICY`. Lists loaded rules; optionally
filters by path prefix.

```bash
asd --policy ./policies.json policy list
asd --policy ./policies.json policy list --prefix /policies/code
```

```json
{
  "source": "./policies.json",
  "strict": false,
  "count": 4,
  "policies": [
    {
      "path": "/policies/code/hazard-requires-human",
      "version": 1,
      "description": "Hazard ledger entries record load-bearing warnings...",
      "match_action": "asd.ledger.append.hazard",
      "deny": false,
      "require_approval": ["human"],
      "agent_id": null
    }
  ]
}
```

---

## `asd policy show <path>`

Dump a single rule by its `path` field.

```bash
asd --policy ./policies.json policy show /policies/code/hazard-requires-human
```

---

## `asd policy evaluate <action>`

Dry-run: evaluate a hypothetical action against the loaded policy without
writing anything.

```bash
asd --policy ./policies.json policy evaluate asd.ledger.append.hazard \
  --agent-id review-bot
```

```json
{
  "status": "awaiting-approval",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "approvers": ["human"],
  "reason": "hazard entries are load-bearing; a human must attest"
}
```

Flags:

- `--agent-id <id>` — default `asd-cli-user`.
- `--description <text>` — optional human-readable situation description
  (not evaluated; for operator-facing logs).

---

## `asd sync`

Mirror live ASG state into `.asd/v1/` on disk so it can be committed to git.

```bash
asd sync
asd sync --dir ./some-other-project
```

```json
{
  "dir": "./.asd/v1",
  "effects_written": 13,
  "ledger_entries_written": 2,
  "symbols_written": 13,
  "schema_version": "0.1.0",
  "note": "current-state only; ASG commit history is not carried in the sidecar"
}
```

Flags:

- `--dir <path>` — project root. `.asd/v1/` is appended internally. Default
  cwd.

---

## `asd hydrate`

Inverse of `sync`. Read the `.asd/v1/` sidecar and write its contents back
into the ASG repo. Use after a fresh `git clone`.

```bash
asd init
asd hydrate
asd index .
```

```json
{
  "dir": "./.asd/v1",
  "effects_loaded": 13,
  "ledger_entries_loaded": 2,
  "symbols_loaded": 13,
  "missing_schema_version": false,
  "note": "commit history not restored; run `asd index` to rebuild the semantic index and call graph"
}
```

Flags:

- `--dir <path>` — same as `sync`.

Note: hydrate does not rebuild the semantic index or call graph. Run
`asd index .` after hydrating.

---

## `asd audit tail`

Read back events from the configured JSONL audit log.

```bash
asd --audit-log ./audit.jsonl audit tail --limit 5
asd audit tail --log ./audit.jsonl --event-type ledger.approve --actor alice
asd audit tail --log ./audit.jsonl --since evt_a1b2... --limit 100
```

Flags:

- `--log <path>` — override the configured audit log path.
- `--event-type <substring>` — `ledger.append`, `ledger.`, `effect`.
- `--actor <id>` — exact match on `actor_id`.
- `--outcome <string>` — exact match (e.g. `denied`, `awaiting-approval`).
- `--since <event_id>` — exclusive cursor for tailing.
- `--limit <n>` — default `200`.

Output:

```json
{
  "path": "./audit.jsonl",
  "count": 1,
  "events": [
    {
      "event_id": "evt_...",
      "event_type": "ledger.approve",
      "actor_id": "alice@example.com",
      "actor_kind": "human",
      "outcome": "approved",
      "timestamp": "2026-04-17T14:22:03Z",
      "subject_id": "led_...",
      "secondary_id": "sym_...",
      "matched_policy": "/policies/code/hazard-requires-human@1",
      "payload": { "tags": ["approver:human", "approved", "approved-by:alice@example.com", "approved-at:..."] }
    }
  ]
}
```

---

## Exit codes

- `0` — success.
- Non-zero — any failure including policy `Deny` (surfaced as
  `policy denied: <reason>`), missing symbol, I/O error, or parse error.

For `asd trace`, the exit code of the traced program is propagated after the
trace is ingested.
