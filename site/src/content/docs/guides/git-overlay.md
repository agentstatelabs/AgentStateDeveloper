---
title: Git+ Overlay Model
description: How ASD layers on top of git without replacing it. Three-tier storage, the .asd/ sidecar, and the cold-clone hydrate contract.
---

ASD does not ship a new version-control system. Your code lives in git; your
reviews happen in the tool you already use; your CI runs as it always did.
ASD is an **overlay**: a structured record of the context that diffs can't
carry, written to files that travel inside your git repository.

## Three-tier storage

Everything ASD persists falls into one of three tiers. The tiering is
deliberate — it determines what a fresh `git clone` recovers.

**In git** (`.asd/v1/` directory, committed):

- `.asd/v1/effects/<qname>.json` — declared effects per symbol
- `.asd/v1/ledger/<qname>/<entry_id>.json` — ledger entries (non-superseded by
  default)
- `.asd/v1/meta/schema-version` — schema marker

**In ASG** (local SQLite at `./.asd-state.db` by default):

- Live authoring state
- Raw trace records
- Supersede chains prior to summarization
- Transitive effect caches
- Per-edit intent / confidence / authority

**Never persisted, always rebuilt:**

- Semantic index (symbols, qname → symbol_id)
- Call graph
- Effect verification results

## Round-trip: `asd sync` → commit → `asd hydrate`

Two CLI verbs move state between ASG and the sidecar:

### Sync: ASG → filesystem

```bash
asd sync
```

```json
{
  "dir": "./.asd/v1",
  "effects_written": 13,
  "ledger_entries_written": 2,
  "symbols_written": 13,
  "schema_version": "0.1.0",
  "note": "current-state only; ASG commit history is not carried in the sidecar"
}
```

`asd sync` mirrors current ASG state into `.asd/v1/` — one file per symbol,
one file per ledger entry. You then `git add .asd/` and commit; the audit
layer travels with the code.

### Hydrate: filesystem → ASG

After `git clone`:

```bash
asd init
asd hydrate
asd index .    # rebuild the semantic index + call graph from source
```

```json
{
  "dir": "./.asd/v1",
  "effects_loaded": 13,
  "ledger_entries_loaded": 2,
  "symbols_loaded": 13,
  "missing_schema_version": false,
  "note": "commit history not restored; run `asd index` to rebuild the semantic index and call graph"
}
```

The machine now has a working ASG populated from the committed sidecar.

## What survives a cold clone

**Survives:**

- All declared effects
- All non-superseded ledger entries (decisions, hazards, rationales, approvals,
  rejections, withdrawals)
- `matched_policy` stamps on each entry
- Schema version

**Lost:**

- Supersede chains older than what's summarized in current entries (the
  non-superseded ones)
- Per-edit intent / confidence / authority that was held in-ASG
- Raw runtime traces (regenerable by re-running `asd trace`)
- Commit history of the ASG itself (you still have git's commit history of
  the `.asd/` directory — the sidecar is file-per-entry so text-diff review
  on GitHub is legible)

The deliberate honesty: if you want full fidelity across machines — every
speculative branch, every intermediate supersede, per-edit provenance — you
need an ASG registry. Without one, you still get everything load-bearing.

## Why file-per-entry

Each ledger entry is its own file. Each effect declaration is its own file.
Consequences:

- Concurrent agent work on different symbols produces **zero merge conflicts**.
- Supersede never mutates an existing file — only writes a new one. So
  supersede never conflicts with a concurrent edit.
- `git diff` of a PR shows you exactly which audit records were added /
  modified / removed, legibly.
- Same-symbol same-field effect edits *can* conflict, but that's the right
  failure mode: two agents proposing different declared effects for the same
  function is a genuine disagreement worth surfacing.

## Rename handling

Two cases:

**ASD-aware rename** (agent or human uses an ASD tool to rename): a rebind
record is written, canonical `symbol_id` is preserved, ledger entries follow
the symbol without re-linking.

**Out-of-band rename** (text-edit directly): next `asd index` sees a new
qname with no rebind record. Ledger entries on the old qname become orphaned;
they're not deleted but they no longer surface on reads of the new symbol.
A heuristic matcher is planned but not yet implemented; today the workaround
is "rename through `asd` when you can."

## Why this matters

Overlay-not-replacement is the positioning that lets teams adopt ASD
without renegotiating their tooling. Keep using GitHub. Keep using whatever
your CI is. Reviewers see `.asd/` files diff alongside source in every PR —
they can read the audit layer at review time without leaving the platform
they already know.
