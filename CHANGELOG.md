# Changelog

All notable changes to AgentStateDeveloper are documented here.
Versions use semantic versioning; each milestone increments by 0.0.5.

---

## [Unreleased]

Everything on `main` since the v1.3.1 tag.

### Added — parallel-agent worktrees

- **`asd worktree`** — plan-scoped git worktrees, mirroring CTXone's
  `ctx worktree`. Each unit of work gets its own worktree + `plan/<name>`
  branch, so parallel agents get isolated files and HEAD (they can't clobber
  each other) while sharing context through the hub.
  - `asd worktree start <plan>` adds `../<repo>-wt-<plan>` on `plan/<plan>`;
    `list` recovers the plan↔worktree binding from `git worktree list`;
    `finish <plan>` merges back and (by default) tears the worktree down
    (`--keep` to skip teardown, `--push` to push the merged branch).
  - `--shared-target` shares one Rust build cache across worktrees (points
    `target-dir` at `<repo>/.wt-target`, avoiding a multi-GB `target/` per
    tree).
  - `--clone` isolates via a fresh clone with its own `.git`
    (`../<repo>-clone-<plan>`) for remote/cloud agents on another machine,
    merging back via `origin` instead of a local merge.
  - New worktrees auto-enable the repo's `.githooks`.

### Added — on-demand instruction disclosure

- **`asd help [topic]`** and the **`asd-mcp` `help` tool** (the **64th** MCP
  tool) — return compiled-in feature docs (synopsis, syntax, params,
  examples, gotchas) for one feature or the full catalog, instead of carrying
  every tool's full docs in context every turn. Docs are version-pinned to the
  running binary, so the CLI and MCP tool return byte-identical payloads.
  - `--agent` for machine-readable JSON; `--manifest` prints this binary's
    feature manifest.
  - **Cross-binary proxy**: an unknown topic is resolved by the owning tool
    (asd ↔ ctx). `--publish` writes this binary's manifest into the shared
    cross-tool help index so a unified `help` discovers asd's features
    alongside ctx's (tool-keyed — publishing asd never clobbers ctx's slice).
  - Comprehensive asd feature registry (15 → 64 features).
  - `asd skill`'s always-on block now points agents at `asd help` for
    on-demand syntax.

### Changed

- **`asd onboard`** now also folds in project-scoped MCP registration, so the
  one-shot post-clone setup connects the agent's tools as part of the same
  command.

## [v1.3.1] — 2026-07-29

Patch release: ASG integrity-gate bump + namespace sanitization.

### Changed

- Bumped `agentstategraph` v0.9.6 → v0.9.10 (merge integrity gate).

### Fixed

- **Project namespaces with out-of-charset characters no longer error.**
  ASG v0.9.10 tightened namespace validation to `[A-Za-z0-9_-]`, but asd
  derived its namespace straight from the project directory name — so dirs
  with dots or spaces (including tempdirs like `.tmpXXXX`) began failing. Added
  `Engine::sanitize_namespace` to map out-of-charset characters to `_` before
  `Namespace::new`. Valid names pass through unchanged, so existing projects
  keep their namespace and no data moves.

## [v1.3.0] — 2026-07-13

Everything on `main` since the v1.2.0 tag (Plans T–V + workflow hardening).

### Added — agent-workflow surface

- **New MCP tools** `map`, `sync`, `test_summary` — bringing the MCP surface to
  **63 tools**. `map` (also `asd map`) persists Ownership ledger entries on
  initial read (contrast the read-only `architecture`); `test_summary`
  (`asd test-summary`) emits a failures-only summary of test-runner output.
- `asd prepare-change` gains an `--avoid` alias (for `--exclude`) plus documented
  `--paths` / `--scope` hints to fight lexical over-matching.
- `asd annotate-commit` splits **directly-edited** symbols from **nearby/touched**
  ones when deriving ledger annotations from a commit.
