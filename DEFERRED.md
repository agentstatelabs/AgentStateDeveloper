# Deferred items

Running log of known gaps, scope-cut trade-offs, and future work that
wasn't picked up in the current milestone. Not a roadmap — these are
things a future maintainer (or future me) should see *before* assuming
something's missing on accident.

Last synced: **1.0.25 (2026-06-02)** — Plan L t-001 refresh.
Previous sync: M17 follow-on (2026-05-02), 7 months stale.

Most "still deferred" items below now have a home in Plans H–L
(see DESIGN.md). This file remains the canonical inventory; the
plans are the execution view.

---

## Status legend

- **DEFERRED** — still intentionally not done; rationale below
- **RESOLVED** — shipped; kept here as a breadcrumb so the entry
  isn't re-raised
- **PLANNED** — pulled into Plan H / I / J / K / L for execution
- **SUPERSEDED** — circumstances changed; entry rewritten or dropped

---

## OSS / commercial tier split (M17 + follow-on)

- **DEFERRED — License-key / billing enforcement** on `asd-pro`. Gated
  on paying customers existing. Tracked as Plan I t-047.
- **RESOLVED — `RatifyOps` trait wired through `Engine`** via
  `OnceLock` override; `asd-pro` installs `RatifyOpsImpl` at startup
  (2026-05-02).
- **RESOLVED — `asd-pro-mcp` + `asd-pro-serve` binaries ship in
  `agentstatedeveloper-pro`** crate; both install `JsonlFileSink` +
  `RatifyOpsImpl` at startup (2026-05-02).
- **RESOLVED — `agentstatedeveloper-ratify`** has 14 integration tests
  covering happy paths, idempotency, authorisation enforcement, and
  all error guards (2026-05-02).

## Tracer (`tools/asd_tracer.py`)

All four items below are **DEFERRED** until `asd trace` becomes a
hot path in real workflows. Tracked as Plan I t-002 / t-003 / t-004
/ t-042; cut from Plan L (rarely used today).

- Single-process: subprocess children aren't instrumented (only the
  parent's `Popen` call is recorded as `proc.spawn`).
- Single-thread: `sys.settrace` installs only on the current thread.
- Monkeypatch gaps: raw sockets not wrapped, memory-mapped file I/O,
  async/await variants of urllib/http.client.
- Trace entries carry timestamps but no duration or per-call timing.

## Static effect inference (Python adapter)

- **PLANNED (Plan L t-002)** — Substring matcher fires on effect-like
  strings inside comments / string literals. Cheap fix: strip
  comments and literals before scanning.
- **DEFERRED (Plan I t-006)** — SQL classifier on `.execute(...)`
  uses prefix match; CTEs (`WITH`) and pragma statements classify
  oddly. Real edge cases but rare.

## Call graph (Python)

M5 scope exclusions, current dispositions:

- **DEFERRED (Plan I t-007)** — `from foo import *`. Needs FTS lookup
  of foo's exports; bigger than the other call-graph fixes.
- **PLANNED (Plan L t-003)** — Relative imports (`from . import x`,
  `from ..pkg import y`).
- **PLANNED (Plan L t-004)** — Function-body / conditional imports.
- **DEFERRED (Plan I t-010)** — Module-scope call sites. Rare in
  production Python; sample `_driver.py` demonstrates the
  `main()`-wrap workaround.
- **PLANNED (Plan L t-005)** — Dynamic dispatch (`getattr`,
  callbacks, metaclasses). Permanently out-of-scope for static
  resolution; Plan L emits a callsite warning when patterns appear.
- **DEFERRED (Plan I t-011)** — Multi-segment module imports
  (`import foo.bar.baz`).

## Policy

- **DEFERRED (Plan I t-013)** — `agentstategraph-policy` crate not
  built; current `FilePolicyGate` is interim. Migration is a rename.
  Gated on POLICY_V1 maturity (upstream work).
