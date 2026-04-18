# Deferred items

Running log of known gaps, scope-cut trade-offs, and future work that
wasn't picked up in the current milestone. Not a roadmap — these are
things I'd want a future maintainer (or future me) to see *before*
assuming something's missing on accident.

Last synced: M9 landed (2026-04-18).

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

M7 shipped `FilePolicyGate` + `--policy` wiring. M8 extended coverage
across MCP, HTTP, Lens. M9 closed the loop with ratification. Status:

- `agentstategraph-policy` crate still not built (design in
  [POLICY_V1.md](/Users/user/Documents/AgentStateLabs/strategy/POLICY_V1.md)).
  Our `FilePolicyGate` is interim; schema is a subset so migration is
  a rename.
- **No selector DSL** — `match_action` is exact or prefix-`.*` match,
  plus optional `agent_id` equality. POLICY_V1 envisions richer
  conditions (paths, timestamps, qualifiers). Expand when real use
  cases arrive.
- **Hot reload** — policy file is loaded once at engine open. Changes
  require restart.
- **No policy coverage over**: traces (`asd trace` ingest), index
  (`asd index` writes), merge (no merge surface yet), rename. Today
  the gate fires on ledger append/approve and effect_declare only.

### Ratification (M9) — remaining gaps

- **No reject/deny action** — reviewers can approve awaiting entries
  but can't explicitly reject them. Currently the workaround is "don't
  approve" — the entry just sits in the queue forever.
- **No revoke** — once approved, it stays approved. Security-adjacent
  scenarios (approver credentials compromised) need a revocation path.
- **No approval rationale** — `approved-by:alice` tells you who, not
  why. Adding a `--message` on approve + a `approval-note:<text>` tag
  or a dedicated field would close that.
- **No cryptographic signing** — `approved-by:alice` is only as
  trustworthy as the author_id on the commit. For enterprise, signed
  approvals (ed25519 or similar) are table stakes.
- **No supersede at any surface** — schema has `supersedes: [entry_id]`
  but `asd ledger` only exposes `append` + `approve`. To retract/
  replace a ledger entry you'd need to write a new one with
  supersedes set manually. MCP/HTTP/CLI all lack this.

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

M6 added via-link resolution + mismatch banner. M8 added the
awaiting-approval badge + matched_policy chip + `/approvals` queue.
M9 added the Approve button on the queue. Remaining gaps:

- **No reject / withdraw-approval buttons** on the queue — pairs with
  the ratification gaps above.
- **No cross-module graph visualization** — edges are exposed via API
  but rendered as flat lists, not a graph.
- **No effect-distribution overview route** (e.g., "which 10 symbols
  have the biggest transitive blast radius?"). Top-N by declared +
  transitive count, filterable by category.
- **No effect_declare UI** — effect decls come from `asd index`'s
  static inference or MCP writes only. Humans can't edit effects
  without going through MCP tool calls manually.
- **No policy authoring UI** — policy files are JSON edited by hand.
  POLICY_V1 has a proposal/ratify UX; we've not surfaced anything.
- **No "who approved what, when" timeline view** — approval history is
  embedded in tags on each entry but not rendered as a flat reviewer
  log anywhere.

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
  promise for cold clones. **M10 target.**
- No audit-log export / event stream. Approvals + denials should be
  stream-able to SIEM (Splunk, Datadog, etc.) for enterprise
  compliance. Today they're observable via `get_tree` on the ledger
  path only.
- Traces carry a `started_at`/`finished_at` but no per-call timing or
  call-depth info.
- No schema migration story on disk. Everything lives under
  `/asd/v1/` so a v2 can sit alongside, but there's no migration tool.

## Working-style

- Two sub-agents across M1–M5 hit sandbox permission denials on
  `cargo` / `npm` and returned without self-verifying. I took over
  each time. If this becomes a pattern, either relax agent sandbox
  allowlists or stop asking agents to self-verify builds.