- Agent-discoverability polish across the CLI help and MCP tool descriptions.
- **Registry hygiene** — `asd repo prune` removes entries whose `.asd-state.db`
  no longer exists (`--dry-run` to preview, `--json` for machine output). Root
  cause fixed too: `asd index` no longer auto-registers databases under temp
  directories and respects `ASD_NO_AUTO_REGISTER`, so test runs stop polluting
  `~/.config/asd/repos.toml`. And a real `asd index` now **opportunistically
  self-heals** the registry — dropping dead entries as it runs (opt out with
  `ASD_NO_AUTO_PRUNE`) — so a standalone asd install stays clean with no
  scheduler or CTXone.

### Added — Lens (asd-serve) review UI, Plan T

- `/territory`, `/activity` (live accountability), and the approvals / `/graph` /
  `/effects` pages; symbol-detail and `/code/*` render from `@agentstate/lens-core`;
  dark-first visual identity.

### Performance

- P0 bulk-read swaps + hydrate cache population + startup self-heal.
- `/callers`, `/callees`, `/graph` cut from minutes to <100ms (head-keyed
  id/edge memos); single-pass `/api/v1/symbols` and `/thinking` from minutes to
  ~2s at 10k symbols.

### Fixed

- `conclusions import` now round-trips thinking entries and confidence (was
  silent data loss).

## [v1.2.0] — 2026-07-06 — cross-service edges, federation & Lens backend

Consolidates the 1.0.24 → 1.2.0 arc (tags 1.0.44 / 1.0.48 / 1.1.15–1.1.23,
which shipped without individual entries). Headlines:

### Added — cross-service & cross-repo intelligence

- **Cross-service edge detection across 9 languages** (HTTP + pub-sub):
  Python (FastAPI/Flask, incl. intra-file / mount-tree / alias-keyed /
  multi-mount router-prefix resolution), TypeScript/JS, Go, Java/Spring,
  C#/ASP.NET, Ruby (Sinatra/Rails), Kotlin (Spring + Ktor), Swift (Vapor),
  and Celery pub-sub. `asd endpoints` keys client calls and server routes by
  the same normalized contract, so unmatched consumers surface contract drift.
- **Federation** (Plan Q): decision-aware `asd repo impact` and cross-repo
  service-edge matching via `asd repo edges`; coherent active-repo resolution
  and `asd mcp --follow-active`.
- **Edge-confidence** model (EdgeEvidence) surfaced in `context_for`.

### Added — MCP surface & onboarding

- New MCP tools `trust`, `architecture`, `endpoints`, `dead_code`.
- `asd skill` (version-stamped SKILL.md install, downgrade refusal, suite
  detection), `asd bootstrap` (paste-to-your-agent installer), `asd watch`
  (auto-reindex on source changes).
- More agent integrations: Kilo Code, Antigravity, Aider, JSONC config support;
  `asd mcp instructions` / `asd mcp install --project`; non-blocking Claude Code
  hook.

### Added — Lens backend & conformance

- Lens read APIs (search, graph, effects overview, timeline) and the
  `/api/v1/events` SSE live-activity stream; `@agentstate/lens-core` as a file dep.
- Cross-language capability matrix + contract tests; tier-2 conformance realism
  over real source trees; adapter network-effect detection fixes.
- `asd scorecard` internal token-economy estimate.

## [v1.0.23] — 2026-06-02 — Plan G complete (agent-thinking layer)

ASD now persists the agent's *thinking* — speculation, mental models,
failed attempts, open questions — so a fresh session resumes with
the expensive understanding it would otherwise re-derive. Auto-surfaced
in `prepare_change` / `context_for` as `prior_thinking`.

### What's new

- **Four new ledger kinds** — `Hypothesis` (uses `confidence`),
  `MentalModel` (body carries `symbols[]` + `name`), `FailedAttempt`
  (body: `tried` / `because`), `OpenQuestion`. Routed through a 7th
  `ConclusionClass::Thinking` bucket in the sidecar. (t-002)
