---
title: TypeScript
description: How the tree-sitter-typescript adapter parses .ts and .tsx, infers effect categories, and resolves cross-module call edges through imports.
---

`agentstatedeveloper-typescript` is the TypeScript / JavaScript language
adapter. It handles `.ts`, `.tsx`, `.mts`, `.cts`, `.jsx`, `.js`, `.mjs`,
and `.cjs` files through tree-sitter-typescript, dispatching to the TSX
grammar for `.tsx` / `.jsx` and the plain TypeScript grammar otherwise.

## Parsing

The walker enumerates:

- `function_declaration` → `Function`
- `generator_function_declaration` → `Function`
- `method_definition` (inside a `class_body`) → `Method`
- `class_declaration` → `Class`
- `lexical_declaration` / `variable_declaration` where the initializer is an
  arrow function or function expression → `Function`
- `export_statement` wrappers — unwrapped to the inner declaration
- `internal_module` / `module` (i.e. `namespace Foo { ... }`) — walked with
  the namespace pushed onto scope; the namespace itself is not emitted

qname derivation strips the file extension and joins path segments with dots:
`src/payments.ts` → `src.payments`; `src/payments.ts`'s `chargeCard` becomes
qname `src.payments.chargeCard`. Nested anonymous arrow functions inside
function bodies are intentionally skipped.

Explicitly not emitted:

- **Interfaces** (not callable)
- **Type aliases** (not callable)
- Nested arrow-functions-in-variables inside function bodies (would be
  anonymous and noisy)

## Effect inference

Effects detected by the TypeScript adapter:

| Category | TypeScript triggers |
|---|---|
| `log` | `console.log`, `console.info`, `console.warn`, `console.error`, `console.debug` |
| `io.fs.read` | `fs.readFile`, `fs.readFileSync`, `fs.createReadStream`, `fsPromises.readFile` |
| `io.fs.write` | `fs.writeFile`, `fs.writeFileSync`, `fs.appendFile`, `fs.createWriteStream`, `fsPromises.writeFile` |
| `io.net.out` | `fetch(...)`, `axios.*`, `http.request`, `https.request`, `http.get`, `https.get` |
| `io.db.read` / `io.db.write` | `db.query(...)`, `conn.query(...)`, `client.query(...)` (similar receivers to Python's). Classification by SQL prefix — `SELECT` / `WITH` / `SHOW` → read; `INSERT` / `UPDATE` / `DELETE` / `REPLACE` / `CREATE` / `DROP` / `ALTER` / `TRUNCATE` → write |
| `proc.spawn` | `child_process.spawn`, `child_process.exec`, `child_process.fork`, `child_process.execSync` |
| `env.read` | `process.env.X` references |
| `time.sleep` | `setTimeout(...)`, `setInterval(...)` |
| `time.read` | `Date.now`, `new Date(...)`, `performance.now` |
| `random` | `Math.random`, `crypto.randomBytes`, `crypto.randomUUID` |
| `throw` | `throw` statements |

Each effect carries the matching source line as `note`. Where extractable,
qualifiers include paths (for `fs.*`), hosts (for `fetch` / `axios` URL
literals), and variable names (for `process.env.*`).

## Call edge extraction

**Intra-module:** identifier calls and method calls within the same file.

**Cross-module** via resolved imports:

- `import { foo, bar } from 'mod'` — `foo()` and `bar()` bind to qname
  `mod.foo` and `mod.bar`
- `import { foo as f } from 'mod'` — `f()` binds to `mod.foo`
- `import foo from 'mod'` — default import; `foo()` binds to `mod.default`,
  with `foo.something()` resolving against `mod` if `mod` has a
  matching export
- `import * as ns from 'mod'` — namespace import; `ns.foo()` binds to
  `mod.foo`
- `import foo, { bar } from 'mod'` — both default and named bindings

Relative specifiers (`'./mod'`, `'../pkg/mod'`) are resolved against the
importing file's path. Non-relative specifiers ( `'react'`, `'fs'`) stay as
the raw module name — external packages aren't walked.

Scope cuts:

- **Re-exports** (`export { X } from 'mod'`) — skipped.
- **Default import resolved to the module handle only** — `foo.something()`
  off a default import is best-effort; if the module has a matching export,
  it binds, otherwise dropped.
- **Dynamic `import()` expressions** — out of scope.
- **CommonJS `require(...)`** — not currently resolved.
- **Type-only imports** (`import type { X } from 'mod'`) — harmless because
  types aren't call sites, but the import isn't walked for edges either.

Unresolved calls are silently dropped.

## What's not here

- No TypeScript runtime tracer yet. Effect declarations flip `verification.by:
  static-checker` with `status: unverified` and stay that way until a TS
  tracer lands. The `verify-effects` CLI command prints the declared set
  verbatim.
- Decorators are not inspected.
- Class field initializers are not parsed for calls.

For files that parse cleanly but produce odd edges (common after aggressive
barrel exports or index-re-export patterns), cross-check with `asd read` on
a symbol you expect to call something — if the `transitive` set is empty
where you expected edges, the re-export skip is likely why.
