# ASD Troubleshooting

The errors people actually hit, and the one-line fix for each. Grouped by
when they show up.

---

## Install & PATH

### `asd --version` shows an old version after upgrading

Almost always a **PATH collision**: you have two copies of `asd` installed
in different roots, and PATH resolves the stale one first.

```bash
which asd            # which binary is actually winning
which -a asd         # every asd on PATH, in resolution order
```

`cargo install --path … --root ~/.cargo` installs to the user-default
location PATH normally resolves first; `--root ~/.local` installs to a
secondary location. Both can exist. Fix by removing the stale one, or
reorder your PATH so the intended root wins. (Homebrew installs to its own
prefix — if you switch from source to `brew`, remove the source build.)

### `brew update` says "Skipping … because it is not trusted"

Recent Homebrew requires explicit trust for third-party taps:

```bash
brew trust agentstatelabs/agentstatedeveloper
```

Without it the initial install still works, but `brew upgrade` becomes a
silent no-op — which looks exactly like the stale-version symptom above.

### Windows: `asd` not found in a new terminal

The installer adds `%LOCALAPPDATA%\asd\bin` to your **user** PATH, but an
already-open shell won't see it. Open a fresh PowerShell after installing.
ARM64 Windows isn't a native target yet; the x86_64 binaries run under
emulation.

---

## Indexing & freshness

### Results look stale or a symbol is "missing"

The index is derived state — if source changed since the last `asd index .`,
it's out of date.

```bash
asd status     # index age, symbol count, dirty files
asd trust      # single rollup: is the index fresh enough to rely on?
asd index .    # rebuild
```

To stop this happening, run `asd watch` (auto-reindex on change) or rely on
the post-merge / post-checkout hooks that `asd init` installs.

### `asd references` returns nothing / errors

`references` shells out to **ripgrep** for rg-style completeness. Install
`rg` and make sure it's on PATH. (`asd search` doesn't need it — it uses the
FTS index.)

### A fresh clone has no ledger history

The local db is gitignored, so a clone starts empty until you import the
committed sidecar:

```bash
asd onboard    # init → index → conclusions import, in the right order
```

If you ran `asd init` + `asd index .` by hand and the judgment is still
missing, you skipped `asd conclusions import`.

---

## MCP & agent wiring

### The agent doesn't see ASD's tools

1. Confirm registration: `asd mcp status` (per-tool).
2. **Restart the agent** — MCP servers are read at startup; a config write
   mid-session won't take effect until the tool reloads.
3. Re-register a single tool if needed: `asd mcp install --tool cursor`.

### The agent connects to the wrong database

`asd-mcp` reads `ASD_DB` to pick the project db; `asd mcp install` writes it
into the env block. If you moved the repo or use a non-default path:

```bash
asd mcp install --db /abs/path/to/.asd-state.db
```

### `asd search --agent` output breaks a `jq` pipe

Fixed in 1.0.64 — empty results now emit valid JSON in `--agent` mode. If
you still see plain text on empty results, you're on an older build; upgrade
(and see the PATH-collision section — you may be running a stale binary).

### A doc/command example references a path that doesn't exist

Older builds referenced some resources by a CWD-relative path that only
resolved from a source checkout. These are now embedded in the binary. If an
example fails only from a non-source directory, upgrade.

---

## Data & integrity

### "Did my sidecar commit?"

The pre-commit hook runs `asd conclusions export` automatically. Verify:

```bash
asd hooks                       # are the hooks installed / active?
git status .asd/conclusions/    # anything staged/changed?
```

If hooks aren't active, `asd init` (idempotent) reinstalls them and sets
`core.hooksPath`.

### The db looks corrupted or has stale edges

```bash
asd repair          # read-only scan for orphaned refs / stale edges
asd repair --fix    # apply safe auto-corrections
```

Worst case, the db is fully regenerable: delete `.asd-state.db`, run
`asd index .`, then `asd conclusions import` to restore judgment from git.

---

## When in doubt

`asd trust` is the fastest single check — it tells you whether the index is
fresh, the sidecar is in sync, and the ledger is dense enough to be worth
consulting for the current task. If Trust is low, reindex before leaning on
any analysis command.

Still stuck? Team/Enterprise support: `licensing@agentstatelabs.com`.