- **`asd think <verb>`** — five subcommands (`speculate`, `model`,
  `failed`, `question`, `list`). Entry IDs are deterministic blake3
  of `(intent, qname, content)` so re-running the initial-read prompt
  overwrites rather than duplicates. Mirror MCP tools:
  `think_speculate` / `think_model` / `think_failed` / `think_question`
  / `think_list`. (t-003)
- **Initial-read prompt template** — `docs/initial-read-prompt.md`. A
  7-section guide the agent reads and acts against the indexed project,
  writing back via the `asd think *` commands. ASD doesn't make LLM
  calls; the template is the contract. (t-004)
- **`asd think bootstrap [--check]`** — guided entry point that prints
  the prompt path + starter checklist + write-back command reference.
  With `--check`, scans existing thinking entries and reports gaps
  (e.g. "no MentalModel yet"). Supports `--json` for MCP. (t-005)
- **`prior_thinking` auto-surface** — both `prepare_change` and
  `context_for` now embed a compact `prior_thinking` projection of
  the relevant symbols' thinking entries. Hypotheses below
  `DEFAULT_CONFIDENCE_FLOOR = 0.3` are excluded; nothing surfaces when
  no thinking exists. (t-006)
- **`ctx:task:<id>` provenance auto-tag** — every `asd think *` write
  (CLI and MCP) stamps the active CTX task id when set, matching the
  trail Plan E added for map/ledger writes. (t-007)

### Plan F follow-ups bundled in

- **`Move` recipe action** revived for the test-migration recipe, with
  `migrate_stale_tests()` reading `Mapping` bodies for move targets.
- **MCP `AsgEffectStore` construction** fixed (struct literal → `::new`).
- **`read_active_task_scope_from(env_raw, db_parent)`** extracted so
  parallel tests can pass explicit env strings instead of mutating the
  process env.

### Adoption (worked example)

```sh
# 1. Seed the project graph.
asd reindex

# 2. Print the prompt template path + checklist.
asd think bootstrap

# 3. Read docs/initial-read-prompt.md, then record findings:
asd think model audio-pipeline \
    --symbols pkg.mixer.Mixer,pkg.io.Input,pkg.io.Output \
    --summary "input → mix → output, single-threaded"
asd think speculate pkg.mixer.Mixer --conf 0.7 \
    --summary "buffer size 4096 chosen for ~93ms latency at 44.1kHz"
asd think question pkg.mixer.Mixer --q "what does the 4096 constant mean?"
asd think failed pkg.io.Output --tried "ring-buffer cache" \
    --because "broke under reload — state leaked across sessions"

# 4. Confirm coverage; --check reports remaining gaps.
asd think bootstrap --check

# 5. Subsequent prepare-change / context-for calls now embed:
#     prior_thinking: { hypotheses, mental_models, failed_attempts, open_questions }
```

### Plumbing

- `core::thinking::gather_prior_thinking(engine, qnames, min_confidence)`
  single-sources the `prior_thinking` shape. 7 unit tests cover the
  null case, surfacing, confidence-floor exclusion, mental_model /
  failed_attempt body parsing, and the non-thinking-kinds exclusion.
- 4 CLI tests cover the `ctx:task:<id>` resolver (env JSON, file
  fallback, env-wins-over-file, both-absent).
- New MCP regression test `plan_g_think_tools_are_registered` proves
  the 5 tool routes are wired.

---

## [v0.9.99] — 2026-05-20 — Plan C complete (semantic-layer moat)

The defining-feature release. Plan A built trust, Plan B built durable
storage, Plan C makes ASD remember the expensive task-specific
understanding the LLM forms so a new session doesn't re-derive the
same project mental model.

### The moat (what's new)

- **Active decisions** — `Constraint`/`Decision` ledger entries carrying
  a penalty role (`stale-api` / `audit-pending`) now actively suppress
  their symbols in ranked search, the same way `WrongLayer` feedback
  does. Memory becomes a ranking input, not a passive note. (t-003)
