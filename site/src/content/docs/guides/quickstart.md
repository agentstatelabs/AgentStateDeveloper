---
title: Quick Start
description: Clone, build, index a sample repository, and append your first ledger entry in five minutes.
---

This walkthrough takes you from a fresh clone to a policy-gated ledger entry
against the included Python sample repo. ASD supports nine languages out of
the box (Python, TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift) — point
`asd index` at any directory and it picks the right adapter automatically.

## 1. Build

```bash
git clone https://github.com/agentstatelabs/AgentStateDeveloper.git
cd AgentStateDeveloper
cargo build --release
```

The resulting binaries land in `target/release/`:

- `asd` — CLI
- `asd-mcp` — MCP stdio server
- `asd-serve` — HTTP server + Lens UI host

Put `target/release/` on your `PATH` or invoke the binaries by full path. The
rest of this guide assumes `asd` is on `PATH`.

## 2. Initialize a repository

From the root of the workspace:

```bash
cd examples/sample-py-repo
asd init
```

```text
initialized at ./.asd-state.db
```

`asd init` creates a local SQLite-backed ASG repository at `./.asd-state.db`
and stamps the ASD schema marker (`/asd/v1/meta/schema-version`).

## 3. Index the sample

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

Every recognized source file under the directory is parsed. Symbols land at
`/asd/v1/index/by-qname/<qname>`; inferred effects at
`/asd/v1/effects/<symbol_id>`; call edges at `/asd/v1/index/callees/` and
`/asd/v1/index/callers/`. `transitive_updates` reports how many symbols
received propagated effects from their callees.

## 4. Read a symbol

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
      { "effect": "log",         "note": "log.info(\"charging user...\")" },
      { "effect": "io.db.write", "note": "db.execute(\"INSERT INTO charges...\")" },
      { "effect": "throw",       "note": "raise ValueError(...)" }
    ],
    "transitive": [],
    "verification": {
      "by": "static-checker",
      "status": "unverified"
    }
  },
  "ledger": []
}
```

`asd read` is the primary "give an agent the context for this symbol" entry
point — it returns the parsed symbol, its declared and transitive effects, and
up to 5 ledger entries in a single JSON object.

## 5. Append a ledger entry

```bash
asd ledger append payments.charge_card \
  --kind hazard \
  --summary "rejects amounts over 10000 — silent failure path if caller ignores exception" \
  --author-kind human \
  --author-id alice@example.com
```

```json
{
  "entry_id": "led_a1b2c3d4...",
  "matched_policy": null,
  "status": "allowed"
}
```

With no policy loaded, solo-dev default is permissive — every write is
`allowed` and `matched_policy` is `null`.

## 6. Load a policy

The bundled `examples/policies.json` requires human approval for hazard
entries. Point `asd` at it:

```bash
asd --policy ../../examples/policies.json \
    ledger append payments.charge_card \
    --kind hazard \
    --summary "boundary amount is 10000, not documented in signature" \
    --author-kind agent \
    --author-id review-bot
```

```json
{
  "entry_id": "led_f5e4d3c2...",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "status": "awaiting-approval"
}
```

The entry still lands — with `awaiting-approval` and `approver:human` tags
attached. A human approver closes the loop:

```bash
asd --policy ../../examples/policies.json \
    ledger approve led_f5e4d3c2... \
    --approver alice@example.com \
    --approver-kind human \
    --message "verified boundary in tests/test_payments.py"
```

```json
{
  "status": "approved",
  "entry_id": "led_f5e4d3c2...",
  "symbol_id": "sym_...",
  "tags": [
    "approver:human",
    "approved",
    "approved-by:alice@example.com",
    "approved-at:2026-04-17T14:22:03Z"
  ]
}
```

## 7. Wire up the audit log

Every ledger mutation and policy decision can be mirrored to JSONL:

```bash
asd --policy ../../examples/policies.json \
    --audit-log ./audit.jsonl \
    ledger append payments.get_balance \
    --kind rationale \
    --summary "returns 0.0 on missing row to avoid forcing callers to handle None"

asd --audit-log ./audit.jsonl audit tail --limit 5
```

```json
{
  "path": "./audit.jsonl",
  "count": 1,
  "events": [
    {
      "event_id": "evt_abc...",
      "event_type": "ledger.append",
      "actor_id": "asd-cli-user",
      "actor_kind": "agent",
      "outcome": "allowed",
      "timestamp": "2026-04-17T14:25:11Z",
      "subject_id": "led_...",
      "secondary_id": "sym_...",
      "payload": { "qname": "payments.get_balance", "kind": "rationale", "tags": [] }
    }
  ]
}
```

Pipe that file into Splunk, Loki, or Datadog — one line per event, no
transformation required.

## Next

- [Core Concepts](/guides/concepts) — the seven primitives in detail.
- [CLI reference](/reference/cli) — every subcommand, flag, and env var.
- [MCP tools](/reference/mcp-tools) — the 14-tool surface for agents.
