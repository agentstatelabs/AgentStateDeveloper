---
title: Quick Start
description: Install, initialize, index a project, register the MCP server, and append your first ledger entry.
---

This walkthrough takes you from a fresh install to a working ledger entry and
a registered MCP server. ASD supports nine languages out of the box (Python,
TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift) — point `asd index` at
any directory and it picks the right adapter automatically.

## 1. Install

Build and install the binaries from source:

```bash
git clone https://github.com/agentstatelabs/AgentStateDeveloper.git
cd AgentStateDeveloper
cargo install --path crates/agentstatedeveloper-cli   # installs asd
cargo install --path crates/agentstatedeveloper-mcp   # installs asd-mcp + asd-serve
```

> **Note:** the name `asd` on crates.io is taken by an unrelated diff tool.
> `cargo install asd` installs the wrong thing — always install from source.

Verify:

```bash
asd --version    # asd 0.5.5
asd-mcp --help   # starts the stdio MCP server
```

## 2. Initialize a repository

Change into the project you want to track, then run:

```bash
cd my-project
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
```

`asd init` creates `.asd-state.db`, updates `.gitignore`, and installs git
hook scripts that keep the sidecar in sync automatically. Pass `--no-hooks`
to skip hook installation.

## 3. Index your code

```bash
asd index .
```

```
Indexing 42 files under . …
Done. 187 symbols, 187 effects. (12 files skipped — run with -v to list)
{
  "files": 42,
  "skipped": 12,
  "symbols": 187,
  ...
}
```

Every recognized source file is parsed. Unrecognized file types (`.yaml`,
`.json`, `.md`, etc.) are silently skipped in standard mode. Add `--verbose`
to see each file as it is processed and list every skipped file:

```bash
asd index . --verbose
```

```
Indexing 42 files under . …
  [ 1/42] src/payments.py
  [ 2/42] src/auth.py
  ...
  12 files skipped (no adapter):
  [skip] config/settings.yaml
  [skip] README.md
  ...
Done. 187 symbols, 187 effects.
```

## 4. Read a symbol

```bash
asd read payments.charge_card
```

```json
{
  "symbol": {
    "qname": "payments.charge_card",
    "language": "python",
    "kind": "function",
    "file": "payments.py"
  },
  "effects": {
    "declared": [
      { "effect": "log",         "note": "log.info(\"charging user...\")" },
      { "effect": "io.db.write", "note": "db.execute(\"INSERT INTO charges...\")" }
    ],
    "verification": { "by": "static-checker", "status": "unverified" }
  },
  "ledger": []
}
```

`asd read` is the primary agent entry point — symbol, declared and transitive
effects, and recent ledger entries in one JSON object.

## 5. Append a ledger entry

```bash
asd ledger append payments.charge_card \
  --kind hazard \
  --summary "rejects amounts over 10000 — silent failure if caller ignores exception" \
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

## 6. Load a policy

The bundled `examples/policies.json` requires human approval for hazard
entries:

```bash
asd --policy examples/policies.json \
    ledger append payments.charge_card \
    --kind hazard \
    --summary "boundary is 10000, not documented in signature" \
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

## 7. Register the MCP server

Wire `asd-mcp` into your agent tools so agents can call ASD directly:

```bash
asd mcp install
```

```
  claude-code    installed  /Users/user/.claude.json
  claude-desktop installed  /Users/user/Library/Application Support/Claude/claude_desktop_config.json
  cursor         installed  /Users/user/.cursor/mcp.json

  asd-mcp binary:  /Users/user/.cargo/bin/asd-mcp
  ASD_DB:          /path/to/my-project/.asd-state.db

Restart your agent tool(s) to activate the MCP server.
```

`asd mcp install` detects all known tool configs on the machine and writes the
`asd-mcp` entry with the correct `ASD_DB` path. Restart the tool to pick it up.

```bash
asd mcp status     # check registration status
asd mcp uninstall  # remove from all tools
asd mcp install --tool cursor  # target a single tool
```

## 8. Share your ASD context via git

```bash
asd sync --prune
git add .asd/v1/
git commit -m "chore: sync ASD sidecar"
```

Teammates who clone the repo run:

```bash
asd init        # installs hooks, updates .gitignore
asd hydrate     # loads .asd/v1/ into local db
asd index .     # rebuilds derived semantic index
asd mcp install # registers asd-mcp with their agent tools
```

After `asd init`, the pre-commit hook runs `asd sync --prune` automatically
on every commit — no manual steps required.

## Next

- [Core Concepts](/guides/concepts) — the seven primitives in detail.
- [Architecture](/guides/architecture) — crate layout and storage model.
- [CLI reference](/reference/cli) — every subcommand, flag, and env var.
- [MCP tools](/reference/mcp-tools) — the 14+ tool surface for agents.
