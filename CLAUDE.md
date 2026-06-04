# CLAUDE.md — project conventions for agents

Load-bearing rules I want every future agent (and future me) to
read before touching the codebase. Kept short by design — anything
that grows beyond ~5 entries belongs in DESIGN.md as a Plan-level
discussion, not here.

---

## Calibration tables: stare at the predictor first

**When writing a calibration / advisory table for a label scheme
(buckets like `low/medium/high`, `core/peripheral/noisy`, etc.),
read the predictor's threshold ladder before assigning expected
pass rates.**

ASD has two bucket axes with OPPOSITE directions:

- **Uncertainty axis** (`uncertainty.level` from
  `core::candidates::compute_uncertainty`): `low` means LOW
  uncertainty = HIGH confidence in the result.
  `critical` means HIGHEST uncertainty.
  → A `low` bucket at 95% pass rate is well-calibrated, NOT
  miscalibrated.

- **Quality axis** (`result_bucket` and recovery estimation):
  `core` / `strong` = good result. `noisy` / `weak` = bad result.
  → A `noisy` bucket at 95% pass rate IS miscalibrated.

The 1.0.59 → 1.0.68 calibration arc burned four rounds of field
data before catching that `crates/agentstatedeveloper-core/src/
calibration.rs::bucket_advice` had the uncertainty axis
inverted — it assumed "low" meant "low quality" instead of "low
uncertainty". Every unit test passed because they encoded the
same wrong assumption. Only real ExampleProj data exposed it.

When you add a new label scheme to ASD or a new predictor that
emits one, write down the table's expected pass rates as comments
NEXT TO the predictor's threshold ladder, and have both reviewed
together. Synthetic tests can't catch axis inversion; real
distributions can.

See DESIGN.md Plan J t-015 for the full four-round arc.

---

## Field-test loop ships UX bugs you can't synthesize

Several rough edges in this session only surfaced when the
binary was actually run on a different repo (ExampleProj):

- `asd think bootstrap` told users to run `asd reindex` — no
  such CLI subcommand until 1.0.62 added the alias.
- `commands/think.rs:283` referenced `docs/initial-read-prompt.md`
  by a CWD-relative path that failed from any non-source
  checkout; fixed in 1.0.61 via `include_str!`.
- `asd search --agent` printed plain text on empty-results,
  breaking jq pipes; fixed in 1.0.64.
- `--paths` is a no-op for results in `asd search` (filed as
  Plan J t-017 — not yet fixed).

Pattern: command examples in help text, JSON contracts in
`--agent` mode, and CWD-relative paths are the three classes
where local-development testing won't surface the issue. The
fix lives in Plan J t-018 (verify shell-command examples in CI).

Until t-018 lands, when adding ANY user-facing string that names
a CLI subcommand or path, smoke-test it from a different repo
checkout before merging.

---

## Worktrees + cargo install

This repo uses git worktrees (`.claude/worktrees/<name>`). The
Bash tool's working directory resets to the active worktree
between invocations, so:

- `cargo install --path crates/agentstatedeveloper-cli --root
  ~/.cargo --force --locked` installs to the user-default
  location that PATH normally resolves first.
- `--root ~/.local` installs to a secondary location. Both may
  exist; `which asd` tells you which one wins. See the
  PATH-collision incident in session log around 1.0.60 if
  you're debugging a "version didn't update" report.