- **DEFERRED (Plan I t-014)** — Selector DSL: paths, timestamps,
  qualifiers.
- **DEFERRED (Plan I t-015)** — Hot reload (today: load-on-engine-open
  only).
- **DEFERRED (Plan I t-016–t-019)** — Policy coverage over `asd
  trace` ingest, `asd index` writes, merge surface, rename. Today
  the gate fires on ledger append/approve and effect_declare only.

### Ratification (M9) — remaining gaps

- **PLANNED (Plan L t-007)** — `asd ledger reject <entry>` action.
  Today the workaround is "don't approve" — entry sits forever.
- **DEFERRED (Plan I t-021)** — Revoke approved entry
  (security-adjacent; gated on real use case).
- **PLANNED (Plan L t-008)** — Approval rationale: `--message` on
  approve + first-class `approval_note` field.
- **DEFERRED (Plan I t-023)** — Cryptographic signing (ed25519).
  Enterprise-shaped; gated on customers.
- **PLANNED (Plan L t-009)** — `asd ledger supersede` surface across
  CLI / MCP / HTTP. Schema already supports `supersedes: [entry_id]`.

## HTTP / MCP

- **DEFERRED (Plan I t-025)** — `resolve_symbols_by_ids` does an O(N)
  qname-tree scan per request. Fine for solo-dev; needs reverse
  `symbol_id → qname` index at enterprise scale.
- **DEFERRED (Plan I t-026)** — `ledger_find` is O(n) tree scan, no
  pagination beyond `limit`, no composite index.
- **DEFERRED (Plan I t-027)** — No auth on HTTP/MCP. Localhost-only
  assumption is implicit. Enterprise repo would add API-key + RBAC.
- **PLANNED (Plan L t-010)** — `health.symbol_count` returns
  indexed-qnames count only; should report total artifact count
  (symbols + ledger entries + effects).

## UI (Lens)

Whole cluster is **DEFERRED**. Needs its own design pass + plan —
not folded into L. Tracked as Plan I t-029 → t-034 + t-046.

- No reject / withdraw-approval buttons (pairs with ratification
  gaps).
- No cross-module graph visualization.
- No effect-distribution overview route.
- No `effect_declare` UI.
- No policy authoring UI.
- No "who approved what, when" timeline view.
- Lens verify-badge UI: backend works, not surfaced.

## Languages

**RESOLVED** — 10 language adapters now ship:
`agentstatedeveloper-{python,typescript,rust,go,java,csharp,ruby,kotlin,swift}`
plus the umbrella `agentstatedeveloper-adapters` crate. The 2026-05
note ("Python only through M5") was 7 months stale.

## Enterprise scaffolding

All five items **DEFERRED**. Gated on customers existing. Tracked as
Plan I t-036 → t-040.

- Registry server (cross-machine authoring-history pull).
- Audit export connectors (SIEM/Splunk/Datadog).
- Enterprise SSO / RBAC on symbols / ledger / policies.
- Admin UI for multi-tenant scoping.
- Postgres multi-tenant exercised end-to-end (ASG supports it; ASD
  defaults to SQLite).

## Sidecar

- **RESOLVED — `.asd/` on-disk committable sidecar** shipped via Plan
  A (M10 target) → Plan B compact format
  (`.asd/conclusions/*.jsonl`) → Plan G adds the thinking shard.
  The 2026-05 "never implemented" claim was wrong by then.
- **DEFERRED — Sidecar canonicalization** (sort-on-write, effect
  filter, confidence-floor filter, self-describing entries,
  `asd onboard`, per-package sharding, `--check-budget`). Tracked
  as Plan K (10 tasks). Standing recommendation: execute Plan K
  before the sidecar grows past 1 MB on any real project.

## Schema migration

- **DEFERRED (Plan I t-043)** — No `/asd/v1/` → `/asd/v2/` migration
  tool. The path prefix is in-SQLite (not on-disk), so v2 can sit
  alongside v1 without conflict, but there's no migration helper.

