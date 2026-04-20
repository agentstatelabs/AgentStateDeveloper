---
title: Audit Log
description: Append-only JSONL event stream emitted on every ledger mutation, effect_declare, and policy decision. Ingest directly into Splunk, Loki, or Datadog.
---

Every user-visible mutation across CLI, MCP, and HTTP surfaces emits exactly
one structured `AuditEvent` to the configured sink. The default sink
discards events; enable the JSONL file sink to capture them.

## Enabling

CLI:

```bash
asd --audit-log ./audit.jsonl ledger append payments.charge_card \
  --kind decision --summary "..."
```

Daemon binaries (`asd-mcp`, `asd-serve`) read `ASD_AUDIT_LOG`:

```bash
ASD_AUDIT_LOG=./audit.jsonl asd-serve
```

The flag / env var points at a file path. The file is opened fresh on each
write (append mode) so you can rotate, tail, or truncate it out-of-band
without breaking the writer.

## Event shape

```json
{
  "event_id": "evt_2f3a1c9d...",
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

One event per line, UTF-8 JSON. No wrapper envelope, no trailing metadata.

Fields:

- **`event_id`** — `evt_<uuid-simple>`, unique per event.
- **`event_type`** — one of `ledger.append`, `ledger.approve`,
  `ledger.reject`, `ledger.withdraw`, `ledger.supersede`, `effect.declare`.
- **`subject_id`** — the entry_id for ledger ops, the symbol_id for
  effect_declare.
- **`secondary_id`** — the symbol_id when subject is an entry_id.
- **`actor_id`** — who initiated it. CLI: `--author-id` / `--approver` /
  similar. MCP: the tool's `author_id` / `approver` param. HTTP: the
  request body's `approver` / `reviewer` / `author_id`.
- **`actor_kind`** — `agent`, `human`, or `system`.
- **`outcome`** — `success`, `allowed`, `awaiting-approval`, `approved`,
  `rejected`, `withdrawn`, `superseded`, `already-approved`,
  `already-rejected`, `already-withdrawn`, `denied`, `unauthorized`, `error`.
- **`matched_policy`** — `<path>@<version>` when a rule matched.
- **`reason`** — human-readable explanation for denies, errors, rejections.
- **`payload`** — op-specific. Kept schemaless.

See [Audit Event Schema](/reference/audit-schema) for the exhaustive field
reference and per-event-type examples.

## `asd audit tail`

```bash
asd --audit-log ./audit.jsonl audit tail --limit 5
```

Supports filters for incremental polling and targeted reads:

- `--event-type <substring>` — `ledger.approve`, `ledger.`, `effect`
- `--actor <id>` — exact match on `actor_id`
- `--outcome <string>` — exact match on `outcome`
- `--since <event_id>` — exclusive cursor for tailing
- `--log <path>` — override the configured audit log path
- `--limit <n>` — cap results (default: 200)

Example: tail everything alice did today:

```bash
asd audit tail --log ./audit.jsonl --actor alice@example.com --limit 50
```

## One event per operation

Every CLI, MCP, and HTTP entry point that mutates state emits exactly one
event before returning. Denies emit an event even though no entry is
written. Errors (symbol not found, policy evaluation failure, storage
failure) emit an event with `outcome: "error"` and a `reason`. The
guarantee is *one-per-op* across every surface — if the user saw a status
code, there is exactly one line in the log.

Sink failures are logged to stderr (`"warning: audit emit failed: ..."`)
but never propagate. Audit issues never block the user's operation — the
log is a recorder, not a gate.

## SIEM integration

The JSONL format is the integration surface for every mainstream log
aggregator:

**Splunk.** Monitor the file with a `sourcetype=_json` input. Fields are
auto-extracted. Build detections on `matched_policy`, `outcome=denied`, or
`event_type=ledger.approve`.

**Loki / Promtail.** Scrape the file with a `json` stage. Label by
`event_type`, `actor_kind`, `outcome`. Stream to Grafana dashboards.

**Datadog.** Log collection agent with `source: jsonl`. Every field
becomes a Datadog attribute; `actor_id` + `outcome` are the obvious facets.

No transformation is needed. The field names are chosen to match common
SIEM taxonomies: `event_id`, `event_type`, `actor_id`, `actor_kind`,
`timestamp`, `outcome` are the vocabulary most aggregators already expect.

## What's not (yet) in the log

- `asd index` runs don't emit events — they're read-heavy and derivable.
- `asd trace` runs don't emit per-observation events; only the per-symbol
  verification status change is implicit in `effect.declare` on re-index.
- Code reads don't emit events. The `asd.code.read` action is reserved in
  the policy taxonomy but has no call site yet.
- No hash-chaining for tamper evidence — a sophisticated actor with
  filesystem access could rewrite the log. For enterprise-grade integrity,
  ship to an append-only SIEM as fast as possible.