- **First-class role-tag vocabulary** — `RoleTag` enum with 8 canonical
  tags (`fast-test`, `diagnostic-test`, `fixture-path`, `stale-api`,
  `package-boundary`, `replacement-coverage`, `performance-critical`,
  `audit-pending`). CLI / MCP warn on unknown tags; old free-form
  strings still round-trip. (t-002)
- **Change-intent recipes** — `asd recipe classify-test-migration <q>`
  returns a structured `{intent, actions[]}` plan
  (`Delete` / `Gate` / `Run` / `KeepAsCovered` / `Review`) instead of a
  flat symbol list. Pattern is reusable; more recipes will follow. (t-004)
- **AlreadyCovered + DiagnosticOnly verdicts** — two new
  `FeedbackVerdict` variants that suppress like `Noisy` and prompt the
  caller to accrue a `Mapping` or `Classification` ledger entry in the
  same gesture. (t-005)
- **CTX task state → ASD ranking bias** — ASD now reads
  `CTX_ACTIVE_TASK` (or `.asd/cache/active-task.json`) and applies a
  soft +1.0 boost to candidates inside the task's recorded scope. Never
  a hard filter. (t-006)
- **`asd map`** — one-shot initial-read command that walks the index,
  identifies package boundaries, classifies test files
  (`fast-test` vs `diagnostic-test`), and writes `Ownership` ledger
  entries with role tags. Idempotent via deterministic entry IDs. The
  bootstrap that makes the downstream Plan C features useful on a fresh
  project. (t-007)

### How to adopt

```sh
asd index .                   # seed the index (unchanged)
asd map                       # one-shot: seed role-tagged Ownership entries
asd ledger append <qname> \\  # accrue a stale-api Constraint
  --kind constraint --role stale-api \\
  --summary "deprecated; do not use"
asd conclusions export        # commit the new conclusions
asd search "legacy"           # the stale-api symbol is demoted
```

Set `CTX_ACTIVE_TASK='{"task_id":"t-X","scope":["src/foo/**"]}'` once
per session to bias all queries toward the current task's scope.

### Acceptance probes

Four new probes in `examples/acmeflow-probes.toml` tagged `plan-c`:
recipe-returns-structured, diagnostic-only-verdict-suppresses,
asd-map-writes-classifications, stale-api-constraint-demotes-symbol.

---

## [v0.9.89] — 2026-05-20 — Plan D complete (Crucible token-efficiency)

Five fixes driven by AgentStateCrucible's A/B testing, which showed
the assisted arm paying 2.5–5.7× baseline tokens despite making
fewer or similar tool calls.

### Added
- **`--brief` output mode** (0.9.85) — global `--brief` flag and
  `ASD_FORMAT=brief` env var. Projects `read` / `callers` / `callees`
  responses down to load-bearing fields (qname, file:line, signature,
  first doc line). Drops `symbol_id`, `symbol_fp`, `language`, `kind`,
  `col`, end positions, full doc body, empty arrays. Expected token
  reduction: 60–80% on the cited commands.
- **MCP-era name aliases on CLI** (0.9.87) — `asd code_read`,
  `code_search`, `code_query`, `callers_of`, `callees_of` all route to
  the canonical CLI subcommand. Agents trained on older MCP-era docs
  no longer hit `unrecognized subcommand`.
- **`query_id` in responses** (0.9.89) — deterministic blake3-derived
  id (`Qf3a2b1c`) on `read` / `callers` / `callees` output so
  wrapper-side tools can dedup repeated queries.

### Fixed
- **`asd investigate` accepts unquoted multi-word queries** (0.9.86) —
  `asd investigate failing test store` now joins to one query. Crucible
  captured 3 consecutive turns lost to this UX trap.
