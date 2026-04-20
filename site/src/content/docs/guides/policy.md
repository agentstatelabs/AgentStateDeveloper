---
title: Policy & Ratification
description: File-backed policy gate, rule matching, the four ratification verbs, and how matched_policy stamps every entry.
---

The policy gate sits between every consequential write and the ASG store. It
takes `(action, situation, agent_id)` and returns one of four decisions:
`Allow`, `Deny`, `RequireApproval`, or `NoPolicyMatch`. The gate is file-
backed — a single JSON file loaded once at engine open.

## Enabling the gate

By default (solo-dev), no policy is loaded; every write is `Allow` with
`matched_policy: null`. Pass `--policy <path>` (CLI) or set `ASD_POLICY=
<path>` (daemon binaries) to enforce rules:

```bash
asd --policy ./examples/policies.json ledger append payments.charge_card \
  --kind hazard --summary "boundary at 10000" --author-id alice
```

```json
{
  "entry_id": "led_f5e4...",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "status": "awaiting-approval"
}
```

## The rule schema

Each rule in the file:

```json
{
  "path": "/policies/code/hazard-requires-human",
  "version": 1,
  "description": "Hazard ledger entries record load-bearing warnings...",
  "match_action": "asd.ledger.append.hazard",
  "deny": false,
  "require_approval": ["human"],
  "reason": "hazard entries are load-bearing; a human must attest",
  "agent_id": null
}
```

- **`path`** — policy identifier surfaced on every matched write as
  `matched_policy: <path>@<version>`.
- **`version`** — integer. Bump on rule change; older stamped entries still
  carry the old version.
- **`match_action`** — exact match (`asd.ledger.append.hazard`) or prefix
  wildcard (`asd.ledger.*` matches `asd.ledger.append.decision`,
  `asd.ledger.supersede`, etc.).
- **`deny: true`** — produces a `Deny` decision.
- **`require_approval: ["human"]`** — produces `RequireApproval` with the
  listed approver labels. If both `deny: true` and `require_approval` is
  non-empty, `require_approval` wins.
- **`reason`** — surfaced in the audit event and deny / approval message.
- **`agent_id`** — optional equality pin. Rule only fires when the caller's
  `agent_id` matches.

Top-level `strict: true` flips unmatched actions to `NoPolicyMatch` instead
of the permissive `Allow` default — callers can then decide fail-safe
behavior.

First matching rule wins; order in the file matters.

See [the full schema reference](/reference/policy-schema) for the exhaustive
JSON.

## Action taxonomy

The canonical ASD actions (from `agentstatedeveloper_core::actions`):

| Action | Emitted when |
|---|---|
| `asd.ledger.append.<kind>` | Any ledger append. `<kind>` = `decision` / `hazard` / `tradeoff` / etc. |
| `asd.ledger.supersede` | `ledger_supersede` call |
| `asd.effect.declare` | `effect_declare` that does not add a new effect category |
| `asd.effect.declare.broadens` | `effect_declare` that introduces an effect category not previously declared |
| `asd.code.read` | (reserved; no current enforcement) |
| `asd.code.commit` | (reserved) |
| `asd.merge.branch_to_main` | (reserved) |
| `asd.rename.symbol` | (reserved) |
| `asd.rename.file` | (reserved) |

Today the gate fires on ledger append / supersede and on effect_declare. The
reserved actions are in the taxonomy so rule files are forward-compatible,
but they don't yet have call sites.

## The four ratification verbs

When a rule returns `RequireApproval`, the entry is written with two tags:

- `awaiting-approval`
- one `approver:<label>` per item in `require_approval` (e.g. `approver:human`,
  `approver:senior_agent`)

Four verbs resolve the pending state:

### `approve`

```bash
asd ledger approve led_f5e4... --approver alice --approver-kind human \
  --message "verified test coverage for boundary"
```

- Flips `awaiting-approval` → `approved`.
- Adds `approved-by:<id>` and `approved-at:<timestamp>`.
- Enforces approver-match: the `approver_kind` or `approver` value must
  match one of the entry's `approver:*` tags.
- **Idempotent.** Re-approving returns `status: "already-approved"`.

### `reject`

```bash
asd ledger reject led_f5e4... --reviewer alice --reviewer-kind human \
  --reason "boundary isn't enforced at all; claim is incorrect"
```

- Flips `awaiting-approval` → `rejected`.
- Adds `rejected-by:<id>`, `rejected-at:<timestamp>`, appends `reason` to
  the entry body.
- `reason` is required.
- Idempotent.

### `withdraw`

```bash
asd ledger withdraw led_f5e4... --author-id review-bot
```

- Flips `awaiting-approval` → `withdrawn`.
- **Must be called by the original author** (`author_id` matching
  `entry.author.id`). Different actor gets `unauthorized`.
- Idempotent.

### `supersede`

```bash
asd ledger supersede payments.charge_card \
  --supersede led_a1b2... \
  --kind decision \
  --summary "replaces earlier hazard with concrete mitigation plan"
```

- Writes a *new* entry with `supersedes: [led_a1b2...]`.
- The new entry is visible in the default ledger view; the superseded one
  disappears from the default view but is preserved with
  `--include-superseded`.
- Can chain supersede of supersedes — the history stays walkable.

## `matched_policy`: the audit trail

Every `Allow` / `RequireApproval` / `Deny` stamps `matched_policy:
<path>@<version>` onto the resulting entry (or records it in the audit event
alone for `Deny`, which produces no entry). Reading an entry later always
tells you *which rule landed it here*:

```json
{
  "entry_id": "led_f5e4...",
  "symbol_id": "sym_...",
  "kind": "hazard",
  "summary": "boundary at 10000",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "tags": ["approver:human", "approved", "approved-by:alice@example.com", "approved-at:2026-04-17T14:22:03Z"]
}
```

When you rotate a policy, bump the `version`. Existing entries keep pointing
at the older version number so you can tell "this was approved under the
old rule" versus "under the new one."

## Current limits

- Policy file is loaded once at engine open; changes require restart.
- `match_action` supports exact match and `.*` suffix only — no complex
  selector DSL yet.
- Gate fires on ledger append / supersede and effect_declare. Traces, index,
  and rename surfaces are not yet gated.
- `FilePolicyGate` is the interim implementation. The planned
  `agentstategraph-policy` sibling crate will replace it with a superset
  schema — the migration is a rename on the ASD side, no logic change.

The full policy schema reference is at
[Policy File Schema](/reference/policy-schema). The actions and outcomes
emitted to the audit log are documented at
[Audit Event Schema](/reference/audit-schema).
