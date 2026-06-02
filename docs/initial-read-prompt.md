# Initial-read prompt for the agent

You're stepping into a codebase. This prompt gives you a structure for the
first read so the thinking you do here doesn't have to be redone next
session. The goal is **durable capture** — every claim, model, failed
attempt, and open question goes into ASD via `asd think` so the next
agent (or you, tomorrow) inherits it without re-deriving.

ASD does not call an LLM. You do the reading and reasoning; ASD provides
structure for what to capture and the write commands.

---

## Before you start

1. **Index the project** if it hasn't been already:
   ```sh
   asd index .
   ```
2. **Seed structural tags** with the cheap pass:
   ```sh
   asd map
   ```
   This walks every symbol and writes `Ownership` entries with
   `package-boundary` / `fast-test` / `diagnostic-test` role tags. It's
   the substrate everything below builds on.
3. **Set your task scope** so writes get auto-tagged:
   ```sh
   export CTX_ACTIVE_TASK='{"task_id":"initial-read-2026-06-02","scope":["."]}'
   ```

---

## Section 1 — Architecture

What are the major subsystems? How do they connect?

Don't enumerate every file. Pick the 3–7 nodes that, if you understood
them well, would let you place any file in the project. Record each as
a **MentalModel**:

```sh
asd think model "<short-name>" \
  --symbols pkg.subsys.entry1,pkg.subsys.entry2,pkg.subsys.entry3 \
  --summary "<one sentence on what this subsystem does + how data flows>"
```

**Bias toward fewer, better models.** One model per major subsystem.

---

## Section 2 — Hot spots

What files are likely touched often? Look for:
- Files at junction points (mentioned by many callers).
- Files with recent dense git churn (`asd since <sha-N>` if you know a
  baseline; otherwise `asd status` for index health).
- "God objects" — large symbols with many effects.

Record each as a **Concept** (already a queryable kind) with a hint:

```sh
asd ledger append <qname> --kind concept \
  --summary "hot spot: changes here ripple through <list>"
```

---

## Section 3 — Implicit constraints (invariants not written down)

What does the code assume that isn't documented? Examples:
- "this function must be called before X" — sequencing
- "this map is never reentrant" — concurrency
- "input is always ASCII" — encoding
- "this struct may grow but never shrink" — schema migration

For each: record as **Invariant** (decision-class) **AND** an
**OpenQuestion** if you're not sure:

```sh
asd ledger append <qname> --kind invariant \
  --summary "must be called before <other> — depends on …"

asd think question <qname> --q "is this invariant load-bearing? \
  what breaks if violated?"
```

---

## Section 4 — Open questions

Magic numbers, unexplained patterns, dead-looking code that might not be.

```sh
asd think question <qname> --q "what does magic value <X> mean?"
asd think question <qname> --q "is this dead code? no callers found"
```

**Be generous here.** Every question recorded is a question the next
session doesn't have to re-ask.

---

## Section 5 — Hypotheses with confidence

Things you suspect but haven't verified. Confidence is `0.0–1.0`:

- `0.9` — you're very confident; one test away from a Decision
- `0.6` — informed hunch; evidence points this way
- `0.3` — speculation; could go either way
- `0.1` — wild guess worth recording

```sh
asd think speculate <qname> --conf 0.7 \
  --summary "I think <X> causes <Y> because <observation>"
```

**Anything below `0.3`** is excluded from auto-surface by default —
useful for noting "I considered X" without polluting future
prepare-change output.

---

## Section 6 — Failed-path warnings

Patterns that look reasonable but don't work. **Negative evidence is
expensive to produce and easy to lose** — record it.

```sh
asd think failed <qname> --tried "<approach>" \
  --because "<why it didn't work>"
```

Examples worth recording:
- "Tried wrapping in async; broke because the caller expects sync."
- "Tried caching the result; broke under concurrent invalidation."
- "Tried the obvious refactor; broke a test you wouldn't expect."

---

## Section 7 — Commit your thinking

When done, export so it commits with the repo:

```sh
asd conclusions export
git add .asd/conclusions/
git commit -m "initial read: capture project mental model + open questions"
```

The thinking now travels with the repo. Next session that runs
`asd conclusions import` (or `git pull` with the post-merge hook
installed) gets your model + questions + warnings without re-deriving.

---

## Verifying the capture worked

```sh
# Show what you captured (filter by class or kind).
asd think list

# Confirm prepare-change now surfaces your work:
asd prepare-change "<a query relevant to one of your hypotheses>"
# The response should include a `prior_thinking` section with your
# hypothesis/model/question.
```

---

## Worked example — examples/sample-py-repo

Run against the small fixture project that ships with ASD:

```sh
cd examples/sample-py-repo
asd index .
asd map

# Mental model — payments module
asd think model "payments-pipeline" \
  --symbols payments.charge_card,payments.refund,payments.audit \
  --summary "charge_card writes to audit + posts to gateway; refund reverses but does NOT touch audit"

# Open question — non-obvious behavior
asd think question payments.audit \
  --q "why does refund skip audit? is this intentional or a bug?"

# Hypothesis — performance suspect
asd think speculate payments.audit --conf 0.6 \
  --summary "audit writes are synchronous; this is likely the bottleneck under load"

# Failed attempt — record what didn't work
asd think failed payments.refund \
  --tried "make refund call audit symmetrically" \
  --because "broke test_refund_does_not_double_count — auditing is suppressed by design"
```

Run `asd prepare-change "refund flow"` — your model + question + failed
attempt now show up in `prior_thinking`.

---

## Re-running

The prompt is meant to be re-runnable. `asd think bootstrap --check`
(Plan G t-005) reports gaps: "no hypothesis on payments.refund yet",
"mental model 'payments-pipeline' exists, others missing", etc. Use it
as a checklist on subsequent sessions; the entries you already wrote
stay.