- **Python relative-import call edges** (0.9.88) — `parse_imports`
  had an early-return on `relative_import` nodes that silently dropped
  every `from .foo import bar` site. Crucible's own package indexed
  71 symbols with **0 cross-module edges**; the fix resolves `.`/`..`
  prefixes against the importing file's module path. End-to-end
  reproducer landed as a unit test.

### Expected Crucible re-run outcomes
When Crucible pins 0.9.89, the three scenarios should flip:
- `asd-vs-baseline` — brief mode puts assisted ≤ baseline tokens.
- `inheritance-bug` — tie becomes clean assisted win.
- `cross-layer-tax` — assisted produces the reference fix
  (rates.py + display.py) instead of the proximate `apply_tax` patch.

Crucible posts the empirical delta report when the re-run completes.

---

## [v0.9.84] — 2026-05-19 — Plan B complete (compact conclusion sidecar)

### Migrating an existing repo

The committed sidecar shape changed in Plan B. Old layout was `.asd/v1/`
(tens of MB on real projects). New layout is `.asd/conclusions/*.jsonl`
(kilobytes). One-shot migration:

```sh
asd sidecar migrate           # writes .asd/conclusions/*.jsonl
git rm -r --cached .asd/v1    # drop the old bloat from tracking
git add .asd/conclusions/
git commit -m "plan-b: switch to compact conclusion sidecar"
```

Re-running `asd init` afterwards is safe — it scaffolds the new layout
and flips the git hooks (pre-commit → `asd conclusions export`;
post-merge / post-checkout → `asd conclusions import`).

### Added
- `asd conclusions list|export|import` — read, write, and round-trip the
  six conclusion classes (decisions, classifications, mappings, hazards,
  recipes, followups). Byte-stable JSONL; idempotent import keyed by
  `entry_id`.
- `asd sidecar migrate` — one-shot migration with savings report and
  `git rm --cached` instructions for the legacy `.asd/v1/` tree.
- Two new `LedgerKind` variants: `Mapping` (legacy → new coverage
  cross-refs) and `FollowUp` (open items tied to external task systems).
- Two optional `LedgerEntry` fields: `role` (classification tag) and
  `command` (canonical reproduction command).
- New MCP tools: `conclusions_list`, `conclusions_export`,
  `conclusions_import`.
- `field_lte` probe assertion kind (used by the new sidecar-size probe).

### Changed
- `asd init` now scaffolds `.asd/conclusions/` (tracked) and `.asd/cache/`
  (gitignored). Git hooks flipped from the old hydrate flow to the new
  conclusions round-trip.
- Updated `.gitignore` to add `.asd/cache/` alongside `.asd/v1/` (Plan A
  line preserved). `.asd/conclusions/` is intentionally not ignored.

---

## [v0.8.5] — 2026-05-05

### Added
- **Swift function signatures** — `asd index` now captures full Swift function
  signatures (parameter labels, types, return type, `async`/`throws`) from
  tree-sitter parse; stored in `symbol.signature`; shown in `asd read` and
  `asd context-for` output.
- **Extensible effect categories** — `EffectCategory` gains an `Other(String)`
  variant accepting any dot-namespaced string (e.g. `midi.send`,
  `audio.graph.connect`, `scheduler.restart`, `ui.state.mutate`).  Existing
  built-in categories are unchanged; new categories are user-declared.
- **Workflow ledger entry types** — three new `LedgerKind` variants:
  `invariant` (what must always be true), `ownership` (subsystem/team owner),
  `proof` (evidence an invariant holds). `hazard` was already present.
  All four are now available via `asd ledger append --kind`.
- **`asd context-for <qnames>`** — context assembly command for agent queries.
  Given one or more comma-separated qnames, returns a ranked package:
  symbol signature + location, direct callers/callees, declared + transitive
  effects, invariants, hazards, ownership, proofs, and other ledger entries.
  Accepts `--budget-tokens` and `--include-body`.

---

