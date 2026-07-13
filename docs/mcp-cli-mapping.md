# MCP ↔ CLI tool-name mapping

Two surfaces, two naming conventions:

- **MCP** (Model Context Protocol) tools live in a **flat namespace** shared
  with every other MCP server an agent has loaded. Names like `code_search`,
  `ledger_append`, `scratch_write` use a `code_` / `ledger_` / `scratch_`
  prefix so they don't collide with `search`, `append`, `write` from other
  servers.
- **CLI** subcommands live under `asd` with **hierarchical** nesting:
  `asd search`, `asd ledger append`, `asd scratch write`. The hierarchy
  matches `--help` output and human shell habits.

These two conventions cannot collapse to one without breaking either MCP
namespace hygiene or CLI ergonomics. This document is the canonical map.

## Direct 1:1 names (same name on both surfaces)

| MCP tool | CLI command |
|---|---|
| `annotate_commit` | `asd annotate-commit` |
| `architecture` | `asd architecture` |
| `callers` | `asd callers` |
| `callees` | `asd callees` |
| `checklist` | `asd checklist` |
| `context_for` | `asd context-for` |
| `dead_code` | `asd dead-code` |
| `effect_declare` | (MCP-only — no CLI verb) |
| `effects` | (MCP-only — no CLI verb) |
| `endpoints` | `asd endpoints` |
| `health` | (MCP-only — HTTP healthcheck) |
| `impact` | `asd impact` |
| `investigate` | `asd investigate` |
| `map` | `asd map` |
| `prepare_change` | `asd prepare-change` |
| `recipe_classify_test_migration` | `asd recipe classify-test-migration` |
| `recipe_migrate_stale_tests` | `asd recipe migrate-stale-tests` |
| `references` | `asd references` |
| `reindex` | (MCP-only — library call) |
| `scopes_list` | `asd scopes list` |
| `scorecard` | `asd scorecard` |
| `search` | `asd search` |
| `since` | `asd since` |
| `status` | `asd status` |
| `sync` | `asd sync` |
| `task_close` | `asd task-close` |
| `test_summary` | `asd test-summary` |
| `traces` | (MCP-only — execution traces) |
| `trust` | `asd trust` |
| `verify_effects` | `asd verify-effects` |

## `code_*` prefix on MCP (namespace collision avoidance)

| MCP tool | CLI command | Why prefix |
|---|---|---|
| `code_search` | `asd search` | "search" collides with other MCP servers |
| `code_read` | `asd read` | "read" collides with file-read tools |
| `code_query` | `asd search` (via filters) | "query" too generic for the flat namespace |

The CLI also accepts the MCP-era names as aliases (Plan D t-003), so
`asd code_search foo` and `asd search foo` both work.

## MCP flattens what the CLI nests

These MCP tools collapse a CLI subcommand-of-subcommand into one
underscore-joined name because MCP has no nested-command concept.

| MCP tool | CLI command |
|---|---|
| `audit_tail` | `asd audit tail` |
| `audit_verify` | `asd audit verify` |
| `conclusions_list` | `asd conclusions list` |
| `conclusions_export` | `asd conclusions export` |
| `conclusions_import` | `asd conclusions import` |
| `feedback_list` | `asd feedback list` |
| `feedback_mark` | `asd feedback mark` |
| `feedback_promote` | `asd feedback promote-as-truth` |
| `invariant_add` | `asd invariant add` |
| `think_speculate` | `asd think speculate` |
| `think_model` | `asd think model` |
| `think_failed` | `asd think failed` |
| `think_question` | `asd think question` |
| `think_list` | `asd think list` |
| `invariant_list` | `asd invariant list` |
| `ledger_append` | `asd ledger append` |
| `ledger_approve` | `asd ledger approve` |
| `ledger_find` | _(MCP-only — no `asd ledger find`; on the CLI reads go through `asd list ledger` / `asd conclusions list`)_ |
| `ledger_get` | _(MCP-only — no `asd ledger get`; on the CLI read a symbol's ledger via `asd read <sym>`)_ |
| `ledger_rebind` | `asd ledger rebind` |
| `ledger_reject` | `asd ledger reject` |
| `ledger_supersede` | `asd ledger supersede` |
| `ledger_withdraw` | `asd ledger withdraw` |
| `scratch_clean` | `asd scratch clean` |
| `scratch_discard` | `asd scratch discard` |
| `scratch_list` | `asd scratch list` |
| `scratch_promote` | `asd scratch promote` |
| `scratch_read` | `asd scratch read` |
| `scratch_update` | `asd scratch update` |
| `scratch_write` | `asd scratch write` |

## Conventions that always hold

1. **No `_of` suffix** anywhere — `callers_of` / `callees_of` / `effects_of`
   / `traces_of` were retired in Plan A t-002. Use the bare form on both
   surfaces.
2. **MCP names always use `snake_case`** (e.g. `task_close`, `conclusions_export`).
3. **CLI names always use `kebab-case`** at the top level (`task-close`,
   `prepare-change`) and `lower_snake` for subcommands (`ledger append`,
   `conclusions export`).
4. **`code_` prefix** is reserved for read-side tools that would collide
   with other MCP servers (`code_search`, `code_read`, `code_query`). New
   write-side tools that need disambiguation should use a verb prefix
   (`recipe_classify_test_migration`), not `code_`.

## Source-of-truth tests

- `crates/agentstatedeveloper-mcp/src/mcp_server.rs::tool_name_regression`
  — locks the renames + asserts the `_of` suffix never returns + verifies
  every tool above is registered.
- The audit comment at the top of the `#[tool_router]` impl in
  `mcp_server.rs` (line ~668) restates these conventions for future editors.
