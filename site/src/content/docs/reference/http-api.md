---
title: HTTP API
description: Complete route reference for asd-serve. JSON API powering the Lens review UI and any external integrator.
---

`asd-serve` (from `agentstatedeveloper-mcp` crate, binary `asd-serve`) hosts
the HTTP JSON API that the Lens SvelteKit UI consumes. Every route is
prefixed with `/api/v1`. All responses are `application/json`; permissive
CORS is enabled.

Start the server:

```bash
asd-serve --db ./.asd-state.db
```

When the server is built with a `lens_dir` pointing at the built SvelteKit
SPA, unmatched non-API requests fall through to static serving so a single
binary ships the API and the UI together.

No authentication is enforced — localhost-only is assumed. Enterprise
deployments add API key / RBAC in front.

---

## `GET /api/v1/health`

Health check.

**Response:**

```json
{
  "status": "ok",
  "db_path": "/home/user/project/.asd-state.db",
  "symbol_count": 13
}
```

```bash
curl http://localhost:8080/api/v1/health
```

---

## `GET /api/v1/symbols`

List every indexed symbol, sorted by qname.

**Response:** array of `Symbol` records.

```bash
curl http://localhost:8080/api/v1/symbols
```

```json
[
  {
    "symbol_id": "sym_...",
    "symbol_fp": "fp_...",
    "qname": "payments.charge_card",
    "language": "python",
    "kind": "function",
    "file": "payments.py",
    "start": { "line": 26, "col": 1 },
    "end":   { "line": 32, "col": 49 },
    "signature": "def charge_card(user_id: str, amount: float)"
  }
]
```

---

## `GET /api/v1/symbols/:qname`

Fetch a symbol's detail — the symbol, its effect declaration, and up to 20
recent ledger entries.

`:qname` may contain dots (e.g. `payments.charge_card`) — URL-encode any
characters your client requires.

**Response:**

```json
{
  "symbol": { "qname": "payments.charge_card", ... },
  "effects": {
    "declared": [ { "effect": "io.db.write", ... } ],
    "transitive": [],
    "verification": { "by": "static-checker", "status": "unverified" }
  },
  "ledger": [
    { "entry_id": "led_...", "kind": "hazard", "summary": "...", ... }
  ]
}
```

**404** when the qname does not resolve to an indexed symbol.

```bash
curl http://localhost:8080/api/v1/symbols/payments.charge_card
```

---

## `GET /api/v1/symbols/:qname/ledger`

Full ledger for a symbol (all entries, not truncated).

**Response:** array of `LedgerEntry`.

```bash
curl http://localhost:8080/api/v1/symbols/payments.charge_card/ledger
```

```json
[
  { "entry_id": "led_f5e4...", "kind": "hazard", "matched_policy": "/policies/code/hazard-requires-human@1",
    "tags": ["approver:human", "approved", "approved-by:alice@example.com", "approved-at:..."], ... }
]
```

---

## `GET /api/v1/symbols/:qname/effects`

Effect declaration for a symbol.

**Response:** `EffectDecl` or `null`.

```bash
curl http://localhost:8080/api/v1/symbols/driver.main/effects
```

```json
{
  "symbol_id": "sym_...",
  "declared": [],
  "transitive": [
    { "effect": "io.db.write", "via": ["sym_payments_charge_card..."] }
  ],
  "verification": { "by": "static-checker", "status": "unverified", "mismatches": [] }
}
```

---

## `GET /api/v1/symbols/:qname/callers`

Symbols with an inbound call edge to the target.

**Response:** array of `Symbol`, sorted by qname.

```bash
curl http://localhost:8080/api/v1/symbols/payments.charge_card/callers
```

```json
[ { "qname": "driver.main", ... } ]
```

---

## `GET /api/v1/symbols/:qname/callees`

Symbols with an outbound call edge from the target.

**Response:** array of `Symbol`.

```bash
curl http://localhost:8080/api/v1/symbols/driver.main/callees
```