## [v0.8.0] — 2026-05-05

### Added
- **`asd callers <qname>`** — show direct or transitive callers of a symbol
  (`--depth N` for BFS expansion); output includes qname, file, and line.
- **`asd callees <qname>`** — same for callees.
- **Callers + callees in `asd read`** — every symbol read now includes its
  direct callers and callees resolved to qname + file:line.
- **`asd list effects --file <substr>`** — filter effects output by source
  file path substring (joins through symbol map).
- **`.asdignore` support** — place one directory-name pattern per line at
  the repo root to exclude custom paths from `asd index`.

### Fixed
- **Broken-pipe panic in `asd list`** — piping to `head` / `grep` now exits
  cleanly (SIGPIPE handled via `BrokenPipe` error check at process exit).
- **Stale `.claude/worktrees` symbols** — `.claude`, `.asd`, `.build`,
  `DerivedData`, `target`, `xcuserdata`, and other build/tool directories
  are now excluded from indexing by default.

---

## [v0.7.5] — 2026-05-05

### Fixed
- **O(N²) object storage in `asd index`** — `spec_set_json` per symbol created
  a growing Map node copy at every shared prefix on each write; 1,341-file repo
  produced a 59 GB DB. Now assembles complete subtree JSON in memory and writes
  each prefix with a single `spec_set_json` call. O(N) objects regardless of
  repo size.
- **O(N²) object storage in `asd hydrate`** — same root cause; fixed with the
  same bulk approach for symbols and effects.
- **Transitive DFS now fully in-memory** — uses in-memory `callees_of` map from
  Pass 2 instead of per-symbol `get_callees` repo reads.

### Added
- **Post-processing phase messages** — `asd index` prints phase markers to
  stderr after file parsing completes so large repos show progress during
  call graph and transitive propagation steps.
- **`asd list symbols/effects/ledger`** — enumerate indexed objects with
  optional filters.
- **`asd list stats`** — aggregate graph metrics: symbols by language/kind,
  effects by category and verification status, call graph edge counts, ledger totals.

### Fixed
- **`asd trace` help text** — clarified Python-only tracer constraint.
- **Always-on index log** — every run writes full per-file progress to
  `.asd/index.log`; `--verbose` tees to stderr; skipped files capped at 100
  on stderr.

---

## [v0.6.2] — 2026-05-05

### Added
- **`asd list symbols/effects/ledger`** — enumerate indexed objects with optional
  filters (`--lang`, `--kind`, `--file`, `--has-declared`, `--category`)
- **`asd list stats`** — aggregate graph metrics: symbol counts by language and
  kind, effect categories, verification status, call graph edge counts, ledger totals

### Fixed
- **`asd trace` help text** — clarified that the tracer is Python-only (uses
  `sys.settrace`); previously said "Run a Python program" which was misleading

---

## [v0.6.1] — 2026-05-05

### Added
- **Always-on index log** — every `asd index` run writes full per-file progress
  and all skipped files to `.asd/index.log`; `--verbose` tees that same output
  to stderr; skipped files capped at 100 on stderr with "…and N more" hint

---

## [v0.6.0] — 2026-05-05

### Added
- **`asd mcp install/uninstall/status`** — registers `asd-mcp` in known agent
  tool configs (Claude Code, Claude Desktop, Cursor); detects all present tools
  automatically; writes `ASD_DB` into the env block so agents connect to the
  right project database; `--tool` flag targets a single tool
- **`asd index` progress output** — standard mode prints file count and
  "Done. N symbols" summary; `--verbose` / `-v` shows each file as it is
  processed
- **Skipped-file reporting** — unrecognized extensions tracked in
  `CollectResult`; `skipped` field added to `IndexSummary` and all JSON
  output; standard mode hints to use `-v`; verbose mode lists every skipped
  file with `[skip]` prefix
- **`asd-mcp` and `asd-serve` binaries installed** via
  `cargo install --path crates/agentstatedeveloper-mcp`

