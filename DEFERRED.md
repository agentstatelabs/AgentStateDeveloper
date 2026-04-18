# Deferred items

Running log of known gaps, scope-cut trade-offs, and future work that
wasn't picked up in the current milestone. Not a roadmap — these are
things I'd want a future maintainer (or future me) to see *before*
assuming something's missing on accident.

Last synced: M7 landed (2026-04-18).

## Tracer (`tools/asd_tracer.py`)

- Single-process. Effects inside a `subprocess.Popen`-launched child
  aren't instrumented; only the parent's call to `Popen` is recorded
  as `proc.spawn`.
- Single-thread. `sys.settrace` installs only on the current thread;
  worker threads spawned by the traced code run untracked.
- Monkeypatch coverage is still pragmatic (open/print/log/time/net/
  env/random/subprocess/os.exec). Gaps: raw sockets not wrapped,
  memory-mapped file I/O, async/await variants of urllib/http.client.

## Static effect inference (Python adapter)

- Regex-ish substring matcher on symbol bodies. False positives
  possible when an effect-like substring appears in a comment or
  string literal. Not fixed because impact on audit signal is small
  (the trace layer is ground truth anyway).
- SQL-keyword detection on `.execute(...)` uses simple prefix match.
  CTEs (`WITH`) and pragma-style statements may classify oddly.

## Call graph (Python)

M5 scope exclusions, in order of likely user-facing impact:

- **`from foo import *`** — unresolvable statically, silently skipped.
- **Relative imports** (`from . import x`, `from ..pkg import y`) —
  skipped in M5. Not structurally hard to add if there's a user ask.
- **Imports inside function bodies / conditional blocks** — only
  module-scope imports are scanned.
- **Module-scope call sites** — the walker only enters
  function/method bodies. A script with top-level work won't produce
  call edges unless wrapped in `main()`. Sample `_driver.py` now
  demonstrates this pattern.
- **Dynamic dispatch** — computed attribute access, callbacks passed
  as arguments, `getattr`, metaclass trickery. Out of static scope.
- **Multi-segment module imports** with attribute chains beyond the
  first (`import foo.bar` → only `foo.X()` binds; `foo.bar.baz()`
  resolves only via `import foo.bar`). M5 accepts this limitation.

## Policy

M7 shipped `FilePolicyGate` + `--policy` wiring on `asd ledger
append`. Remaining gaps:

- `agentstategraph-policy` crate still not built (design in
  [POLICY_V1.md](/Users/user/Documents/AgentStateLabs/strategy/POLICY_V1.md)).
  Our `FilePolicyGate` is interim; schema is a subset so migration is
  a rename.
- **MCP parity** — MCP `ledger_append` and `effect_declare` don't yet
  route through the gate. Only the CLI does. M8 target.
- **Effect-declare routing** even in the CLI — `asd effect` (no such
  command yet; effect_declare is only via MCP). Once MCP is plumbed,
  CLI should grow it for parity.
- **No selector DSL** — `match_action` is exact or prefix-`.*` match,
  plus optional `agent_id` equality. POLICY_V1 envisions richer
  conditions (paths, timestamps, qualifiers). Expand when real use
  cases arrive.
- **No policy introspection CLI yet** — POLICY_V1 §7 has `ctx policy
  list / show / evaluate`. Planned for M8 as `asd policy …`.
- **Approval is advisory** — entries land with `tags:
  [awaiting-approval, approver:human]` but nothing prevents a later
  reader from acting on them before a human flips the tag. No
  ratification workflow yet.
- **Hot reload** — policy file is loaded once at engine open. Changes
  require restart.
- **No policy coverage over**: traces (`asd trace` ingest), index
  (`asd index` writes), merge (no merge surface yet), rename.
- **Lens surface** — matched_policy is in the data but not yet
  rendered, and there's no filter for `awaiting-approval`. M8 target.

## HTTP / MCP

- `resolve_symbols_by_ids` does an O(N) qname-tree scan per request.
  Fine for solo-dev repos; needs a reverse `symbol_id → qname` index
  at enterprise scale.
- `ledger_find` is an O(n) tree scan across all entries, capped by
  `limit`. No composite index; no pagination beyond limit.
- No auth on HTTP or MCP servers. Localhost-only assumption is
  implicit, not enforced. Enterprise repo would add API-key + RBAC.
- `health.db_path` reports the canonical path (M4); `symbol_count`
  reflects indexed-qnames count only, not a total artifact count.

## UI (Lens)

- Read-only. No ratification queue, no ledger writes from the web,
  no effect-declaration edits. All mutations via CLI or MCP.
- No cross-module graph visualization (edges are exposed via API
  but rendered as flat lists, not a graph).
- No effect-distribution overview route (e.g., "which 10 symbols
  have the biggest transitive blast radius?").
- `via` symbol_ids in transitive effects should be clickable/
  resolvable to qnames. Planned for M6b.

## Languages

- Python only through M5. TypeScript adapter planned for M6.
- No Go / Java / Rust self-hosting / other languages.

## Enterprise scaffolding

Explicitly out-of-repo, but we have nothing built:

- No registry server (for cross-machine authoring-history pull).
- No audit export connectors (SIEM/Splunk/Datadog).
- No enterprise SSO / RBAC on symbols, ledger entries, policies.
- No admin UI for multi-tenant scoping.
- No Postgres multi-tenant wiring exercised (ASG supports it; ASD
  just defaults to SQLite).

## Miscellaneous

- `asd index` summary doesn't report *dropped* call edges (callees
  that couldn't be resolved). Visibility helper — would tell users
  how much of the dynamic/cross-module surface the adapter missed.
- Trace entries carry timestamps but no duration or per-call timing.
- No schema migration story on disk. Everything lives under
  `/asd/v1/` so a v2 can sit alongside, but there's no migration tool.
- `.asd/` on-disk committable sidecar was specified in DESIGN.md but
  never implemented — currently everything lives in the SQLite ASG
  repo only. Implementing `.asd/` would enable the git-roundtrip
  promise for cold clones.

## Working-style

- Two sub-agents across M1–M5 hit sandbox permission denials on
  `cargo` / `npm` and returned without self-verifying. I took over
  each time. If this becomes a pattern, either relax agent sandbox
  allowlists or stop asking agents to self-verify builds.