```json
[ { "qname": "payments.charge_card", ... }, { "qname": "payments.get_balance", ... } ]
```

---

## `GET /api/v1/ledger`

Flat cross-symbol ledger listing.

**Query:**

- `tag=<name>` — optional; filter to entries carrying that tag (e.g.
  `awaiting-approval`).

**Response:** array of `LedgerEntry`, newest first.

```bash
curl 'http://localhost:8080/api/v1/ledger?tag=awaiting-approval'
```

```json
[
  { "entry_id": "led_f5e4...", "kind": "hazard",
    "tags": ["approver:human", "awaiting-approval"], ... }
]
```

---

## `POST /api/v1/approvals/:entry_id/approve`

Approve an awaiting-approval entry.

**Body:**

```json
{
  "approver": "alice@example.com",
  "approver_kind": "human",
  "message": "verified boundary in test_payments.py",
  "agent_id": "asd-http"
}
```

- `approver` — required. Recorded as `approved-by:<id>`.
- `approver_kind` — optional; default `"human"`. Must match an `approver:*`
  tag on the entry unless `approver` matches directly.
- `message` — optional; appended to entry body as "Approver note".
- `agent_id` — optional; default `"asd-http"`. Recorded as the commit agent.

**Response:** `{ status, entry }`. `status`: `approved` or `already-approved`.

```bash
curl -X POST http://localhost:8080/api/v1/approvals/led_f5e4.../approve \
  -H 'content-type: application/json' \
  -d '{"approver":"alice@example.com","approver_kind":"human","message":"verified"}'
```

```json
{
  "status": "approved",
  "entry": {
    "entry_id": "led_f5e4...",
    "tags": ["approver:human", "approved", "approved-by:alice@example.com", "approved-at:..."],
    ...
  }
}
```

---

## `POST /api/v1/approvals/:entry_id/reject`

Reject an awaiting-approval entry.

**Body:**

```json
{
  "reviewer": "alice@example.com",
  "reviewer_kind": "human",
  "reason": "boundary isn't enforced at claimed line",
  "agent_id": "asd-http"
}
```

- `reviewer`, `reason` — required.
- `reviewer_kind` — optional; default `"human"`.
- `agent_id` — optional; default `"asd-http"`.

**Response:** `{ status, entry }`. `status`: `rejected` or `already-rejected`.

```bash
curl -X POST http://localhost:8080/api/v1/approvals/led_f5e4.../reject \
  -H 'content-type: application/json' \
  -d '{"reviewer":"alice","reason":"claim is wrong"}'
```

---

## `POST /api/v1/approvals/:entry_id/withdraw`

Original author retracts an awaiting-approval entry.

**Body:**

```json
{
  "author_id": "review-bot",
  "agent_id": "asd-http"
}
```

- `author_id` — required. Must match `entry.author.id`.
- `agent_id` — optional; default `"asd-http"`.

**Response:** `{ status, entry }`. `status`: `withdrawn` or
`already-withdrawn`.

```bash
curl -X POST http://localhost:8080/api/v1/approvals/led_f5e4.../withdraw \
  -H 'content-type: application/json' \
  -d '{"author_id":"review-bot"}'
```

---

## Error responses

All errors are JSON:

```json
{ "error": "symbol not found: payments.unknown" }
```

Status codes:

- `404` — symbol or entry not found.
- `500` — internal (storage error, policy evaluation failure).

Each mutation route emits exactly one audit event regardless of outcome —
success, already-resolved, or error — when the audit sink is configured via
`ASD_AUDIT_LOG`.

---

## What's not in the HTTP API

- No `ledger_append` endpoint. Writes go through MCP or CLI today; HTTP is
  read + ratification-only.
- No `effect_declare` endpoint. Same reason.
- No `index` / `trace` triggers. Those are operator actions driven by CLI.
- No `/audit` tail endpoint. Read the JSONL file directly or `asd audit tail`.
