# Changelog

All notable changes to AgentStateDeveloper are documented here.
Versions use semantic versioning; each milestone increments by 0.0.5.

---

## [0.9.84] — 2026-05-19 — Plan B complete (compact conclusion sidecar)

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

## [0.8.5] — 2026-05-05

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

## [0.8.0] — 2026-05-05

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

## [0.7.5] — 2026-05-05

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

## [0.6.2] — 2026-05-05

### Added
- **`asd list symbols/effects/ledger`** — enumerate indexed objects with optional
  filters (`--lang`, `--kind`, `--file`, `--has-declared`, `--category`)
- **`asd list stats`** — aggregate graph metrics: symbol counts by language and
  kind, effect categories, verification status, call graph edge counts, ledger totals

### Fixed
- **`asd trace` help text** — clarified that the tracer is Python-only (uses
  `sys.settrace`); previously said "Run a Python program" which was misleading

---

## [0.6.1] — 2026-05-05

### Added
- **Always-on index log** — every `asd index` run writes full per-file progress
  and all skipped files to `.asd/index.log`; `--verbose` tees that same output
  to stderr; skipped files capped at 100 on stderr with "…and N more" hint

---

## [0.6.0] — 2026-05-05

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

## [0.5.5] — 2026-05-05

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

## [0.5.0] — 2026-05-04 (M19)

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

## [0.4.5] — 2026-05-03 (M17–M18)

### Added
- OSS / commercial tier split: `approve`, `reject`, `withdraw` ledger operations gated behind `asd-pro`
- Audit log: hash-chained JSONL event stream for all ledger and policy mutations
- Audit tail: `asd audit tail` and `asd audit verify` CLI commands; `audit_tail` and `audit_verify` MCP tools
- Lens review UI: live streaming audit feed, tamper-evident badge, SPA routing

---

## [0.4.0] — 2026-04-28 (M13–M16)

### Added
- Marketing and docs site (`agentstatedeveloper.dev`) built with Astro
- Lens review UI (`asd-serve`) with symbol inspector and ledger timeline
- 9-language semantic index: Python, TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift
- Swift and Kotlin adapters using bundled tree-sitter C grammars + `LanguageFn::from_raw`

---

## [0.3.0] — 2026-04-20 (M1–M12)

### Added
- Core semantic index (tree-sitter), decision ledger, effect declarations, call graph
- Policy gate (file-backed JSON rules: allow, deny, require-approval)
- Ratification workflow (approve, reject, withdraw — commercial tier)
- MCP server (`asd-mcp`) exposing 14+ tools to coding agents
- HTTP server (`asd-serve`) for Lens UI
- CLI: `init`, `index`, `read`, `ledger`, `policy`, `verify-effects`, `trace`, `sync`, `hydrate`, `audit`, `hooks`
