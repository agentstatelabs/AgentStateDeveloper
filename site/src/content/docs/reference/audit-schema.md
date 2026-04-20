---
title: Audit Event Schema
description: JSONL event shape emitted by the asd audit sink — fields, event types, outcome values, and success/error examples for every event type.
---

Every consequential write emits one `AuditEvent` to the configured sink.
The JSONL format is one event per line, UTF-8 JSON. Schema is stable —
downstream SIEM rules can key on `event_type`, `outcome`, and
`matched_policy` without worrying about format drift.

## Event shape

```json
{
  "event_id": "evt_2f3a1c9d8b5e4f7a...",
  "event_type": "ledger.append",
  "subject_id": "led_a1b2c3d4...",
  "secondary_id": "sym_payments_charge_card...",
  "actor_id": "alice@example.com",
  "actor_kind": "human",
  "timestamp": "2026-04-17T14:22:03.519Z",
  "outcome": "awaiting-approval",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "reason": null,
  "payload": {
    "qname": "payments.charge_card",
    "kind": "hazard",
    "tags": ["approver:human", "awaiting-approval"]
  }
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `event_id` | string | yes | `evt_<uuid-simple>`. Unique per event. |
| `event_type` | string | yes | One of the canonical types below. |
| `subject_id` | string | optional | Entry id for ledger ops, symbol id for effect ops. Omitted when the op has no persistent subject. |
| `secondary_id` | string | optional | Secondary subject — typically the symbol id when `subject_id` is an entry id. |
| `actor_id` | string | yes | Who initiated the action. |
| `actor_kind` | string | yes | `"agent"`, `"human"`, or `"system"`. |
| `timestamp` | RFC3339 | yes | UTC. |
| `outcome` | string | yes | See below. |
| `matched_policy` | string | optional | `<path>@<version>` when a rule matched. |
| `reason` | string | optional | Human-readable explanation. Used for deny reasons, error messages, rejection reasons, approval notes. |
| `payload` | object | optional | Op-specific extras. Schemaless. |

Optional fields are omitted entirely when unset (no `null` in the JSON).

## Canonical event types

```
ledger.append
ledger.approve
ledger.reject
ledger.withdraw
ledger.supersede
effect.declare
```

## Outcome vocabulary

The `outcome` string is free-form per call site, but a stable set of values
is used consistently:

- `success` — generic success (used for `ledger.supersede`).
- `allowed` — policy gate returned `Allow`, write landed.
- `awaiting-approval` — policy gate returned `RequireApproval`, entry was
  written with `awaiting-approval` tag.
- `approved` — `ledger.approve` succeeded, entry flipped to approved.
- `rejected` — `ledger.reject` succeeded.
- `withdrawn` — `ledger.withdraw` succeeded.
- `denied` — policy gate returned `Deny`; no entry was written.
- `already-approved` / `already-rejected` / `already-withdrawn` —
  idempotent repeat of a prior action.
- `no-policy-match` — strict mode and no rule matched; entry was not
  written.
- `unauthorized` — caller failed the approver-match or withdraw-author-id
  check.
- `error` — anything else (symbol not found, storage error, policy
  evaluation error).

## Per-event examples

### `ledger.append` — success

```json
{"event_id":"evt_abc...","event_type":"ledger.append","subject_id":"led_1...","secondary_id":"sym_1...","actor_id":"review-bot","actor_kind":"agent","timestamp":"2026-04-17T14:20:01Z","outcome":"allowed","payload":{"qname":"payments.get_balance","kind":"rationale","tags":[]}}
```

### `ledger.append` — awaiting approval (policy match)

```json
{"event_id":"evt_def...","event_type":"ledger.append","subject_id":"led_2...","secondary_id":"sym_1...","actor_id":"alice@example.com","actor_kind":"human","timestamp":"2026-04-17T14:22:03Z","outcome":"awaiting-approval","matched_policy":"/policies/code/hazard-requires-human@1","payload":{"qname":"payments.charge_card","kind":"hazard","tags":["approver:human","awaiting-approval"]}}
```

### `ledger.append` — denied

No entry is written. Event carries `matched_policy` + `reason`.

```json
{"event_id":"evt_ghi...","event_type":"ledger.append","secondary_id":"sym_1...","actor_id":"experimental-bot","actor_kind":"agent","timestamp":"2026-04-17T14:23:10Z","outcome":"denied","matched_policy":"/policies/code/no-tradeoffs-without-body@1","reason":"experimental-bot has not earned tradeoff-write authority","payload":{"qname":"payments.charge_card","kind":"tradeoff"}}
```

### `ledger.append` — error (symbol not found)

```json
{"event_id":"evt_jkl...","event_type":"ledger.append","actor_id":"review-bot","actor_kind":"agent","timestamp":"2026-04-17T14:24:00Z","outcome":"error","reason":"symbol not found: payments.unknown","payload":{"qname":"payments.unknown","kind":"decision"}}
```

### `ledger.approve` — success

```json
{"event_id":"evt_mno...","event_type":"ledger.approve","subject_id":"led_2...","secondary_id":"sym_1...","actor_id":"alice@example.com","actor_kind":"human","timestamp":"2026-04-17T14:30:00Z","outcome":"approved","matched_policy":"/policies/code/hazard-requires-human@1","payload":{"tags":["approver:human","approved","approved-by:alice@example.com","approved-at:2026-04-17T14:30:00Z"]}}
```

### `ledger.approve` — already approved

```json
{"event_id":"evt_mnp...","event_type":"ledger.approve","subject_id":"led_2...","secondary_id":"sym_1...","actor_id":"alice@example.com","actor_kind":"human","timestamp":"2026-04-17T14:31:00Z","outcome":"already-approved","matched_policy":"/policies/code/hazard-requires-human@1","payload":{"tags":["approver:human","approved","approved-by:alice@example.com","approved-at:2026-04-17T14:30:00Z"]}}
```

### `ledger.reject` — success

```json
{"event_id":"evt_pqr...","event_type":"ledger.reject","subject_id":"led_2...","secondary_id":"sym_1...","actor_id":"alice@example.com","actor_kind":"human","timestamp":"2026-04-17T14:35:00Z","outcome":"rejected","matched_policy":"/policies/code/hazard-requires-human@1","reason":"boundary isn't enforced at claimed line","payload":{"tags":["approver:human","rejected","rejected-by:alice@example.com","rejected-at:2026-04-17T14:35:00Z"]}}
```

### `ledger.reject` — error (unauthorized)

```json
{"event_id":"evt_pqs...","event_type":"ledger.reject","subject_id":"led_2...","actor_id":"other-bot","actor_kind":"agent","timestamp":"2026-04-17T14:36:00Z","outcome":"error","reason":"reviewer kind 'agent' does not match any approver tag on entry"}
```

### `ledger.withdraw` — success

```json
{"event_id":"evt_stu...","event_type":"ledger.withdraw","subject_id":"led_5...","secondary_id":"sym_2...","actor_id":"review-bot","actor_kind":"agent","timestamp":"2026-04-17T14:40:00Z","outcome":"withdrawn","payload":{"tags":["approver:human","withdrawn"]}}
```

### `ledger.withdraw` — error (mismatched author)

```json
{"event_id":"evt_stv...","event_type":"ledger.withdraw","subject_id":"led_5...","actor_id":"some-other-bot","actor_kind":"agent","timestamp":"2026-04-17T14:41:00Z","outcome":"error","reason":"author_id does not match entry.author.id"}
```

### `ledger.supersede` — success

```json
{"event_id":"evt_vwx...","event_type":"ledger.supersede","subject_id":"led_9...","secondary_id":"sym_1...","actor_id":"alice@example.com","actor_kind":"human","timestamp":"2026-04-17T15:00:00Z","outcome":"success","payload":{"supersedes":["led_2..."],"qname":"payments.charge_card"}}
```

### `effect.declare` — success (non-broadening)

```json
{"event_id":"evt_yz1...","event_type":"effect.declare","subject_id":"sym_1...","actor_id":"review-bot","actor_kind":"agent","timestamp":"2026-04-17T15:10:00Z","outcome":"allowed","payload":{"qname":"payments.charge_card","declared":["log","io.db.write","throw"],"broadens":false,"action":"asd.effect.declare"}}
```

### `effect.declare` — awaiting approval (broadening)

```json
{"event_id":"evt_yz2...","event_type":"effect.declare","subject_id":"sym_1...","actor_id":"review-bot","actor_kind":"agent","timestamp":"2026-04-17T15:12:00Z","outcome":"awaiting-approval","matched_policy":"/policies/code/broaden-net-effects@1","payload":{"qname":"payments.charge_card","declared":["log","io.db.write","throw","io.net.out"],"broadens":true,"action":"asd.effect.declare.broadens"}}
```

### `effect.declare` — denied

```json
{"event_id":"evt_yz3...","event_type":"effect.declare","subject_id":"sym_1...","actor_id":"experimental-bot","actor_kind":"agent","timestamp":"2026-04-17T15:15:00Z","outcome":"denied","matched_policy":"/policies/code/...","reason":"..."}
```

## Reading back

```bash
asd --audit-log ./audit.jsonl audit tail --limit 5
asd audit tail --log ./audit.jsonl --event-type ledger.approve --actor alice@example.com
asd audit tail --log ./audit.jsonl --outcome denied
```

Or parse the file directly — it is plain JSONL:

```bash
jq 'select(.outcome == "denied")' audit.jsonl
```

## One event per operation

Across CLI, MCP, and HTTP surfaces, every mutation emits exactly one event
before returning. Denies emit an event even though no entry was written.
Errors emit an event with `outcome: "error"` and a populated `reason`. Sink
failures are logged to stderr but never block the user's operation — the
audit log is a recorder, not a gate.
