---
title: Policy File Schema
description: JSON schema for asd policy files — rules, match actions, decision shapes, canonical action taxonomy, and a working example.
---

A policy file is a single JSON document passed via `--policy <path>` or
`ASD_POLICY`. It defines a list of rules evaluated in order against every
consequential write. First match wins.

## Top-level

```json
{
  "strict": false,
  "policies": [ /* array of PolicyRule */ ]
}
```

- **`policies`** — array of rules, evaluated top-to-bottom. First match wins.
- **`strict`** (default `false`) — when `true`, actions that match no rule
  return `NoPolicyMatch` (the caller decides fail-safe behavior). When
  `false`, unmatched actions become `Allow` with `matched_policy: null` — the
  solo-dev-friendly default.

## PolicyRule

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

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | string | yes | Policy identifier. Stamped on matched writes as `matched_policy: <path>@<version>`. Convention: `/policies/<domain>/<slug>`. |
| `version` | integer | no (default `1`) | Bump when rule changes. Stamped alongside `path` so pre-existing entries keep pointing at the rule version they were written under. |
| `description` | string | no | Human-readable; not evaluated. |
| `match_action` | string | yes | Exact match (`asd.ledger.append.hazard`) or prefix wildcard (`asd.ledger.*` matches any subaction). |
| `deny` | bool | no (default `false`) | When `true` and `require_approval` is empty, decision is `Deny`. |
| `require_approval` | string[] | no (default `[]`) | When non-empty, decision is `RequireApproval` with these labels as approver kinds. Wins over `deny: true` when both are set. |
| `reason` | string | no | Surfaced to the caller on Deny / Approval and into the audit event. |
| `agent_id` | string | no | Equality pin. When set, rule fires only when the caller's `agent_id` matches. |

## Match semantics

- **Exact:** `"match_action": "asd.ledger.append.hazard"` matches only that
  action.
- **Prefix wildcard:** `"match_action": "asd.ledger.*"` matches
  `asd.ledger`, `asd.ledger.append.decision`, `asd.ledger.supersede`, and
  every other action starting with `asd.ledger.`.
- **`agent_id` pin:** when set, the rule only fires if the caller's
  `agent_id` exactly matches. Used for "this specific bot cannot write
  tradeoffs" patterns.

First matching rule wins, so order matters. Put most-specific rules first.

## Decision precedence

Inside one rule:

1. `require_approval` non-empty → `RequireApproval` with that list.
2. Else `deny: true` → `Deny`.
3. Else → `Allow` with `matched_policy: <path>@<version>`.

`NoPolicyMatch` arises only at file-level, when `strict: true` and no rule
matched.

## Canonical action taxonomy

The actions ASD currently emits (from
`agentstatedeveloper_core::actions`):

| Action | When emitted |
|---|---|
| `asd.ledger.append.decision` | `ledger_append` with `kind=decision` |
| `asd.ledger.append.assumption` | `kind=assumption` |
| `asd.ledger.append.constraint` | `kind=constraint` |
| `asd.ledger.append.rationale` | `kind=rationale` |
| `asd.ledger.append.hazard` | `kind=hazard` |
| `asd.ledger.append.tradeoff` | `kind=tradeoff` |
| `asd.ledger.supersede` | any `ledger_supersede` call |
| `asd.effect.declare` | `effect_declare` that does not introduce a new category |
| `asd.effect.declare.broadens` | `effect_declare` that introduces a new category |

Reserved (in taxonomy, no active call site yet — rules here are
forward-compatible but inert):

- `asd.code.read`
- `asd.code.commit`
- `asd.merge.branch_to_main`
- `asd.rename.symbol`
- `asd.rename.file`

## Working example

Verbatim from `examples/policies.json`:

```json
{
  "strict": false,
  "policies": [
    {
      "path": "/policies/code/hazard-requires-human",
      "version": 1,
      "description": "Hazard ledger entries record load-bearing warnings. A human must approve before they land so an agent can't silently escalate risk.",
      "match_action": "asd.ledger.append.hazard",
      "deny": false,
      "require_approval": ["human"],
      "reason": "hazard entries are load-bearing; a human must attest"
    },
    {
      "path": "/policies/code/supersede-requires-senior-review",
      "version": 1,
      "description": "Superseding existing ledger entries can erase institutional context. Requires a senior-agent or human attestation.",
      "match_action": "asd.ledger.supersede",
      "deny": false,
      "require_approval": ["senior_agent", "human"],
      "reason": "supersede can drop context; needs second reviewer"
    },
    {
      "path": "/policies/code/broaden-net-effects",
      "version": 1,
      "description": "Declaring new outbound network effects widens the attack surface. Approval required.",
      "match_action": "asd.effect.declare.broadens",
      "deny": false,
      "require_approval": ["human"],
      "reason": "net surface widening must be reviewed"
    },
    {
      "path": "/policies/code/no-tradeoffs-without-body",
      "version": 1,
      "description": "Example hard-deny: agent nick 'experimental-bot' isn't allowed to write tradeoff entries at all.",
      "match_action": "asd.ledger.append.tradeoff",
      "deny": true,
      "reason": "experimental-bot has not earned tradeoff-write authority",
      "agent_id": "experimental-bot"
    }
  ]
}
```

Readout of that file:

- Hazard appends (any agent) → `RequireApproval ["human"]`.
- Supersedes (any agent) → `RequireApproval ["senior_agent", "human"]`.
- Broadening effect declares (any agent) → `RequireApproval ["human"]`.
- Tradeoff appends by `experimental-bot` → `Deny`. Tradeoffs by other agents
  → `Allow` (no rule matches).
- Every other action → `Allow` (non-strict, no rule matches).

## Introspection

```bash
asd --policy ./policies.json policy list
asd --policy ./policies.json policy show /policies/code/hazard-requires-human
asd --policy ./policies.json policy evaluate asd.ledger.append.hazard --agent-id review-bot
```

See the [Policy guide](/guides/policy) for the write-path workflow and
[Audit Event Schema](/reference/audit-schema) for the outcomes emitted on
each decision.
