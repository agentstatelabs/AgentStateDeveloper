---
title: MCP Tools
description: Every tool exposed by the asd-mcp stdio server. Purpose, params, return shape, and an example JSON-RPC exchange for each.
---

`asd-mcp` is the stdio MCP server. It exposes 14 tools to coding agents over
the MCP protocol. All tools operate against a single ASG-backed `Engine`
opened at server start; write tools route through the configured
`PolicyGate` and emit to the configured `AuditSink`.

Tools are grouped below as read-only, write, and admin. Every request
follows the MCP JSON-RPC convention:

```json
{ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "<tool_name>", "arguments": { ... } } }
```

Responses are `tools/call` results containing a stringified JSON payload.
Examples below show just the `arguments` object and the inner payload for
brevity.

---

## `health`

Server status check. No params.

**Returns:** `{ status, db_path, symbol_count }`.

```json
// request arguments
{}

// response payload
{ "status": "ok", "db_path": "/home/user/project/.asd-state.db", "symbol_count": 13 }
```

---

## `code_query`

Query indexed symbols by substring / kind / language.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `name_contains` | string | — | Substring match on qname. |
| `kind` | string | — | `module` / `function` / `method` / `class` / `variable`. |
| `language` | string | — | `python` / `typescript`. |
| `limit` | number | 50 | Max results. |

Filters are AND-combined.

**Returns:** array of `Symbol` records, sorted by qname.

```json
// request
{ "name_contains": "charge", "kind": "function", "limit": 20 }

// response
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

## `code_read`

Full symbol read — the primary "give me context for this symbol" tool.
Returns the symbol, its effect declaration (declared + transitive +
verification), and all ledger entries.

**Params:** `{ qname: string }`.

**Returns:** `{ symbol, effects, ledger }`.

```json
// request
{ "qname": "payments.charge_card" }

// response
{
  "symbol": { "qname": "payments.charge_card", "kind": "function", ... },
  "effects": {
    "symbol_id": "sym_...",
    "declared": [
      { "effect": "log", "note": "log.info(...)" },
      { "effect": "io.db.write", "note": "db.execute(\"INSERT...\")" },
      { "effect": "throw", "note": "raise ValueError(...)" }
    ],
    "transitive": [],
    "verification": { "by": "static-checker", "status": "unverified", "mismatches": [] }
  },
  "ledger": []
}
```

---

## `callers_of`

Inbound call edges for a symbol.

**Params:** `{ qname: string }`.

**Returns:** array of `Symbol` records that call the target, sorted by qname.

```json
// request
{ "qname": "payments.charge_card" }

// response
[
  { "qname": "driver.main", "kind": "function", "file": "_driver.py", ... }
]
```

---

## `callees_of`

Outbound call edges for a symbol.

**Params:** `{ qname: string }`.

**Returns:** array of `Symbol` records called by the target.

```json
// request
{ "qname": "driver.main" }

// response
[
  { "qname": "payments.charge_card", ... },
  { "qname": "payments.get_balance", ... }
]
```

---

## `effects_of`

Declared + transitive effects for a symbol, with verification status.

**Params:** `{ qname: string }`.

**Returns:** the `EffectDecl` record or `null`.

```json
// request
{ "qname": "driver.main" }

// response
{
  "symbol_id": "sym_...",
  "declared": [],
  "transitive": [
    { "effect": "io.db.write", "via": ["sym_payments_charge_card..."] },
    { "effect": "log",         "via": ["sym_payments_charge_card..."] },
    { "effect": "throw",       "via": ["sym_payments_charge_card..."] }
  ],
  "verification": { "by": "static-checker", "status": "unverified", "mismatches": [] }
}
```

---

## `ledger_get`

Ledger entries for a symbol, newest first. Entries superseded by later
entries are omitted unless `include_superseded: true`.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `qname` | string | — | Symbol qname. |
| `include_superseded` | bool | false | Include entries referenced in any `supersedes` array. |

**Returns:** array of `LedgerEntry` records.

```json
// request
{ "qname": "payments.charge_card" }

// response
[
  {
    "entry_id": "led_f5e4...",
    "symbol_id": "sym_...",
    "kind": "hazard",
    "summary": "boundary at 10000 is undocumented",
    "author": { "kind": "human", "id": "alice@example.com" },
    "matched_policy": "/policies/code/hazard-requires-human@1",
    "tags": ["approver:human", "approved", "approved-by:alice@example.com", "approved-at:..."],
    "created_at": "2026-04-17T14:19:10Z"
  }
]
```

---

## `ledger_find`

Flat cross-symbol ledger search. O(n) scan — v1 simplicity.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `kind` | string | — | `decision` / `hazard` / etc. |
| `tag` | string | — | Must be present on entry. |
| `author_id` | string | — | Exact match. |
| `limit` | number | 50 | Cap. |

**Returns:** array of `LedgerEntry`, sorted newest-first.

```json
// request
{ "tag": "awaiting-approval", "limit": 10 }

// response
[
  { "entry_id": "led_f5e4...", "kind": "hazard", "tags": ["approver:human", "awaiting-approval"], ... }
]
```

---

## `ledger_append`

Write a new ledger entry attached to a symbol. Routes through the policy
gate — may deny, allow, or tag as awaiting approval.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `qname` | string | — | Target symbol. |
| `kind` | string | — | `decision` / `assumption` / `constraint` / `rationale` / `hazard` / `tradeoff`. |
| `summary` | string | — | One-line. |
| `body` | string | — | Optional freeform markdown. |
| `tags` | string[] | — | Optional extra tags. |
| `author_kind` | string | `"agent"` | `"agent"` or `"human"`. |
| `author_id` | string | `"asd-mcp"` | Caller identity. |

**Returns:** `{ entry_id, symbol_id, matched_policy, status }`.

`status` values: `allowed`, `awaiting-approval`, `denied`, `no-policy-match`.

```json
// request
{
  "qname": "payments.charge_card",
  "kind": "hazard",
  "summary": "boundary at 10000 is undocumented",
  "author_kind": "human",
  "author_id": "alice@example.com"
}