### Fixed
- **`ledger rebind`** — from-qname was not resolved to `sym_...` ID before
  looking up entries to re-parent; `entries_moved` was always 0

### Docs
- README, quickstart, introduction, architecture updated with correct install
  instructions (`cargo install --path`, not `cargo install asd`), `asd mcp`
  usage, and index progress output examples
- Hero and FeatureGrid updated to show `asd mcp install` in setup and clone
  onboarding flows

---

## [v0.5.5] — 2026-05-05

### Added
- **30 new CLI integration tests** across 5 test modules:
  - `lang_smoke_rust_go_java_csharp` — index smoke + log-effect inference for Rust, Go, Java, C#
  - `lang_smoke_ruby_kotlin_swift` — same coverage for Ruby, Kotlin, Swift
  - `ledger_integration` — `ledger append`, `supersede`, `rebind`; commercial gates for `approve`/`reject`/`withdraw`
  - `policy_integration` — `policy list`, `policy evaluate` (allow/deny/awaiting-approval), write-time policy enforcement
  - `verify_effects_integration` — inferred effects surface as `unverified`, pure-symbol empty list, unknown-symbol error

### Fixed
- **`ledger rebind` bug** — `--from` qname was passed as a raw `symbol_id` key to `list_entries_with_superseded`, so no entries were ever moved to the new symbol. Now resolves the from-qname to its `sym_...` ID before re-parenting entries.

---

## [v0.5.0] — 2026-05-04 (M19)

### Added
- **Git-native sidecar** — `.asd/v1/` directory tracked in git; ledger entries and effects travel with every clone
- **`asd init`** — initializes `.asd-state.db`, updates `.gitignore`, and (by default) installs git hook scripts under `.asd/hooks/` with `core.hooksPath` activation; prints full hook table on install
- **`asd sync --prune`** — flushes SQLite state to `.asd/v1/` and removes orphaned sidecar files
- **`asd hydrate`** — loads `.asd/v1/` entries back into local SQLite (for post-clone or post-merge workflows)
- **`asd hooks`** — reports installed/missing status with ✓/✗ per hook
- **`--no-hooks` flag** on `asd init` — skips hook installation
- **Sidecar smoke tests** — 5 tests covering prune, hook install, `--no-hooks`, hooks status, `.gitignore` management

### Changed
- `.gitignore` management: `asd init` removes blanket `.asd/` ignore entries and adds `.asd-state.db` specifically

---

## [v0.4.5] — 2026-05-03 (M17–M18)

### Added
- OSS / commercial tier split: `approve`, `reject`, `withdraw` ledger operations gated behind `asd-pro`
- Audit log: hash-chained JSONL event stream for all ledger and policy mutations
- Audit tail: `asd audit tail` and `asd audit verify` CLI commands; `audit_tail` and `audit_verify` MCP tools
- Lens review UI: live streaming audit feed, tamper-evident badge, SPA routing

---

## [v0.4.0] — 2026-04-28 (M13–M16)

### Added
- Marketing and docs site (`agentstatedeveloper.dev`) built with Astro
- Lens review UI (`asd-serve`) with symbol inspector and ledger timeline
- 9-language semantic index: Python, TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift
- Swift and Kotlin adapters using bundled tree-sitter C grammars + `LanguageFn::from_raw`

---

## [v0.3.0] — 2026-04-20 (M1–M12)

### Added
- Core semantic index (tree-sitter), decision ledger, effect declarations, call graph
- Policy gate (file-backed JSON rules: allow, deny, require-approval)
- Ratification workflow (approve, reject, withdraw — commercial tier)
- MCP server (`asd-mcp`) exposing 14+ tools to coding agents
- HTTP server (`asd-serve`) for Lens UI
- CLI: `init`, `index`, `read`, `ledger`, `policy`, `verify-effects`, `trace`, `sync`, `hydrate`, `audit`, `hooks`
