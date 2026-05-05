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

### MCP server registration

`asd mcp install` writes the `asd-mcp` entry into the `mcpServers` block of
every agent tool config it finds:

```bash
asd mcp install           # all detected tools
asd mcp install --tool cursor          # single tool
asd mcp install --db /abs/path/to.db  # explicit db path
asd mcp status            # show ✓/✗ per tool
asd mcp uninstall         # remove from all tools
```

Detected tools and their config paths:

| Tool | Config path |
|---|---|
| Claude Code | `~/.claude.json` |
| Claude Desktop | `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) |
| Cursor | `~/.cursor/mcp.json` |

The installer sets `ASD_DB` in the `env` block so `asd-mcp` connects to the
correct project database regardless of the working directory the tool uses to
spawn the server. After installation, restart the agent tool to activate.

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

## Git-native sidecar

ASD's live state lives in a local SQLite database (`.asd-state.db`,
gitignored). The sidecar mirrors the human-authored subset of that state
— ledger entries, declared effects, symbol records — into `.asd/v1/` so
it travels with `git commit`.

```
.asd/
  v1/             ← checked in; travels with the repo
    effects/      one JSON file per symbol's declared effects
    ledger/       one JSON file per ledger entry, grouped by symbol_id
    symbols/      one JSON file per indexed symbol
    meta/         schema-version stamp
  hooks/          ← checked in; git hook scripts
    pre-commit    runs `asd sync --prune` before every commit
    post-merge    runs `asd hydrate && asd index .` after pull/merge
    post-checkout runs `asd hydrate && asd index .` after branch switch
.asd-state.db     ← gitignored; local SQLite, rebuilt from sidecar
```

`asd init` writes the hook scripts, sets `git config core.hooksPath
.asd/hooks`, and updates `.gitignore`. After that, **the sidecar stays
in sync automatically** — contributors never have to think about it.

A fresh contributor workflow:

```bash
git clone <repo>
asd init          # installs hooks, updates .gitignore
asd hydrate       # loads .asd/v1/ → local SQLite
asd index .       # rebuilds derived index
```

The result: everyone who clones gets the full decision history, effect
graph, and call graph that was built up before them — no extra server,
no onboarding step.

## Why a single workspace

The CLI, MCP server, HTTP server, tracer, and SvelteKit reviewer all share
one schema, one policy model, and one audit event shape. Enterprise
deployments keep this boundary — a Postgres-backed ASG substitutes for
SQLite, a real `agentstategraph-policy` crate substitutes for `FilePolicyGate`,
but the consumer-facing APIs (MCP tool names, HTTP routes, CLI flags) don't
change.