// response
{
  "entry_id": "led_f5e4...",
  "symbol_id": "sym_...",
  "matched_policy": "/policies/code/hazard-requires-human@1",
  "status": "awaiting-approval"
}
```

On policy deny, returns `{ "error": "policy denied: <reason> (matched <path>@<version>)" }`.

---

## `ledger_approve`

Approve an awaiting-approval entry.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `entry_id` | string | — | Target entry. |
| `approver` | string | — | Recorded as `approved-by:<id>`. |
| `approver_kind` | string | `"human"` | Must match an `approver:*` tag on the entry unless `approver` matches directly. |
| `message` | string | — | Optional rationale appended to body as "Approver note". |

**Returns:** `{ status, entry }`. `status`: `approved` or `already-approved`.

```json
// request
{ "entry_id": "led_f5e4...", "approver": "alice@example.com", "approver_kind": "human",
  "message": "verified boundary in test_payments.py" }

// response
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

## `ledger_reject`

Reject an awaiting-approval entry.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `entry_id` | string | — | Target entry. |
| `reviewer` | string | — | Recorded as `rejected-by:<id>`. |
| `reviewer_kind` | string | `"human"` | Same approver-match rule as approve. |
| `reason` | string | — | **Required.** Appended to entry body. |

**Returns:** `{ status, entry }`. `status`: `rejected` or `already-rejected`.

```json
// request
{ "entry_id": "led_f5e4...", "reviewer": "alice@example.com", "reviewer_kind": "human",
  "reason": "boundary not actually enforced at claimed line" }

// response
{ "status": "rejected", "entry": { "tags": ["rejected", "rejected-by:alice@example.com", ...], ... } }
```

---

## `ledger_withdraw`

Original author retracts an awaiting-approval entry.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `entry_id` | string | — | Target entry. |
| `author_id` | string | — | Must match `entry.author.id`. |

**Returns:** `{ status, entry }`. `status`: `withdrawn` or `already-withdrawn`.

```json
// request
{ "entry_id": "led_f5e4...", "author_id": "review-bot" }

// response
{ "status": "withdrawn", "entry": { "tags": ["withdrawn", ...], ... } }
```

A mismatched `author_id` returns an error event with `outcome: "unauthorized"`.

---

## `ledger_supersede`

Append a new entry that supersedes one or more prior entries on the same
symbol.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `qname` | string | — | Target symbol. |
| `supersedes` | string[] | — | Entry ids to supersede; all must belong to `qname`. |
| `kind` | string | — | Ledger kind for the new entry. |
| `summary` | string | — | One-line. |
| `body` | string | — | Optional. |
| `author_kind` | string | `"agent"` | |
| `author_id` | string | `"asd-mcp"` | |

**Returns:** `{ status, entry_id, symbol_id, supersedes }`.

```json
// request
{
  "qname": "payments.charge_card",
  "supersedes": ["led_a1b2..."],
  "kind": "decision",
  "summary": "replaces earlier hazard with concrete mitigation plan"
}

// response
{
  "status": "superseded",
  "entry_id": "led_new...",
  "symbol_id": "sym_...",
  "supersedes": ["led_a1b2..."]
}
```

---

## `effect_declare`

Overwrite the `declared` effect list for a symbol. Routes through the
policy gate. The action is `asd.effect.declare.broadens` when the new list
introduces an effect category not previously present; otherwise
`asd.effect.declare`.

**Params:**

| Name | Type | Default | Description |
|---|---|---|---|
| `qname` | string | — | Target symbol. |
| `declared` | Effect[] | — | List of Effect objects. |
| `author_id` | string | `"asd-mcp"` | For policy `agent_id` matching. |

Each `Effect` object:

```json
{ "effect": "io.fs.write", "qualifiers": { "paths": ["logs/**"] }, "note": "writes structured logs" }
```

**Returns:** `{ effect_decl, matched_policy, status, action }`.

```json
// request
{
  "qname": "payments.charge_card",
  "declared": [
    { "effect": "log" },
    { "effect": "io.db.write" },
    { "effect": "throw" }
  ]
}

// response
{
  "effect_decl": {
    "symbol_id": "sym_...",
    "declared": [ { "effect": "log" }, { "effect": "io.db.write" }, { "effect": "throw" } ],
    "transitive": [],
    "verification": { "by": "static-checker", "status": "unverified", ... },
    "matched_policy": null
  },
  "matched_policy": null,
  "status": "allowed",
  "action": "asd.effect.declare"
}
```

On broadening (introducing e.g. `io.net.out` to a symbol that didn't
previously have it), `action` switches to `asd.effect.declare.broadens` and
the policy gate may return `RequireApproval` / `Deny` on that action even
when the non-broadening path is permissive.

---

## Error shape

On error (bad params, symbol not found, policy deny, storage error), tools
return a JSON string with an `error` key:

```json
{ "error": "symbol not found: payments.unknown" }
{ "error": "policy denied: hazard entries are load-bearing (matched /policies/code/hazard-requires-human@1)" }
```

Audit events are still emitted for errors — you can observe a denied write
in `audit tail --outcome denied` even though no entry landed.