## Audit log

- **RESOLVED — Event stream** (M12), **tail parity across CLI / HTTP
  / MCP / Lens** (M14), **hash-chained tamper evidence** (M15). Every
  event carries a blake3 hash of its own canonical bytes plus the
  previous event's hash; `asd audit verify` walks and reports breaks.
- **DEFERRED (Plan I t-044)** — Log rotation / retention policy.
- **DEFERRED (Plan I t-045)** — Real-time streaming (today: poll via
  `since:<event_id>` cursor).
- **DEFERRED (Plan I t-046)** — Lens verify-badge UI.

## Diagnostics

- **PLANNED (Plan L t-006)** — `asd index` summary doesn't report
  dropped (unresolved) call edges. Closes a real visibility gap —
  agents/humans can't tell what the indexer missed.

## Working-style (meta)

- **DEFERRED (Plan I t-048)** — Sub-agent sandbox allowlist for
  `cargo` / `npm` so agents can self-verify builds. Belongs in
  `~/.claude/settings.json`, not ASD repo — included here only so
  it doesn't fall through.

---

## New gaps surfaced since 2026-05-02

These weren't in the previous DEFERRED sync; surfaced during M18–M31
and Plans A–G.

### From M18–M27 field evaluations (Plan J)

- **PLANNED (Plan J t-011)** — Per-query scope/exclusion polish
  (negative globs, language exclusions, named exclude sets).
- **PLANNED (Plan J t-012)** — Why-this-result `why[]` array on
  every hit listing the signals that ranked it.
- **PLANNED (Plan J t-014)** — Feedback verdict TTL — `Useful +1.5`
  boosts persist forever today; needs half-life.
- **PLANNED (Plan J t-009)** — qname collision across language
  adapters (e.g. `pkg.Model` in both Python and Swift).
- **PLANNED (Plan J t-001)** — Invariants on a caller silently
  dropped when the query lands on the callee.
- See DESIGN.md "Plan J" for the full 16-task list.

### Plan F dormant tasks (Plan H)

- **DEFERRED until external signal** — Index-time penalty denorm,
  Crucible re-run validation, ExampleFlow sidecar-size validation,
  full `prepare_change` orchestration extract. All four tracked as
  Plan H with concrete trigger conditions in DESIGN.md.

---

## Cross-reference: DEFERRED → execution plan

| Cluster | Plan | Count | Status |
|---|---|---|---|
| OSS billing | I t-047 | 1 | Gated on customers |
| Tracer | I t-002/3/4/42 | 4 | Cut from L (rare usage) |
| Effect inference | L t-002 (one), I t-006 | 2 | One planned, one deferred |
| Call graph (Python) | L t-003/4/5 + I t-007/10/11 | 6 | Three planned, three deferred |
| Policy | I t-013–t-019 | 7 | All gated on POLICY_V1 upstream |
| Ratification | L t-007/8/9 + I t-021/23 | 5 | Three planned, two deferred |
| HTTP/MCP | L t-010 + I t-025/26/27 | 4 | One planned, three deferred |
| Lens UI | I t-029–t-034 + t-046 | 7 | Needs own plan |
| Languages | RESOLVED | 0 | All shipped |
| Enterprise | I t-036–t-040 | 5 | Gated on customers |
| Sidecar | Plan K | 10 | Drafted; ready when sidecar grows |
| Schema migration | I t-043 | 1 | Deferred |
| Audit log | I t-044/45/46 | 3 | All deferred |
| Diagnostics | L t-006 + L t-010 | 2 | Planned |
| Working-style | I t-048 | 1 | Out-of-repo (~/.claude) |
| Field eval | Plan J (16) | 16 | M25 cluster most urgent |
| Plan F dormant | Plan H (4) | 4 | Trigger-gated |
