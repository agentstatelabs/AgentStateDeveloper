# ASD Features & Command Reference

A complete tour of what AgentStateDeveloper does and every command it
exposes. If you want a narrative "how do I actually use this" guide,
read [WALKTHROUGH.md](WALKTHROUGH.md) first and come back here for the
details.

ASD has three surfaces:

- **`asd`** — the CLI (everything below).
- **`asd-mcp`** — a stdio MCP server exposing a curated subset to coding
  agents (see [MCP tools](#mcp-tools)).
- **`asd-serve`** — an HTTP server + the Lens review UI.

Every read command supports `--agent` (or `ASD_FORMAT=brief` / `--brief`)
for compact, token-thrifty output. The default JSON shape is verbose and
human/pipeline friendly.

---

## The primitives

ASD is a code-context and audit overlay. Six primitives sit under all the
commands:

| Primitive | What it is |
|---|---|
| **Semantic index** | Every function, method, and class parsed by tree-sitter across **9 languages** (Python, TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift), stored in a local SQLite database (the ASG) with an FTS index for concept search. |
| **Call graph** | Intra- and cross-module caller/callee edges, walked transitively by `callers`, `callees`, `impact`, and friends. |
| **Decision ledger** | Append-only judgment attached to any symbol: decisions, hazards, invariants, rationale, constraints, proofs. Entries survive renames (rebind) and carry an approval workflow. |
| **Effect declarations** | 17 effect categories (`io.fs.read`, `io.net.out`, `io.db.write`, …) declared per symbol and propagated transitively through the call graph. |
| **Policy + audit** | A file-backed JSON policy gate (allow / deny / require-approval per action and actor kind) and a hash-chained JSONL audit log of every mutation and evaluation. |
| **Git-native sidecar** | The compact, committed subset of the ledger (`.asd/conclusions/*.jsonl`) — kilobytes that travel with every clone, so judgment is inherited, not lost. |

---

## Commands

### Setup & onboarding

| Command | What it does |
|---|---|
| `asd init` | Initialize an ASD repo: create the SQLite db, update `.gitignore`, and install the git hooks (pre-commit export, post-merge/post-checkout import + reindex). |
| `asd onboard` | One-shot post-clone setup: runs `init → index → conclusions import` in the right order so a fresh checkout is fully usable in one command. Idempotent. |
| `asd hooks` | Show the installed git hooks and their status. |
| `asd mcp install` | Register the `asd-mcp` stdio server in every detected agent's MCP config (Claude Code/Desktop, Cursor, Codex, Gemini CLI, Windsurf, Zed, VS Code, Cline, and more). `--tool <name>` targets one; `--db <path>` pins a database; `status` / `uninstall` manage it. |
| `asd skill` | Install ASD's **Agent Skill** (`SKILL.md`) into each host's skills directory — teaches the agent *when* to reach for ASD. Version-stamped; won't clobber a newer on-disk skill. Installs the combined **ASD + CTXone** suite skill when `ctx` is present. `--status` / `--dry-run` / `--project` / `--tool` / `--remove`. |
| `asd bootstrap` | Print a paste-into-your-agent block that installs, indexes, and connects ASD — and offers to set up CTXone too. |
| `asd watch` | Watch the repo and re-index on source changes, so the index never silently drifts. |

### Indexing, reading & orientation

| Command | What it does |
|---|---|
| `asd index .` (alias `reindex`) | Walk source files and build the FTS + ASG index. Re-runnable and idempotent; `--verbose` lists skipped files. |
| `asd read <symbol>` (alias `code_read`) | Read a symbol with its effects and recent ledger entries. |
| `asd list` | List indexed symbols, effects, or ledger entries. |
| `asd search <query>` (aliases `code_search`, `code_query`) | Ranked concept search over indexed symbols (FTS + hybrid boost). |
| `asd references <symbol>` | Exact-identifier references via literal text scan + index lookup — rg-style completeness (needs `rg` on PATH). |
| `asd status` | Index health: age, symbol count, and optionally dirty source files. |
| `asd trust` | **State Trust Score** — a single machine-readable rollup of index freshness, sidecar status, ledger density, dirty files, and concept gaps. Answers "can I rely on ASD for this task right now?" |
| `asd scopes` | List named scope aliases from `.asd/scopes.toml` (discoverability for `--scope` / `--paths`). |
| `asd architecture` | One-call "orient me" overview for a cold agent: languages, packages, layers, routes, and call-graph hotspots. |

### Change analysis (the pre-edit workflow)

| Command | What it does |
|---|---|
| `asd callers <symbol>` (alias `callers_of`) | Symbols that call this one, direct or transitive. |
| `asd callees <symbol>` (alias `callees_of`) | Symbols this one calls, direct or transitive. |
| `asd context-for <symbol…>` | Assemble query context for one or more symbols: signature, callers/callees, effects, invariants, hazards, and ledger. |
| `asd impact <symbol>` | **Blast-radius** analysis before an edit: transitive callers, aggregated effects, invariants/hazards, affected tests, and recent git touches. |
| `asd checklist <symbol>` | A structured pre-edit checklist: files to inspect, invariants to preserve, tests to run, known hazards, effects to verify. |
| `asd prepare-change` | The one-call agent context package for a planned change: design invariants, layer-grouped entry points, likely edit files, affected tests, effects, and recent git touches — composed into a single JSON response. |
| `asd since <sha>` | Symbols in files changed since a commit + combined blast radius. The PR-review path: pass a base SHA, get impact without knowing symbol names. |
| `asd investigate <query>` | Broad feature archaeology: search → expand call chains, invariants, hazards, and effects for the top entry points in one pass. |
| `asd endpoints` | List cross-service endpoints (HTTP routes/clients, pub-sub) detected in the repo, show matched in-repo edges, and `--export` a service manifest. Resolves nested/aliased/multi-mount router prefixes so a client call and a server route are keyed by the same full runtime contract. |
| `asd dead-code` | Functions/methods with no inbound call edges (candidate dead code), excluding route handlers, tests, and main/dunder methods. |

> **Contract-drift detection.** Because `asd endpoints` keys both client
> calls and server routes by the same normalized contract (method + full
> resolved path), a client call with **no matching route** is a candidate
> drift signal — a caller reaching an endpoint the server no longer serves
> at that path (e.g. a frontend calling `/api/calculators/budget` after the
> backend moved budgets under `/api/budget/…`). In one repo this surfaces as
> unmatched consumers; across repos it becomes cross-service impact (Team).

### Ledger & judgment

| Command | What it does |
|---|---|
| `asd ledger append/get/find/approve/reject/withdraw/supersede/rebind` | Full decision-ledger operations. Append decisions, hazards, rationale, constraints; run the approval workflow; rebind entries across renames. |
| `asd invariant add/list/withdraw` | Record and manage invariants on symbols — a typed shortcut for `ledger … --kind invariant`. |
| `asd conclusions` | View ledger entries bucketed by the six conclusion classes (decisions, classifications, mappings, hazards, recipes, followups); `export` / `import` drive the git sidecar. |
| `asd scratch` | Working notes scoped to a symbol or investigation, with a promote-to-ledger path. Local-only; not synced. |
| `asd think speculate/model/failed/question/list` | Capture agent thinking — hypotheses, mental models, failed attempts, open questions — so the next session inherits the reasoning, not just the result. |
| `asd map` | Initial-read project summary: walk the index, identify package boundaries, tag test files, and write Ownership entries so the next session inherits the mental model. |
| `asd recipe` | Structured change-intent recipes — per-file action plans (Delete / Gate / Run / KeepAsCovered / Review) for known task families. |
| `asd annotate-commit` | Derive ledger annotations from a git commit (changed files + message → touched symbols) and suggest or record decisions, invariants, proofs, and hazards. |
| `asd task-close` | Close an active task: write proof + validation entries for every symbol affected by HEAD, tagged with CTX plan/task provenance. |
| `asd feedback` | Record and list search-quality feedback verdicts for (query, symbol) pairs — the signal that tunes ranking. |

### Effects, policy & audit

| Command | What it does |
|---|---|
| `asd verify-effects <symbol>` | Verify a symbol's declared effects. |
| `asd trace <program>` | Run a program under the ASD runtime tracer and ingest observed effects (Python, via `sys.settrace`). |
| `asd policy` | Introspect the active policy (requires `--policy` / `ASD_POLICY`). |
| `asd audit tail/verify` | Read back the hash-chained audit log of ledger mutations, policy evaluations, and effect declarations; verify chain integrity. |

### Sidecar, sync & maintenance

| Command | What it does |
|---|---|
| `asd conclusions export` / `import` | Write the committed sidecar (`.asd/conclusions/*.jsonl`) from the ledger, and load it back. Wired into the git hooks automatically. |
| `asd sync` / `asd hydrate` | Mirror ASG state to the legacy `.asd/v1/` sidecar and back (local debug; superseded by conclusions for the commit path). |
| `asd sidecar migrate` | Flip a repo from the legacy `.asd/v1/` layout to the compact `.asd/conclusions/` layout. |
| `asd repair` | Scan the ASG for integrity issues (orphaned refs, malformed blobs, stale edges); read-only by default, `--fix` applies safe corrections. |

### Benchmarks & multi-repo

| Command | What it does |
|---|---|
| `asd scorecard` | Benchmark scorecard across the five ASD dimensions: truth, feedback, change, uncertainty, workflow. |
| `asd probe` | Golden benchmark harness — structural assertions against command output to catch ranking/classification regressions. |
| `asd workflow` | Task-workflow session history: evidence quality, workflow type, and missing steps across recent `task-close` runs. |
| `asd repo add/list/use/rm/show` | Manage the shared repo registry at `~/.config/asd/repos.toml`. |

---

## Effect categories

Effects are declared per symbol and propagate transitively. The 17
categories cover filesystem, network, database, process, environment, and
more — e.g. `io.fs.read`, `io.fs.write`, `io.net.out`, `io.net.in`,
`io.db.read`, `io.db.write`, `io.proc.spawn`, `io.env.read`. `asd read`,
`asd impact`, and `effects_of` (MCP) surface the aggregated set for any
symbol so an agent knows the real-world reach of a change before making it.

---

## MCP tools

Agents reach ASD over MCP through a flat namespace (so tools don't collide
with other servers). The canonical CLI↔MCP mapping is in
[mcp-cli-mapping.md](mcp-cli-mapping.md). Representative tools:

- **Read & orient:** `health`, `code_search` / `code_query`, `code_read`,
  `references`, `callers`, `callees`, `effects`, `context_for`, `impact`,
  `prepare_change`, `since`, `investigate`, `architecture`, `trust`,
  `endpoints`, `dead_code`, `status`, `scorecard`, `traces`, `scopes_list`.
- **Ledger & judgment:** `ledger_get`, `ledger_find`, `ledger_append`,
  `ledger_approve`, `ledger_reject`, `ledger_withdraw`, `ledger_supersede`,
  `ledger_rebind`, `invariant_add`, `invariant_list`, `effect_declare`,
  `verify_effects`, `checklist`, `feedback_list` / `feedback_mark` /
  `feedback_promote`.
- **Thinking & scratch:** `think_speculate`, `think_model`, `think_failed`,
  `think_question`, `think_list`, `scratch_*`.
- **Sidecar & audit:** `conclusions_export` / `conclusions_import` /
  `conclusions_list`, `audit_tail`, `audit_verify`, `annotate_commit`,
  `task_close`, `reindex`.
- **Recipes:** `recipe_classify_test_migration`,
  `recipe_migrate_stale_tests`.

---

## On-disk layout

| Location | Contents | In git? |
|---|---|---|
| `.asd-state.db` | Live SQLite ASG — index, call graph, FTS, full ledger, traces | No (gitignored) |
| `.asd/conclusions/*.jsonl` | Compact committed subset: decisions, classifications, mappings, hazards, recipes, followups, agent thinking | **Yes** |
| `.asd/v1/` (legacy) | Older verbose mirror, superseded by conclusions | No (gitignored) |

The principle: **the committed sidecar carries judgment; everything else is
regenerable** from source via `asd index .`.

---

## Editions

Everything above is **OSS** — single-repo, self-hosted, no account. Team
(cross-repo) and Enterprise (org governance) build on top. See
[LICENSING.md](../LICENSING.md).
