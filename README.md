# AgentStateDeveloper

Code-level context and audit overlay for agent-authored code.

ASD gives every function a decision ledger, an effect declaration, and a
call graph — all queryable by the coding agents that write the code, and
all checked into git so they travel with every clone.

## Install

### macOS / Linux — Homebrew (recommended)

```bash
brew tap agentstatelabs/agentstatedeveloper
brew trust agentstatelabs/agentstatedeveloper   # one-time, third-party-tap trust
brew install asd
```

Installs `asd`, `asd-mcp`, and `asd-serve`. Upgrades via `brew upgrade asd`.

> Recent Homebrew requires explicit `brew trust` for third-party taps.
> If you skip the trust step you'll see `Warning: Skipping … because it
> is not trusted` on `brew update` — the install will still succeed
> until then, but updates are silent no-ops.

### macOS / Linux — one-liner

```bash
curl -fsSL https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/install.sh | sh
```

Drops the three binaries in `~/.local/bin`. Optional overrides:
`ASD_VERSION=v1.1.19`, `INSTALL_DIR=/usr/local/bin`.

### Windows — PowerShell

```powershell
iwr https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/install.ps1 | iex
```

Installs `asd.exe`, `asd-mcp.exe`, and `asd-serve.exe` to
`%LOCALAPPDATA%\asd\bin` and adds it to your user `PATH`. Open a fresh
PowerShell after the install so the new PATH takes effect.

> Windows binaries are built for `x86_64-pc-windows-msvc`. ARM64 Windows
> isn't a release target yet — the x86_64 binaries run under Windows
> ARM emulation in the interim.

### From source (Rust toolchain required)

```bash
cargo install --path crates/agentstatedeveloper-cli   # installs asd
cargo install --path crates/agentstatedeveloper-mcp   # installs asd-mcp + asd-serve
```

> **Note:** the crate name `asd` on crates.io is taken by an unrelated diff tool.
> Install from source using the commands above.

### Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/uninstall.sh | sh
# or:
brew uninstall asd
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
| **Git-native sidecar** | The committed, compact subset of ledger entries (decisions, hazards, recipes, mappings, classifications, follow-ups, agent thinking) lives in `.asd/conclusions/*.jsonl` — checked into git, travels with every clone. Kilobytes, not megabytes. |

## Quick start

```bash
# Clone and build
git clone https://github.com/agentstatelabs/AgentStateDeveloper.git
cd AgentStateDeveloper
cargo install --path crates/agentstatedeveloper-cli
cargo install --path crates/agentstatedeveloper-mcp

# Initialize your project
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

# Register the MCP server with your agent tools
asd mcp install

# Export the committed sidecar and commit
asd conclusions export
git add .asd/conclusions/
git commit -m "chore: sync ASD sidecar"
```

(The `asd init` pre-commit hook runs `asd conclusions export` automatically;
the explicit two-step above is just to illustrate what's happening.)

### MCP ↔ CLI naming reference

ASD has two surfaces with two naming conventions: MCP uses a flat
namespace (`ledger_append`, `code_search`) so tools don't collide with
other MCP servers; the CLI nests (`asd ledger append`, `asd search`).
The canonical mapping lives in [docs/mcp-cli-mapping.md](docs/mcp-cli-mapping.md).
Both forms work on the CLI — agents trained on older MCP-era docs can
type either `asd ledger append` or (via clap aliases from Plan D t-003)
the equivalent `asd code_search` / `asd callers_of` etc.

### Brief output mode for agents

`asd` defaults to a verbose JSON shape that's helpful for humans and
structured-parsing pipelines but spends tokens an agent rarely needs.
Set `ASD_FORMAT=brief` (or pass `--brief` per-invocation) to project
`read` / `callers` / `callees` responses down to load-bearing fields
only (qname, file:line, signature, first doc line). Typical reduction:
60–80% on those commands.

Recommended once at process start for any agent that drives `asd`:

```bash
export ASD_FORMAT=brief
```

Applies to CLI and MCP. The spawned `asd-mcp` server inherits
`ASD_FORMAT=brief` from its parent process at startup and projects the
three highest-volume read tools (`code_read`, `code_search`,
`references`) through the same compact shape.

## MCP server setup

`asd-mcp` is the stdio MCP server that coding agents use to query ASD.
Register it in all detected tools with one command:

```bash
asd mcp install
```

This writes the `asd-mcp` entry into `mcpServers` in every config file it
finds (Claude Code, Claude Desktop, Cursor). Restart the tool to activate.

```bash
asd mcp status    # show registration status across all tools
asd mcp install --tool cursor          # install into one specific tool
asd mcp install --db /abs/path/to/db  # use a non-default db path
asd mcp uninstall                      # remove from all tools
```

The MCP server reads `ASD_DB` (set by `install` in the env block) so agents
always connect to the right project database.

## Git-native sidecar

ASD has two on-disk locations and one in-SQLite namespace. Knowing which
is which avoids surprise:

| Location | What's in it | Tracked in git? | Authoritative for? |
|----------|--------------|-----------------|---------------------|
| `.asd-state.db` | Live SQLite ASG (index, call graph, FTS, full ledger, traces) | **No** (gitignored) | Everything at runtime |
| `.asd/conclusions/*.jsonl` | Compact subset: decisions, classifications, mappings, hazards, recipes, follow-ups, agent thinking | **Yes** | What a fresh clone needs to inherit |
| `.asd/v1/` (legacy) | Older verbose mirror — superseded by `.asd/conclusions/` | **No** (gitignored) | Vestigial; `asd sync`/`asd hydrate` still write/read it for local debug. Not on the commit path. |

The principle: **the committed sidecar carries judgment** (decisions
the agent or human had to make). **Everything else is regenerable**
from source via `asd index .`, so it's gitignored.

**One-time setup:**

```bash
asd init
```

```
initialized at ./.asd-state.db
.gitignore: updated (.asd-state.db and .asd/v1/ ignored — both are local derived state)

ASD git hooks installed (.asd/hooks/):

  pre-commit    trigger:  git commit
                command:  asd conclusions export
                purpose:  write committed conclusions (decisions/hazards/recipes/…) to .asd/conclusions/*.jsonl

  post-merge    trigger:  git merge / git pull
                command:  asd conclusions import && asd index .
                purpose:  import committed .asd/conclusions/ into local ledger and rebuild index

  post-checkout trigger:  git checkout / git switch
                command:  asd conclusions import && asd index .
                purpose:  sync local db to the checked-out branch's sidecar state

  core.hooksPath → .asd/hooks  (hooks are now active)

  To skip hook installation: asd init --no-hooks
  To review hooks later:     asd hooks
```

After `asd init`, the pre-commit hook runs `asd conclusions export`
automatically on every commit — no manual steps.

**Onboarding after clone:**

```bash
git clone <repo>
asd init                  # installs hooks, updates .gitignore
asd conclusions import    # loads .asd/conclusions/*.jsonl → local ledger
asd index .               # rebuilds derived semantic index from source
asd mcp install           # registers asd-mcp with your agent tools
```

## Indexing

```bash
asd index .            # index current directory
asd index . --verbose  # show each file as it is processed, list skipped files
```

Standard output:
```
Indexing 42 files under . …
Done. 187 symbols, 187 effects. (12 files skipped — run with -v to list)
```

Unrecognized file types (`.yaml`, `.json`, `.md`, etc.) are silently skipped
in standard mode and listed with `[skip]` in `--verbose` mode. The `skipped`
count is always included in the JSON summary.

## Surfaces

- **`asd`** — CLI: `init`, `index`, `read`, `ledger`, `policy`, `verify-effects`, `trace`, `sync`, `hydrate`, `audit`, `hooks`, `mcp`
- **`asd-mcp`** — stdio MCP server exposing 14+ tools to coding agents
- **`asd-serve`** — HTTP server + Lens review UI

## MCP tools

Agents access ASD through 14+ MCP tools: `health`, `code_query`, `code_read`,
`effects_of`, `callers_of`, `callees_of`, `ledger_get`, `ledger_find`,
`ledger_append`, `ledger_approve`, `ledger_reject`, `ledger_withdraw`,
`ledger_supersede`, `effect_declare`, `traces_of`, `reindex`,
`ledger_rebind`, `audit_tail`, `audit_verify`.

## License

BSL-1.1; converts to Apache-2.0 four years after each release.
