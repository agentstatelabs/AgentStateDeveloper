---
title: Python
description: How the tree-sitter-python adapter parses symbols, infers 17 effect categories, resolves cross-module call edges, and how the runtime tracer verifies them.
---

`agentstatedeveloper-python` is the Python language adapter. It implements the
`LanguageAdapter` trait from core and is paired with the runtime tracer at
`tools/asd_tracer.py` for observed-effect verification.

## Parsing

tree-sitter-python parses every `.py` file. The walker enumerates:

- `function_definition` → `Function` (at module scope) or `Method` (inside a
  class body)
- `class_definition` → `Class`

Nested functions inherit their enclosing function / class names in their
qname — `payments.Payment.refund`, for example. Nested functions inside
other functions are walked and emitted.

`qname` is derived from the file path: `foo/bar.py` becomes module prefix
`foo.bar`. Symbols then extend that: `foo.bar.my_fn`, `foo.bar.MyClass.method`.

## Effect inference

Every parsed symbol's body is scanned for known patterns. Effects are
substring-based (not full AST matching) — pragmatic for M1, and the runtime
tracer is the ground truth anyway. The 17 categories emitted:

| Category | Python triggers |
|---|---|
| `io.fs.read` | `open(...)` — path extracted into qualifiers when a literal |
| `io.fs.write` | `open(..., mode)` where `mode` contains `w` or `a` |
| `io.net.out` | `requests.`, `urllib.`, `httpx.`, `aiohttp.` — host extracted when present |
| `io.db.read` | `db.execute("SELECT...")`, `.execute("WITH...")`, `.execute("SHOW...")` (receivers: `db`, `conn`, `cursor`, `cur`, `session`, `c`, `self.db`, `self.conn`) |
| `io.db.write` | `db.execute("INSERT/UPDATE/DELETE/REPLACE/CREATE/DROP/ALTER/TRUNCATE...")` plus any `.commit()` |
| `env.read` | `os.environ`, `os.getenv(...)` — variable names extracted |
| `time.read` | `time.time`, `time.monotonic`, `datetime.now` |
| `time.sleep` | `time.sleep(...)` |
| `random` | `random.*`, `secrets.*` |
| `proc.spawn` | `subprocess.*`, `os.system(...)`, `os.exec*` |
| `throw` | `raise` statements |
| `log` | `print(...)`, `sys.stdout`, `sys.stderr`, `logging.*`, `log.*`, `logger.*` |

Each detected `Effect` carries the matching source line as `note` and, where
extractable, `qualifiers` like `{"paths": ["./config.json"]}`,
`{"hosts": ["api.example.com"]}`, `{"vars": ["HOME"]}`.

Honest limits:

- Pattern matching is substring-based. A literal string `"requests."` inside
  a docstring produces a false `io.net.out` positive. Impact is bounded —
  the runtime tracer verifies or rebuts.
- SQL classification is prefix-based on `.execute(` args. CTEs (`WITH`) are
  classified as reads but don't distinguish `WITH ... INSERT` patterns.

## Call edge extraction

Pass 2 of `asd index` walks every parsed symbol's body and extracts call
sites. Supported forms:

**Intra-module** (within the same `.py` file):

- `identifier(...)` — resolves to a module-level function
- `self.X(...)` / `Class.X(...)` — method calls with the enclosing class as
  scope

**Cross-module** (through imports):

- `import foo` — calls to `foo.fn(...)` bind to qname `foo.fn`
- `import foo as f` — calls to `f.fn(...)` bind to qname `foo.fn`
- `from foo import bar` — calls to `bar(...)` bind to qname `foo.bar`
- `from foo import bar as b` — calls to `b(...)` bind to qname `foo.bar`
- `import foo.bar` — calls to `foo.bar.X(...)` bind via the leading segment

Only module-scope imports are scanned. Imports inside function bodies or
conditional blocks are skipped.

Unresolved call sites are silently dropped — they don't emit edges but don't
error either.

Scope cuts (intentionally out of scope for M5):

- `from foo import *` — unresolvable statically
- Relative imports (`from . import x`, `from ..pkg import y`)
- Multi-segment attribute chains beyond the first (`foo.bar.baz()` resolves
  only via `import foo.bar`; `import foo` alone won't bind it)
- Dynamic dispatch — `getattr`, attribute computed at runtime, callbacks
  passed as arguments
- Module-scope call sites — the walker only enters function / method bodies.
  Top-level `main()` is required for script-style code to produce edges.
  The sample `_driver.py` demonstrates this pattern.

## Runtime tracer

`tools/asd_tracer.py` is a standalone Python script, invoked via
`asd trace -- <cmd>`. It installs `sys.settrace` on the current thread and
monkey-patches a pragmatic slice of stdlib entry points:

- `builtins.open` / `builtins.print`
- `logging` module methods
- `time.sleep`, `time.time`, `time.monotonic`, `datetime.now`
- `urllib.request.urlopen`, `http.client` basics
- `os.environ.__getitem__`, `os.getenv`
- `subprocess.Popen`, `os.system`, `os.execv*`
- `random.*`, `secrets.*`

On program exit, the tracer writes a JSON report keyed by qname. `asd trace`
ingests it, creates `Trace` records under `/asd/v1/traces/`, and updates each
touched symbol's `verification` block. Declared-but-not-observed or
observed-but-not-declared produces a `mismatch` diagnostic; otherwise `ok`.

```bash
asd trace -- python _driver.py
```

```json
{
  "exit_code": 0,
  "traced_qnames": 4,
  "updates": [
    { "qname": "payments.charge_card", "status": "ok", "mismatches": [] }
  ]
}
```

Scope cuts:

- **Single-process.** Effects inside a `subprocess.Popen`-launched child
  aren't instrumented; only the parent's `Popen` call is recorded as
  `proc.spawn`.
- **Single-thread.** `sys.settrace` installs on the current thread only.
  Worker threads run untracked.
- **Pragmatic monkey-patches.** Raw sockets, memory-mapped I/O, async/await
  variants of urllib/http.client are not wrapped.

For deeper instrumentation, pair the tracer with `strace` / `dtrace` /
auditing frameworks at the OS layer — ASD's tracer is explicitly scoped to
what a solo developer can stand up locally.
