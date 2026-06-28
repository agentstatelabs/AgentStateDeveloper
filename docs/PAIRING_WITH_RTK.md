# Pairing ASD with rtk (and other output compressors)

ASD and [rtk](https://github.com/rtk-ai/rtk) ("Rust Token Killer") attack agent
token cost from **different layers of the stack**, so they compose rather than
compete. This is the same "token-saving stack" pattern people already run as
[rtk + Serena](https://www.rushis.com/rtk-kills-the-token-waste-hiding-in-every-ai-coding-session/)
or rtk + TokenSave: a code-intelligence/index layer underneath an
output-compression layer. ASD fills the index/change-context slot (the
Serena/TokenSave role); rtk compresses everything that still goes through the
shell.

## Two different token sinks

| Layer | What burns tokens | Tool | How it saves |
|---|---|---|---|
| **Exploration** | grep-ing and reading whole files to understand code before editing | **ASD** | Returns structured, compact answers — symbols, callers/callees, effects, invariants, blast radius, routes, an architecture overview — instead of raw file dumps |
| **Command output** | verbose stdout from `cargo`/`npm`/`pytest`, build, lint, `git`, docker | **rtk** | Filters/compresses command output *before* it reaches the agent's context |

ASD never sees your shell command output; rtk never indexes your code. Neither
duplicates the other's work.

## Why the index layer matters most

The biggest, least-visible token sink is **exploration**: an agent grep-ing for
a symbol, then reading several whole files to find and understand it. ASD
replaces that with one structured call:

- `asd search` / `asd context-for <qname>` — signature, callers/callees,
  effects, invariants, hazards, ledger — without reading the files.
- `asd prepare-change` — the files you'll likely edit, design invariants,
  affected tests, blast radius, for a planned change.
- `asd impact` / `asd since` — blast radius before editing or for PR review.
- `asd architecture` — a one-call orient-me (languages, packages, layers,
  routes, hotspots, call-graph clusters).

ASD's `scorecard` reports an internal estimate of this structural saving
(`token_economy`): on ASD's own repo, the index represents the codebase in
roughly a tenth of the tokens it would take to read the source.

rtk then compresses the shell output ASD doesn't touch.

## How to run both

1. **Install ASD's MCP server** into your agent tools:
   ```
   asd mcp install        # auto-detects claude-code, cursor, gemini-cli,
                          # windsurf, zed, vscode, cline, codex-cli, …
   ```
   Index your repo with `asd index` (and keep it fresh via the git hooks
   `asd init` installs).

2. **Install rtk** per its own docs (hook-based for Claude Code / Cursor /
   Copilot / Gemini, plugin-based for others). rtk transparently rewrites
   shell commands so their output is compressed before the agent sees it.

That's it — they don't share config or state, so there's nothing to reconcile.

## One overlap to know about: test output

ASD ships its own failures-only test compaction, `asd test-summary`:

```
cargo test 2>&1 | asd test-summary        # "✓ 12 passed, 0 failed" or just the failures
```

This overlaps rtk's test-output filtering. Pick one for test runs — `asd
test-summary` if you want ASD-native parsing (cargo/pytest precise, others via a
generic scan); rtk if you prefer its broader runner coverage as part of one
compression layer. rtk still adds value for everything else (build, lint, git,
docker, cloud CLIs) that ASD has no opinion about.

## Rule of thumb

- **Understanding code → ASD.** (exploration, change planning, impact, review)
- **Reading command output → rtk.** (build/test/lint/git/infra)
- **Test output → either**, not both.

The two together cover both halves of an agent's token budget.
