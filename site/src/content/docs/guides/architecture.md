---
title: Architecture
description: Three-tier storage model, the crate layout, and how CLI / MCP / HTTP / Lens share a single core.
---

ASD is a Rust workspace with a clear split: a language-agnostic core, nine
language adapters, and three binary surfaces that wrap the core with different
transports.

## Three-tier storage

ASD persists state across three tiers, deliberately:

**In git** (travels with the code, survives cold clone):

- Source code
- `.asd/v1/effects/<qname>.json` — declared effects per symbol
- `.asd/v1/ledger/<qname>/<entry_id>.json` — non-superseded ledger entries
- `.asd/v1/meta/schema-version`

**In ASG** (local SQLite; optionally backed by a registry for cross-machine
authoring history):

- Live authoring state and supersede chains
- Per-edit intent, confidence, authority
- Raw traces
- Transitive effect caches
- Call edges

**Never persisted** (rebuilt on demand):

- Semantic index (symbols, qname → symbol_id map) — rebuilt by `asd index`
- Effect verification results — rerun on demand

A fresh `git clone` + `asd init` + `asd hydrate` + `asd index` reconstructs
everything the repository needs to work without any network call.

## Crate layout

```
crates/
  agentstatedeveloper-core/       traits, schema, ASG-backed stores, policy, audit
  agentstatedeveloper-adapters/   default_adapters() — registers all language adapters
  agentstatedeveloper-python/     Python adapter (tree-sitter)
  agentstatedeveloper-typescript/ TypeScript/JavaScript adapter (tree-sitter)
  agentstatedeveloper-rust/       Rust adapter (tree-sitter)
  agentstatedeveloper-go/         Go adapter (tree-sitter)
  agentstatedeveloper-java/       Java adapter (tree-sitter)
  agentstatedeveloper-csharp/     C# adapter (tree-sitter)
  agentstatedeveloper-ruby/       Ruby adapter (tree-sitter)
  agentstatedeveloper-kotlin/     Kotlin adapter (tree-sitter)
  agentstatedeveloper-swift/      Swift adapter (tree-sitter)
  agentstatedeveloper-cli/        `asd` binary (clap)
  agentstatedeveloper-mcp/        `asd-mcp` stdio server + `asd-serve` HTTP server
web/                              Lens SvelteKit review UI
tools/asd_tracer.py               Python runtime tracer
examples/sample-py-repo/          working Python example
examples/policies.json            working policy example
```

### core

`agentstatedeveloper-core` defines the trait surface (`LanguageAdapter`,
`IndexStore`, `EffectStore`, `LedgerStore`, `PolicyGate`, `AuditSink`) and
ships ASG-backed default implementations. It is language-agnostic and wraps
`agentstategraph` (the substrate) rather than talking to SQLite directly —
this lets ASD swap to a Postgres-backed ASG at the enterprise tier without
changing the core code.

Key modules:

- `schema.rs` — `Symbol`, `LedgerEntry`, `EffectDecl`, `Effect`, `Verification`.
- `effects.rs`, `ledger.rs`, `index.rs` — per-concept ASG-backed stores.
- `policy.rs` — `PolicyGate` trait, `FilePolicyGate`, `PermissivePolicyGate`,
  canonical `actions::*` constants.
- `audit.rs` — `AuditEvent`, `AuditSink`, `JsonlFileSink`, `NullSink`,
  `event_types::*` constants.
- `sidecar.rs` — `sync_to_dir` / `hydrate_from_dir` for the `.asd/v1/` roundtrip.
- `transitive.rs` — cycle-safe DFS propagator.
- `paths.rs` — canonical ASG path constants (`ASD_PATH_PREFIX = "/asd/v1"`).

### Language adapters

Each language crate implements `LanguageAdapter` — three methods:
`parse_symbols`, `infer_effects`, `extract_call_edges`. All adapters are
registered at startup via `default_adapters()` in `agentstatedeveloper-adapters`.
A file's extension selects the adapter automatically:

| Extension | Adapter |
|---|---|
| `.py` | Python |
| `.ts` `.tsx` `.mts` `.cts` `.js` `.jsx` | TypeScript |
| `.rs` | Rust |
| `.go` | Go |
| `.java` | Java |
| `.cs` | C# |
| `.rb` | Ruby |
| `.kt` `.kts` | Kotlin |
| `.swift` | Swift |

See [the Python guide](/guides/python) and [the TypeScript guide](/guides/typescript)
for detailed adapter documentation.

### Surfaces

Three transports, one core:

```
                     ┌──────────────────────────────────────┐
                     │       agentstatedeveloper-core        │
                     │  (Engine, stores, policy, audit)      │
                     └─────────────┬────────────────────────┘
                                   │
         ┌──────────────┬──────────┼──────────┬──────────────┐
         ▼              ▼          ▼          ▼              ▼
   asd (clap CLI)  asd-mcp    asd-serve  .asd/ sidecar   tools/asd_tracer.py
                   (rmcp       (axum       (sync /        (python sys.settrace)
                    stdio)      HTTP +      hydrate)
                                ServeDir)
                                   │
                                   │ static-serve
                                   ▼
                              web/ (SvelteKit Lens)
```

All three surfaces share the same `Engine`, the same `PolicyGate`, and the
same `AuditSink`. A policy deny surfaces identically across CLI / MCP / HTTP;
a ledger append through MCP appears in the same audit log as one through the
CLI. That consistency is deliberate — security and compliance guarantees
rely on it.

### Path convention

ASD namespaces every node it writes under `/asd/v1/`. Layout:

```
/asd/v1/
  code/<lang>/<canonical-path>/<symbol-fp>   source blob per symbol
  index/
    by-qname/<qname>                          → symbol-id lookup
    callers/<symbol-id>                       inbound edges
    callees/<symbol-id>                       outbound edges
  ledger/<symbol-id>/<entry-id>               ledger entries
  effects/<symbol-id>                         declared + verification block
  traces/<symbol-id>/<trace-id>               runtime trace records
  meta/schema-version                          schema version stamp
```

The `v1` prefix means a later version can sit alongside without corruption.
Every ASG path is stable, which is what makes `.asd/` sidecar sync a direct
file-per-node mirror.

## Why a single workspace

The CLI, MCP server, HTTP server, tracer, and SvelteKit reviewer all share
one schema, one policy model, and one audit event shape. Enterprise
deployments keep this boundary — a Postgres-backed ASG substitutes for
SQLite, a real `agentstategraph-policy` crate substitutes for `FilePolicyGate`,
but the consumer-facing APIs (MCP tool names, HTTP routes, CLI flags) don't
change.
