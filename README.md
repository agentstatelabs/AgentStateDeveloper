# AgentStateDeveloper

Code-level context and audit overlay for agent-authored code.

ASD gives every function a decision ledger, an effect declaration, and a
call graph — all queryable by the coding agents that write the code, and
all checked into git so they travel with every clone.

```
cargo install asd
```

## What it does

| Primitive | What you get |
|---|---|
| **Decision ledger** | Append decisions, hazards, rationale, and constraints to any symbol. Entries survive renames. Approve, reject, or withdraw. |
| **Effect declarations** | 17 categories (`io.fs.read`, `io.net.out`, `io.db.write`, …). Declared per symbol, propagated transitively through the call graph. |
| **Semantic index** | Every function, method, and class parsed by tree-sitter. 9 languages: Python, TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift. |
| **Call graph** | Intra- and cross-module edges. Transitive effects propagate automatically. |
| **Policy gate** | File-backed JSON rules: allow, deny, or require-approval per action and actor kind. |
| **Ratification** | Approve, reject, or withdraw ledger entries. Full approval workflow. |
| **Audit event stream** | Hash-chained JSONL log of every ledger mutation and policy evaluation. |
| **Git-native sidecar** | Ledger entries and effects live in `.asd/v1/` — checked into git, travel with every clone. |

## Git-native sidecar

ASD's live state lives in a local SQLite database (`.asd-state.db`,
gitignored). The sidecar mirrors the human-authored subset into `.asd/v1/`
so it travels with `git commit`.

**One-time setup:**

```bash
asd init
```

```
initialized at ./.asd-state.db
.gitignore: updated (.asd-state.db ignored; .asd/v1/ tracked)

ASD git hooks installed (.asd/hooks/):

  pre-commit    trigger:  git commit
                command:  asd sync --prune
                purpose:  flush ledger/effects to .asd/v1/ and remove stale entries

  post-merge    trigger:  git merge / git pull
                command:  asd hydrate && asd index .
                purpose:  load new .asd/v1/ entries into local db and rebuild index

  post-checkout trigger:  git checkout / git switch
                command:  asd hydrate && asd index .
                purpose:  sync local db to the checked-out branch's sidecar state

  core.hooksPath → .asd/hooks  (hooks are now active)

  To skip hook installation: asd init --no-hooks
  To review hooks later:     asd hooks
```

After `asd init`, the pre-commit hook runs `asd sync --prune` automatically
on every commit — no manual steps.

**Onboarding after clone:**

```bash
git clone <repo>
asd init        # installs hooks, updates .gitignore
asd hydrate     # loads .asd/v1/ → local SQLite
asd index .     # rebuilds derived semantic index
```

## Quick start

```bash
# Build
git clone https://github.com/agentstatelabs/AgentStateDeveloper.git
cd AgentStateDeveloper
cargo build --release

# Index a project
cd my-project
asd init
asd index .

# Read a symbol
asd read payments.chargeCard

# Append a ledger entry
asd ledger append payments.chargeCard \
  --kind hazard \
  --summary "fails silently above 10000 — caller must check return value" \
  --author-kind human \
  --author-id alice@example.com

# Sync to sidecar and commit
asd sync --prune
git add .asd/v1/
git commit -m "chore: sync ASD sidecar"
```

## Surfaces

- **`asd`** — CLI: `init`, `index`, `read`, `ledger`, `policy`, `verify-effects`, `trace`, `sync`, `hydrate`, `audit`, `hooks`
- **`asd-mcp`** — stdio MCP server exposing 14 tools to coding agents
- **`asd-serve`** — HTTP server + Lens review UI

## MCP tools

Agents access ASD through 14 MCP tools: `health`, `code_query`, `code_read`,
`effects_of`, `callers_of`, `callees_of`, `ledger_get`, `ledger_find`,
`ledger_append`, `ledger_approve`, `ledger_reject`, `ledger_withdraw`,
`ledger_supersede`, `effect_declare`, `traces_of`, `reindex`,
`ledger_rebind`, `audit_tail`, `audit_verify`.

## License

BSL-1.1; converts to Apache-2.0 four years after each release.
