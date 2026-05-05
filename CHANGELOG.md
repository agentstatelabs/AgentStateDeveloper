# Changelog

All notable changes to AgentStateDeveloper are documented here.
Versions use semantic versioning; each milestone increments by 0.0.5.

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
