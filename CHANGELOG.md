# Changelog

All notable changes to AgentStateDeveloper are documented here.
Versions use semantic versioning.

> **Note on numbering.** Releases up to `v1.3.1` used an earlier scheme that
> was never tagged in this repository. Commit `b332c4a` renumbered the
> workspace `1.3.1 -> 0.9.38` so asd would line up with the rest of the suite
> ahead of a coordinated `1.0.0`. Version numbers therefore go *down* between
> `v1.3.1` and `v0.9.38` below. The older entries are kept: the work is real,
> only its numbering was abandoned.
>
> `v1.4.0` follows `v1.2.1`: the `1.3.x` range is skipped so that no version in
> the current scheme shares a heading with one from the old.

---

## [Unreleased]

## [v1.4.3] — 2026-10-03

### Fixed
- **The symbol index only ever grew, and ledger entries came loose from the code they described.** `asd index` added and updated symbols but never removed one, so the index kept every name a symbol had ever had: SessionDrift-ios held 12,044 entries for 9,244 live symbols. Most of the excess was not deleted code but line-number churn — same-named symbols in a file are told apart by line (`load:412`), the line is part of the symbol's id, so an edit above them gave each a new id — and a ledger entry stayed on the old id, out of the reach of `prepare-change` and of conclusions export. Each run now settles the stale entries in the files it parsed (and, on a run over the whole project, in files that no longer exist): a symbol whose body reappears unchanged under a new id in the same file has moved, and its ledger entries and any effects record go with it; a symbol that is gone is removed with its effects and code entries — unless ledger entries or runtime evidence still hang off it, in which case it stays as before. A hand-declared effect on a symbol that is gone (not merely moved) goes with it. A partial run (`asd index src`) only touches the files it parsed. On a copy of SessionDrift-ios the first run moved 1,628 symbols (carrying 270 ledger entries) and removed 890, leaving 9,526 entries — the live symbols plus 282 kept for their ledger; conclusions export went from 11,128 records to 11,398, losing none; later runs found nothing stale and re-index time went from 19.2 s to 17.2 s. The index summary reports `stale_pruned`, `stale_rebound`, `ledger_entries_rebound` and `stale_kept`.

## [v1.4.2] — 2026-10-02

### Fixed
- **`asd repair --fix` multiplied the store's size when it restored many ledger entries.** It wrote two commits per restored entry — the entry, then its reverse-index record — and each stored a fresh copy of the ledger and ledger-index maps: restoring SessionDrift-ios's 1,684 lost entries grew its store from 1.5 GB to 5.5 GB (`asd gc --sweep --vacuum` took it back to 1.5 GB). A restore is now one commit, with every restored entry id in its reasoning. The same restore on a copy of that store now adds 7 MB and takes 7 s instead of about 50 s.
- **`asd index <dir>` run from another directory indexed into a store there, not the project's.** Without `--db`, `index` always used `./.asd-state.db`, so `asd index SessionDrift-ios` run from the parent `Apps/` built and registered a new store in `Apps/` and left the project's own store as it was. It now uses the indexed directory's store when it has one, keeps `./.asd-state.db` for the current directory or a subdirectory of the current project (as before), and otherwise creates the store in the directory being indexed rather than where `asd` ran. The JSON summary names the store it used (`db`), like `sync` and `hydrate`. An explicit `--db` or `ASD_DB` still wins.

## [v1.4.1] — 2026-10-02

### Fixed
- **The tag pipeline can no longer starve its own tap mirror.** `verify-install` polled the GitHub tap for up to 1800s while holding the GitLab instance's only runner — the runner the tap project's `publish-github` job needed to put the formula there. The mirror could not land until the poll gave up, so the wait always ran out, and v1.4.0 stalled for over an hour. The v1.2.0 "mirror latency" of 11m43s that the 1800s budget was based on was mostly this queueing. The formula check is now its own `verify-formula` job, delayed 10 minutes with `when: delayed`, which waits without holding a runner; its remaining wait is 300s and still reports an exhausted budget apart from a wrong formula. `verify-install` keeps the shell-installer and docs checks and no longer waits. Every `curl` in `verify-release.sh` is now time-bounded, and with a token the wait loop polls the authoritative API rather than the lagging CDN. `site-version` now waits for both `verify-install` and `verify-formula`, so the site still only advertises a release proven installable by both the shell installer and Homebrew.
- **Ledger entries were silently lost from the store while every surface kept showing them.** On one store 1,684 of 12,085 entries — decisions, invariants, proofs, validation scenarios — were in `asd_ledger_cache` but gone from the ASG ledger tree, so `asd sync` could not export them, `asd hydrate` could not restore them, and `asd status` still said "hydrated". `sync_to_dir` was not dropping anything; the store was. AgentStateGraph moved refs unconditionally, so concurrent writers (parallel agent processes, the MCP server, git-hook re-indexes) discarded each other's commits, and every `asd index` speculation commit reverted ledger writes made while it was open — after `append_entry` had already returned `Ok` and written the cache. Fixed at the source in AgentStateGraph v1.2.5 (compare-and-swap ref writes; speculations merge onto the current head), which this release pins. Two new tests reproduce both paths: four concurrent writers lost 35 of 48 acknowledged entries before the fix, and none after.
- **`asd hydrate` could load one project into another project's store.** Without `--db`, read commands fall back to a parent directory's store and then the registry's active repo — and `hydrate` shared that fallback, though it writes. Run in a directory holding only a copied `.asd/` sidecar, it loaded that project's 12,003 symbols and 12,085 ledger entries into the active repo, a different project, and re-warmed its caches. `hydrate` now resolves like `init`/`index`: it fills the `.asd-state.db` beside the sidecar it reads (`./`, or `<dir>/` with `--dir`), never a parent's or the registry's; an explicit `--db`/`ASD_DB` still wins. `asd sync` had the mirror-image fault — it exported a fallback-resolved store into whatever directory it ran from, including a subdirectory of the right project — and now defaults `--dir` to the store's own directory, as the MCP `sync` tool already did. Both print the store they used (`db` in their JSON).
- **Re-indexing replaced declared effects with the parser's inference, and threw away runtime evidence.** Every `asd index` — so every commit, through the post-commit hook — rebuilt each re-parsed symbol's effects from scratch: a list recorded with `effect_declare` was replaced by whatever the adapter inferred, and a runtime-trace verification, the confidence accumulated from traces and the matched policy were dropped. The index now refreshes only what it inferred. Inferred effects carry the adapter that inferred them (`adapter`); a declaration carries none and stands, with the fresh inference recorded against it as `declared_not_inferred`/`inferred_not_declared` mismatches — the ones `verify-effects` reports. Runtime and test verifications, `runtime`, `confidence`, `matched_policy` and transitive effects are kept. Effects stored by an earlier release carry no `adapter` either: a list identical to the current inference is taken as inferred, and any other is kept as a declaration — so an inference that has since gone stale shows up as a mismatch rather than a declaration being dropped; declare the right list to clear it. `effect_declare` strips `adapter` from what it is given.
- **`asd index` reverted writes that landed while it ran.** It read the stored symbols, effects and code tree before parsing and wrote that copy back whole at the end, so anything written in between — an `effect_declare`, a runtime trace, `verify-effects --write`, another index — was reverted, for every symbol, not only the ones being re-parsed. The transitive-effects pass did the same to the whole effects tree. Both now write their changes on top of the state their speculation forked from, so the commit — a merge onto the current head since AgentStateGraph v1.2.5 — keeps every write to anything the index did not itself change. A re-index no longer changes an unchanged symbol's effects at all: a static verification keeps its time while its verdict holds.
- **`asd conclusions import` reverted entries revised since the last export.** The post-merge and post-checkout hooks run it on every pull and branch switch, and it re-appended every committed record unconditionally, rolling an `asd think` re-run, or an approval, rejection or withdrawal, back to the committed copy. A record now replaces the stored entry only when it is newer, judged by `created_at` and the `*-at:` tags that in-place revisions add (`approved-at:`, `withdrawn-at:`, …); a tie goes to the store, and a teammate's newer revision still imports. Skipped records are counted as `skipped_up_to_date` (per file and in total, CLI and MCP) and are not reported as dropped. The store is now read once per import rather than once per record: on SessionDrift-ios's 8,711 committed records the hook takes 3.6 s when nothing changed.
- **Transitive effects were incomplete for functions that call each other, and changed from run to run.** The walk that propagates effects up the call graph stopped at a call cycle's first member and kept that partial answer for the others, so a function in a recursive loop could miss effects that only another member's callees have — and which function lost out depended on the order symbols happened to be visited, so identical re-indexes disagreed (and committed the difference). Effects are now computed per strongly connected component of the call graph: every member of a cycle gets everything the cycle reaches, the same on every run. On this repository a re-index now writes no transitive updates when nothing changed, where it used to flip one symbol back and forth. The walk is also iterative, so a deep call chain no longer risks overflowing the stack. `propagate_transitive` uses the same computation.

### Added
- **`asd repair` finds and restores lost ledger entries.** A new `ledger_missing_from_asg` issue reports every entry the ledger cache acknowledged but the store no longer has, by kind; `asd repair --fix` writes each back from the cache, byte-for-byte with its reverse-index record, one commit per restore so the audit trail says what was recovered. Entries are matched by id, so one moved by `ledger rebind` is never mistaken for a lost one. On the affected store `--fix` restored all 1,684 in 50 s, after which cache, store and sidecar all read 12,085.
- **`asd sync` and `asd status` report ledger counts in the cache, the store and the sidecar** (CLI and MCP; `ledger` and `ledger_warning` in JSON), and warn on a mismatch — pointing at `asd repair --fix` for entries the store lost and at `asd sync` for entries not yet exported. `asd status` no longer says the sidecar is current when those counts disagree: "hydrated" only ever meant a marker file existed.

### Changed
- **Human-readable `asd status` now opens the store** to count ledger entries: ~0.1 s warm on a 12,000-entry store, up from ~0.02 s. Counting reads keys only; no entry is loaded.
- **Inferred effects record the adapter that inferred them** (`"adapter": "python"`), so the first `asd sync` after upgrading rewrites each file under `.asd/v1/effects/` once.
- **A re-index writes only what changed.** It used to restamp every re-parsed symbol's effects and clear and rewrite every symbol's transitive effects (496 of 3,032 on this repository) on every run. Measured on this repository: each re-index now grows the store by 0.5 MB instead of 5.8 MB and takes ~4.0 s instead of ~4.25 s; a cold index takes 3.9 s instead of 3.8 s and leaves a 21 MB store instead of 23 MB.
- **AgentStateGraph pinned to v1.2.7.** v1.2.5 made concurrent writers stop discarding each other's commits (the ledger loss above). v1.2.6 closes the losses that remained, two of which reached ASD: an `asd gc` sweep running while `asd index` held a speculation could delete the index pass's not-yet-committed objects, after which the commit either published a broken tree or silently dropped the pass's writes — open speculations are now GC roots in every process; and a write arriving while a sweep held its lock could be rolled back after returning `Ok` — it now waits, or fails loudly. It also stops concurrent updates to one task, policy or reminder reverting each other, and makes merges fail instead of guessing when they can't read an input. v1.2.7 fixes the last batch: every asd process initializes the store it opens, and two opening a fresh store at once (`asd init` beside a hook or the MCP server) could each create `main`, the later discarding what the earlier had written — `main` is now created only if absent; a write below an existing value no longer replaces that value; and integers above `i64::MAX` are stored exactly instead of as floats.

---

## [v1.4.0] — 2026-09-23

### Added
- **`asd gc` — reclaim the space the ASG store has been accumulating.** Every `asd index` commits a fresh state tree, and AgentStateGraph has long had a sweep, a vacuum and a retention policy — but ASD wired only `/gc/dry-run`, a report. Nothing ever reclaimed. `asd gc` previews by default and deletes nothing without `--sweep`; `--unpin-legacy` releases milestone pins distilled before AgentStateGraph v1.2.2 (one-time, deletes nothing itself); `--vacuum` shrinks the file. Run for real, each store backed up first and verified after — no object missing from any live ref tip, SQLite `integrity_check` ok, ledger/symbol/effect counts unchanged:

  | store | before | after |
  |---|---|---|
  | SessionDrift-ios | 8.49 GB | 1.43 GB |
  | AgentStateDeveloper | 1.51 GB | 109 MB |
  | AgentStateGraph | 1.23 GB | 85 MB |
  | CTXone | 611 MB | 73 MB |

  On a store that predates v1.2.2 the flag matters: SessionDrift reclaimed 3.4% without `--unpin-legacy`, 95.5% with it. Operationally, **don't commit in a repository while `asd gc --sweep` runs on its store** — its hooks wait on the sweep's write lock and then fail (cleanly; nothing is lost).
- **`POST /api/v1/gc/sweep` — a policy-aware GC preview.** It takes the same body as AgentStateGraph's own route but only previews: `asd-serve` binds every interface without authentication, so `mutate`/`vacuum` are refused with 403 pointing at `asd gc`, and `keep_milestones: false` with 400. Deletion is deliberately CLI-only.
- **`git_sha` on `/api/v1/history/milestones`**, beside `pins_state` — the revision a milestone was derived from.

### Changed
- **The index checkpoint records the git revision it indexed, and no longer pins its state.** Every re-index used to pin a full snapshot as a milestone, kept forever: on SessionDrift-ios, 1,503 milestones held 5.3M objects reachable. Nothing in ASD ever read those snapshots. The revision (full hash) is recorded instead, so derived state is rebuilt from source — `git checkout <sha> && asd index` — rather than restored. A rebuild is faithful to what the code looked like, not bit-identical to what an older ASD computed. `pins_state: false` is now the ordinary case for routine checkpoints rather than a legacy-row signal.
- **A sweep keeps no sparse checkpoints by default** (`--checkpoint-every 0`), departing from AgentStateGraph's 100. ASD's history is derived state; measured on SessionDrift after unpinning, one checkpoint per hundred commits would have kept 1,195,871 objects instead of 256,247.
- **Requires AgentStateGraph v1.2.3.** It makes pinning opt-in (v1.2.2) and fixes a data-loss race in the sweep this release relies on: previously a commit from another connection — a git hook, `asd-serve`, an MCP server — landing mid-sweep could lose its objects. The sweep now holds the store's write lock from computing its keep-set to its last delete.

## [v1.2.1] — 2026-09-04

### Fixed
- **`install.ps1` could not be parsed by Windows PowerShell 5.1 — the Windows installer was broken for anyone running the stock shell.** Windows PowerShell 5.1 reads a `.ps1` file in the system ANSI codepage unless the file carries a UTF-8 BOM. The installer was UTF-8 with no BOM and 235 box-drawing characters in its section headers; `U+2500` is the bytes `E2 94 80`, and `0x94` in CP1252 is a smart right-double-quote — which PowerShell treats as a **string delimiter**. 5.1 saw 235 stray quotes and died on a wall of unterminated strings before reaching a single line of logic. `iwr … | iex` was unaffected, because `Invoke-WebRequest` decodes the HTTP body by its charset and never reads a file through the codepage — so the *safer* habit, downloading the script to read it before running it, was the broken one. Both `.ps1` files are now pure ASCII, asserted as an invariant by the test suite. PowerShell 7 reads UTF-8 regardless and was never affected, which is why this survived unnoticed.

### Added
- **`install.ps1`'s checksum verification now runs on every change, on a real Windows runner.** It had never executed anywhere — there is no PowerShell on the development machine, so the code was reviewed rather than run, and it *fails closed*: a pattern that never matches means every Windows install dies rather than degrading. `scripts/test-install-ps1.ps1` exercises eight cases against a local fixture release — sums matching, a tampered tarball, sums absent (must stay soft), sums omitting the tarball, the documented `iwr | iex` path, and guards for uppercase sums, CRLF line endings and the binary-mode `*` marker — on **both** PowerShell 7 and Windows PowerShell 5.1.
- **Every release now verifies that its Windows artifact actually installs.** `verify-release-windows.ps1` runs on `windows-latest` after publish, installing the real release and asserting a clean runner, a checksum verified against the published `SHA256SUMS`, all three binaries present, and the tag's version reported back. This is the only check on the Windows tarball's sum: the Homebrew formula pins a sha256 per target but has no Windows bottle, so the formula comparison skips that target entirely.
- **`ASD_DOWNLOAD_BASE`** on both installers — points them at an alternate asset location so the installer can be exercised against a fixture without editing the file under test. `ASD_RELEASES_REPO` could already redirect the download, so this adds no new exposure.

### Changed
- **The tap-mirror wait is now based on a measurement rather than a guess.** `verify-install` allowed 300s for the GitLab→GitHub tap mirror; the real latency on v1.2.0 was **11m43s**, so a sound release reported *"formula version is 1.1.0, expected 1.2.0"* — indistinguishable from a bad one. The budget is now 1800s, and an exhausted budget is reported differently from a genuinely wrong formula. A check whose red is ambiguous gets ignored, and being ignored is how the breaks it exists to catch survive.

## [v1.2.0] — 2026-09-03

### Fixed
- **The documented Homebrew install failed for every new user.** Homebrew 6.0 refuses to load formulae from untrusted third-party taps, so `brew tap agentstatelabs/agentstatedeveloper && brew install asd` stopped with *"Refusing to load formula … from untrusted tap"*. An existing install and `brew upgrade` both bypass the gate, which is why it survived a release unnoticed. A `brew trust` step is now documented everywhere the command appears — including the website's copy button, which was handing out the broken command.
- **`install.sh` and `install.ps1` installed downloaded tarballs without verifying them.** The Homebrew formula pins a sha256 per target and refuses on mismatch; the shell path trusted TLS alone. The sums already existed — `release.yml` computed one per target into its own job log, where nothing could consume it — and are now published as a `SHA256SUMS` release asset that both installers check. A missing sums file warns and continues, so pinning an older `ASD_VERSION` still works; a file that is present and does not match is a hard failure.
- **The distilled history rollup stopped advancing.** ASG's extractor only ran when one of three asd-serve endpoints was hit, so a store whose Lens pages nobody browsed drifted arbitrarily far behind — on this repo the cursor sat at rowid 6,017 of 36,638, and `/records`, `/health` and `asg history` were all reporting May–July data as current. `asd index` now advances it, which keeps it current as a side effect of ordinary work: a 33,429-commit backlog folded in one call, and the steady state is the handful of commits indexing itself writes.

### Added
- **`scripts/verify-release.sh`** — proves a published release is actually installable: the published `install.sh` still matches the repo copy, a clean install yields all three binaries at the right version, checksum verification actually fires when sums exist, the tap formula parses and agrees with `SHA256SUMS` and names assets that resolve, and the documented commands still carry the `brew trust` step. Runs on every tag as the `verify-install` CI job, in a clean container so no pre-existing install can mask a break. `--brew-clean` adds the destructive uninstall/reinstall a container cannot do.
- **`install.sh` warns when it shadows another `asd`.** Its default `INSTALL_DIR` (`~/.local/bin`) sits ahead of Homebrew on many PATHs, so installing there silently overrode a brew install and made `brew upgrade asd` appear to do nothing — both binaries report the same version, so there was no signal at all.

### Changed
- **`/api/v1/commits` uses the engine's DAG walk** rather than a hand-rolled one. AgentStateGraph v1.2.1 ships `Repository::log_dag`, so the workaround here is gone; `scanned` / `capped` / `distilled` reporting is unchanged, and the metrics API tests pass untouched.
- **AgentStateGraph pin moves v0.9.22 → v1.2.1**, bringing `blame` that names the commit rather than the merge, `commit_graph` whose node set contains every node its edges name, `stats` that counts merged-in commits, a `detect_timestamp_anomalies` that sees clock rewinds inside a merged branch, and a history rollup that says `unattributed` instead of claiming `default`. Rollup rows distilled before this release keep their old value: clear `asg_history_meta.commit_cursor` and re-extract to normalise.
- **A `distilled` larger than `scanned` no longer means "already-garbage".** That reading was wrong twice over — the walk starts at one ref head, and this store legitimately holds two namespaces whose histories are disjoint, so 1,628 commits it never reaches are another namespace's live history rather than garbage; and `distilled` is the extractor's cursor, not the store's size, so it can err in either direction. GC was always right to report ~5% reclaimable.

## [v1.1.0] — 2026-09-01

### Added
- **ASD Lens gains two pages: `/records` and `/health`.** `/records` makes the
  distilled history searchable — the milestone spine, the commit rollup, the
  raw commit chain and recorded search feedback, with free-text search, facet
  filters and date ranges. All filter state lives in the URL, so any view is a
  shareable link. `/health` answers whether that record is worth trusting: the
  five capability scores, the ledger-density caveat, the token-economy
  estimate and index freshness, with a filterable per-dimension gap list.
- **Six read-only endpoints** behind those pages, sharing one envelope
  (`total` / `offset` / `limit` / `items` / `facets`):
  `/api/v1/history/milestones`, `/history/rollup`, `/commits`, `/feedback`,
  `/index-health` and `/scorecard`.
- **A lifecycle for search feedback.** Until now a recorded verdict could not
  be taken back, so a mistyped `noisy` suppressed a good symbol on every
  future query:
  - `asd feedback expire` — the verdict was right but is no longer relevant.
  - `asd feedback withdraw` — it was wrong. Records who retracted it and why,
    and cannot be revived by re-dating an expiry.
  - `asd feedback purge --yes` — the escape hatch for data that must not
    persist at all (test entries, a secret pasted into a `--note`). Prints the
    entry, including its note, before it will act.

  Retired verdicts stop influencing ranking but stay listed, marked: they
  still explain why a past search ranked as it did. `withdraw` and `expire`
  are also available from the Lens Feedback tab, and `withdraw` as the
  `feedback_withdraw` MCP tool. `purge` is deliberately CLI-only — a hard
  delete from an otherwise append-only store should not be one click away.

- **A CI guard against conclusion-record loss.** `conclusions-guard` fails any
  branch whose sidecar would drop a record present on the target, naming each
  id and the exact recovery command. Deliberate removals pass with
  `ALLOW_CONCLUSIONS_DROP=1` or `[conclusions-drop-ok]` in the commit message.
  It earned its place immediately: it caught a real three-record loss on a
  branch of its own release series, caused by a stale installed binary.
- **`scripts/wait-pipeline.sh`** — waits on the pipeline for a specific commit.
  `glab ci status --branch` answers "the latest pipeline on this ref", not
  "the pipeline for this commit", so automation that pushes and polls in one
  breath can read the previous commit's result and merge a SHA whose pipeline
  never ran. Queries `?sha=` and treats "no pipeline yet" as keep-waiting
  rather than as a verdict.

### Fixed
- **The conclusions sidecar could silently delete records a clone could not
  reproduce.** `asd conclusions export` regenerates `.asd/conclusions/*.jsonl`
  wholesale from the local database, so any record the local database could
  not produce was dropped on the next commit. The export is now **additive**:
  `gather()` reports the ids it retired *deliberately* (superseded entries, and
  `Hypothesis` records below the confidence floor) and the merge keeps every
  committed record that is not one of them. Deliberate withdrawals still stick;
  everything else survives.

  The audit that found this walked 363 commits of `main` and turned up exactly
  one real loss, restored two merges later by luck. The cause was not the stale
  branch everyone assumed: it was `:line`-disambiguated qname churn. A decision
  anchored to `ApiError:237` stopped resolving when the symbol moved, so the
  export skipped it — **which happens on a perfectly up-to-date branch**, not
  just a stale one.

  Note the limit: this protects against a local database that cannot produce a
  record. It cannot resurrect one that a previous bad export already removed
  from the file — restore that from the target branch first, then re-export.
- **`asd feedback mark --ttl-days` never actually did anything.** The field,
  the `is_expired()` helper, the SQLite persistence and doc comments on both
  the field and the helper all claimed lapsed verdicts stopped influencing
  ranking — but the filter was never wired into `flat_verdicts`, so every TTL
  ever set was decorative. Now enforced.

  **This changes search results on upgrade** for any repo with TTL'd
  verdicts: entries whose expiry has passed stop affecting ranking, which is
  what they were always meant to do. `asd feedback list` still shows them.
- **`GET /api/v1/gc/dry-run` no longer stalls the whole API.** Its
  reachability marker walks the entire object DAG — 27s on an 866k-object
  store — and held the engine lock throughout, so an unrelated request issued
  one second into a run waited 25s behind it. It is now memoized on the ref
  head, single-flight so concurrent callers share one walk, and run on its own
  read-only SQLite connection. Repeat calls 27s → 16ms; the stall on unrelated
  requests 25.2s → none. `?cached_only=1` returns the memo or a cheap
  `uncomputed` marker without starting a walk, and the Lens History page no
  longer blocks its own render on it.
- Wide tables in Lens scroll instead of clipping. Long symbol names — Swift
  qnames reach ~200 characters — pushed the Health drill-down past the
  viewport with no scroll container, putting its last five columns out of
  reach entirely.

### Changed
- The five-dimension scorecard has **one implementation** instead of three.
  `asd scorecard`, the `scorecard` MCP tool and `GET /api/v1/scorecard` each
  carried their own copy of the same formulas, and the MCP copy had already
  drifted. Output is byte-identical for the CLI and HTTP; the MCP tool gains
  the `token_economy`, `coverage_pct` and sparse-ledger blocks it was missing.
- `/api/v1/commits` walks the full parent DAG rather than first parents only.
  On this repo that is the difference between seeing 4,268 commits and 5,896,
  and the ones a first-parent walk skips are the population most likely to be
  reclaimed by a sweep. The endpoint also reports how many commits the
  distilled rollup knows about that the ref head no longer reaches.

## [v1.0.0] — 2026-08-24

First stable release. No functional change from `v0.9.41` — the version marks
the suite's coordinated 1.0, alongside agentstategraph and ctxone.

### Added
- ASD Lens screenshots in the README (territory hero, symbol/effects gallery).

## [v0.9.41] — 2026-08-19

### Added
- **`GET /api/v1/files`** and **`GET /api/v1/files/{path}`** — list the indexed
  file set and read a file's contents over the serve API.

### Fixed
- Releases no longer push tags straight to GitHub, which bypassed the
  fail-closed leak-scan gate on the public mirror.

## [v0.9.40] — 2026-08-17

### Changed
- The Homebrew formula is rendered GitLab-side, dropping both tap secrets.

### Fixed
- The GitLab host moved into a CI secret — as a literal it tripped the
  leak-scan gate and blocked the pipeline.

## [v0.9.39] — 2026-08-15

### Changed
- **GitHub Actions is the sole release publisher**; local builds are retired.
  Two publishers racing on one release is how assets end up with `sha256`s that
  disagree with what CI built.

### Fixed
- `TAP_ROOT` resolution and annotated-tag comparison in the release script.

## [v0.9.38] — 2026-08-14

The first release on the renumbered line — see the note at the top of this
file. Carries the worktree, help and onboarding work that had accumulated on
`main` since `v1.3.1`.

### Added — code intelligence
- **Lens History page** — project-history and store-health dashboards, backed
  by new `/history` and `/gc/dry-run` endpoints (agentstategraph rolled to
  v0.9.22).
- **Decision rationale and confidence** are carried onto the commit in the
  ledger.
- **A git union merge driver for the conclusions sidecar JSONL**, so parallel
  agents appending conclusions no longer conflict on every merge.

### Fixed
- `conclusions list` and `conclusions export` are driven from the ledger tree
  rather than every symbol — the previous walk did not scale, and a large-DB
  regression guard now pins it in CI.

### Added — parallel-agent worktrees

- **`asd worktree`** — plan-scoped git worktrees, mirroring CTXone's
  `ctx worktree`. Each unit of work gets its own worktree + `plan/<name>`
  branch, so parallel agents get isolated files and HEAD (they can't clobber
  each other) while sharing context through the hub.
  - `asd worktree start <plan>` adds `../<repo>-wt-<plan>` on `plan/<plan>`;
    `list` recovers the plan↔worktree binding from `git worktree list`;
    `finish <plan>` merges back and (by default) tears the worktree down
    (`--keep` to skip teardown, `--push` to push the merged branch).
  - `--shared-target` shares one Rust build cache across worktrees (points
    `target-dir` at `<repo>/.wt-target`, avoiding a multi-GB `target/` per
    tree).
  - `--clone` isolates via a fresh clone with its own `.git`
    (`../<repo>-clone-<plan>`) for remote/cloud agents on another machine,
    merging back via `origin` instead of a local merge.
  - New worktrees auto-enable the repo's `.githooks`.

### Added — on-demand instruction disclosure

- **`asd help [topic]`** and the **`asd-mcp` `help` tool** (the **64th** MCP
  tool) — return compiled-in feature docs (synopsis, syntax, params,
  examples, gotchas) for one feature or the full catalog, instead of carrying
  every tool's full docs in context every turn. Docs are version-pinned to the
  running binary, so the CLI and MCP tool return byte-identical payloads.
  - `--agent` for machine-readable JSON; `--manifest` prints this binary's
    feature manifest.
  - **Cross-binary proxy**: an unknown topic is resolved by the owning tool
    (asd ↔ ctx). `--publish` writes this binary's manifest into the shared
    cross-tool help index so a unified `help` discovers asd's features
    alongside ctx's (tool-keyed — publishing asd never clobbers ctx's slice).
  - Comprehensive asd feature registry (15 → 64 features).
  - `asd skill`'s always-on block now points agents at `asd help` for
    on-demand syntax.

### Changed

- **`asd onboard`** now also folds in project-scoped MCP registration, so the
  one-shot post-clone setup connects the agent's tools as part of the same
  command.

<!-- Numbering pivot: everything below predates the 1.3.1 -> 0.9.38 renumber. -->

## [v1.3.1] — 2026-07-29

Patch release: ASG integrity-gate bump + namespace sanitization.

### Changed

- Bumped `agentstategraph` v0.9.6 → v0.9.10 (merge integrity gate).

### Fixed

- **Project namespaces with out-of-charset characters no longer error.**
  ASG v0.9.10 tightened namespace validation to `[A-Za-z0-9_-]`, but asd
  derived its namespace straight from the project directory name — so dirs
  with dots or spaces (including tempdirs like `.tmpXXXX`) began failing. Added
  `Engine::sanitize_namespace` to map out-of-charset characters to `_` before
  `Namespace::new`. Valid names pass through unchanged, so existing projects
  keep their namespace and no data moves.

## [v1.3.0] — 2026-07-13

Everything on `main` since the v1.2.0 tag (Plans T–V + workflow hardening).

### Added — agent-workflow surface

- **New MCP tools** `map`, `sync`, `test_summary` — bringing the MCP surface to
  **63 tools**. `map` (also `asd map`) persists Ownership ledger entries on
  initial read (contrast the read-only `architecture`); `test_summary`
  (`asd test-summary`) emits a failures-only summary of test-runner output.
- `asd prepare-change` gains an `--avoid` alias (for `--exclude`) plus documented
  `--paths` / `--scope` hints to fight lexical over-matching.
- `asd annotate-commit` splits **directly-edited** symbols from **nearby/touched**
  ones when deriving ledger annotations from a commit.
- Agent-discoverability polish across the CLI help and MCP tool descriptions.
- **Registry hygiene** — `asd repo prune` removes entries whose `.asd-state.db`
  no longer exists (`--dry-run` to preview, `--json` for machine output). Root
  cause fixed too: `asd index` no longer auto-registers databases under temp
  directories and respects `ASD_NO_AUTO_REGISTER`, so test runs stop polluting
  `~/.config/asd/repos.toml`. And a real `asd index` now **opportunistically
  self-heals** the registry — dropping dead entries as it runs (opt out with
  `ASD_NO_AUTO_PRUNE`) — so a standalone asd install stays clean with no
  scheduler or CTXone.

### Added — Lens (asd-serve) review UI, Plan T

- `/territory`, `/activity` (live accountability), and the approvals / `/graph` /
  `/effects` pages; symbol-detail and `/code/*` render from `@agentstate/lens-core`;
  dark-first visual identity.

### Performance

- P0 bulk-read swaps + hydrate cache population + startup self-heal.
- `/callers`, `/callees`, `/graph` cut from minutes to <100ms (head-keyed
  id/edge memos); single-pass `/api/v1/symbols` and `/thinking` from minutes to
  ~2s at 10k symbols.

### Fixed

- `conclusions import` now round-trips thinking entries and confidence (was
  silent data loss).

## [v1.2.0] — 2026-07-06 — cross-service edges, federation & Lens backend

Consolidates the 1.0.24 → 1.2.0 arc (tags 1.0.44 / 1.0.48 / 1.1.15–1.1.23,
which shipped without individual entries). Headlines:

### Added — cross-service & cross-repo intelligence

- **Cross-service edge detection across 9 languages** (HTTP + pub-sub):
  Python (FastAPI/Flask, incl. intra-file / mount-tree / alias-keyed /
  multi-mount router-prefix resolution), TypeScript/JS, Go, Java/Spring,
  C#/ASP.NET, Ruby (Sinatra/Rails), Kotlin (Spring + Ktor), Swift (Vapor),
  and Celery pub-sub. `asd endpoints` keys client calls and server routes by
  the same normalized contract, so unmatched consumers surface contract drift.
- **Federation** (Plan Q): decision-aware `asd repo impact` and cross-repo
  service-edge matching via `asd repo edges`; coherent active-repo resolution
  and `asd mcp --follow-active`.
- **Edge-confidence** model (EdgeEvidence) surfaced in `context_for`.

### Added — MCP surface & onboarding

- New MCP tools `trust`, `architecture`, `endpoints`, `dead_code`.
- `asd skill` (version-stamped SKILL.md install, downgrade refusal, suite
  detection), `asd bootstrap` (paste-to-your-agent installer), `asd watch`
  (auto-reindex on source changes).
- More agent integrations: Kilo Code, Antigravity, Aider, JSONC config support;
  `asd mcp instructions` / `asd mcp install --project`; non-blocking Claude Code
  hook.

### Added — Lens backend & conformance

- Lens read APIs (search, graph, effects overview, timeline) and the
  `/api/v1/events` SSE live-activity stream; `@agentstate/lens-core` as a file dep.
- Cross-language capability matrix + contract tests; tier-2 conformance realism
  over real source trees; adapter network-effect detection fixes.
- `asd scorecard` internal token-economy estimate.

## [v1.0.23] — 2026-06-02 — Plan G complete (agent-thinking layer)

ASD now persists the agent's *thinking* — speculation, mental models,
failed attempts, open questions — so a fresh session resumes with
the expensive understanding it would otherwise re-derive. Auto-surfaced
in `prepare_change` / `context_for` as `prior_thinking`.

### What's new

- **Four new ledger kinds** — `Hypothesis` (uses `confidence`),
  `MentalModel` (body carries `symbols[]` + `name`), `FailedAttempt`
  (body: `tried` / `because`), `OpenQuestion`. Routed through a 7th
  `ConclusionClass::Thinking` bucket in the sidecar. (t-002)
- **`asd think <verb>`** — five subcommands (`speculate`, `model`,
  `failed`, `question`, `list`). Entry IDs are deterministic blake3
  of `(intent, qname, content)` so re-running the initial-read prompt
  overwrites rather than duplicates. Mirror MCP tools:
  `think_speculate` / `think_model` / `think_failed` / `think_question`
  / `think_list`. (t-003)
- **Initial-read prompt template** — `docs/initial-read-prompt.md`. A
  7-section guide the agent reads and acts against the indexed project,
  writing back via the `asd think *` commands. ASD doesn't make LLM
  calls; the template is the contract. (t-004)
- **`asd think bootstrap [--check]`** — guided entry point that prints
  the prompt path + starter checklist + write-back command reference.
  With `--check`, scans existing thinking entries and reports gaps
  (e.g. "no MentalModel yet"). Supports `--json` for MCP. (t-005)
- **`prior_thinking` auto-surface** — both `prepare_change` and
  `context_for` now embed a compact `prior_thinking` projection of
  the relevant symbols' thinking entries. Hypotheses below
  `DEFAULT_CONFIDENCE_FLOOR = 0.3` are excluded; nothing surfaces when
  no thinking exists. (t-006)
- **`ctx:task:<id>` provenance auto-tag** — every `asd think *` write
  (CLI and MCP) stamps the active CTX task id when set, matching the
  trail Plan E added for map/ledger writes. (t-007)

### Plan F follow-ups bundled in

- **`Move` recipe action** revived for the test-migration recipe, with
  `migrate_stale_tests()` reading `Mapping` bodies for move targets.
- **MCP `AsgEffectStore` construction** fixed (struct literal → `::new`).
- **`read_active_task_scope_from(env_raw, db_parent)`** extracted so
  parallel tests can pass explicit env strings instead of mutating the
  process env.

### Adoption (worked example)

```sh
# 1. Seed the project graph.
asd reindex

# 2. Print the prompt template path + checklist.
asd think bootstrap

# 3. Read docs/initial-read-prompt.md, then record findings:
asd think model audio-pipeline \
    --symbols pkg.mixer.Mixer,pkg.io.Input,pkg.io.Output \
    --summary "input → mix → output, single-threaded"
asd think speculate pkg.mixer.Mixer --conf 0.7 \
    --summary "buffer size 4096 chosen for ~93ms latency at 44.1kHz"
asd think question pkg.mixer.Mixer --q "what does the 4096 constant mean?"
asd think failed pkg.io.Output --tried "ring-buffer cache" \
    --because "broke under reload — state leaked across sessions"

# 4. Confirm coverage; --check reports remaining gaps.
asd think bootstrap --check

# 5. Subsequent prepare-change / context-for calls now embed:
#     prior_thinking: { hypotheses, mental_models, failed_attempts, open_questions }
```

### Plumbing

- `core::thinking::gather_prior_thinking(engine, qnames, min_confidence)`
  single-sources the `prior_thinking` shape. 7 unit tests cover the
  null case, surfacing, confidence-floor exclusion, mental_model /
  failed_attempt body parsing, and the non-thinking-kinds exclusion.
- 4 CLI tests cover the `ctx:task:<id>` resolver (env JSON, file
  fallback, env-wins-over-file, both-absent).
- New MCP regression test `plan_g_think_tools_are_registered` proves
  the 5 tool routes are wired.

---

## [v0.9.99] — 2026-05-20 — Plan C complete (semantic-layer moat)

The defining-feature release. Plan A built trust, Plan B built durable
storage, Plan C makes ASD remember the expensive task-specific
understanding the LLM forms so a new session doesn't re-derive the
same project mental model.

### The moat (what's new)

- **Active decisions** — `Constraint`/`Decision` ledger entries carrying
  a penalty role (`stale-api` / `audit-pending`) now actively suppress
  their symbols in ranked search, the same way `WrongLayer` feedback
  does. Memory becomes a ranking input, not a passive note. (t-003)
- **First-class role-tag vocabulary** — `RoleTag` enum with 8 canonical
  tags (`fast-test`, `diagnostic-test`, `fixture-path`, `stale-api`,
  `package-boundary`, `replacement-coverage`, `performance-critical`,
  `audit-pending`). CLI / MCP warn on unknown tags; old free-form
  strings still round-trip. (t-002)
- **Change-intent recipes** — `asd recipe classify-test-migration <q>`
  returns a structured `{intent, actions[]}` plan
  (`Delete` / `Gate` / `Run` / `KeepAsCovered` / `Review`) instead of a
  flat symbol list. Pattern is reusable; more recipes will follow. (t-004)
- **AlreadyCovered + DiagnosticOnly verdicts** — two new
  `FeedbackVerdict` variants that suppress like `Noisy` and prompt the
  caller to accrue a `Mapping` or `Classification` ledger entry in the
  same gesture. (t-005)
- **CTX task state → ASD ranking bias** — ASD now reads
  `CTX_ACTIVE_TASK` (or `.asd/cache/active-task.json`) and applies a
  soft +1.0 boost to candidates inside the task's recorded scope. Never
  a hard filter. (t-006)
- **`asd map`** — one-shot initial-read command that walks the index,
  identifies package boundaries, classifies test files
  (`fast-test` vs `diagnostic-test`), and writes `Ownership` ledger
  entries with role tags. Idempotent via deterministic entry IDs. The
  bootstrap that makes the downstream Plan C features useful on a fresh
  project. (t-007)

### How to adopt

```sh
asd index .                   # seed the index (unchanged)
asd map                       # one-shot: seed role-tagged Ownership entries
asd ledger append <qname> \\  # accrue a stale-api Constraint
  --kind constraint --role stale-api \\
  --summary "deprecated; do not use"
asd conclusions export        # commit the new conclusions
asd search "legacy"           # the stale-api symbol is demoted
```

Set `CTX_ACTIVE_TASK='{"task_id":"t-X","scope":["src/foo/**"]}'` once
per session to bias all queries toward the current task's scope.

### Acceptance probes

Four new probes in `examples/acmeflow-probes.toml` tagged `plan-c`:
recipe-returns-structured, diagnostic-only-verdict-suppresses,
asd-map-writes-classifications, stale-api-constraint-demotes-symbol.

---

## [v0.9.89] — 2026-05-20 — Plan D complete (Crucible token-efficiency)

Five fixes driven by AgentStateCrucible's A/B testing, which showed
the assisted arm paying 2.5–5.7× baseline tokens despite making
fewer or similar tool calls.

### Added
- **`--brief` output mode** (0.9.85) — global `--brief` flag and
  `ASD_FORMAT=brief` env var. Projects `read` / `callers` / `callees`
  responses down to load-bearing fields (qname, file:line, signature,
  first doc line). Drops `symbol_id`, `symbol_fp`, `language`, `kind`,
  `col`, end positions, full doc body, empty arrays. Expected token
  reduction: 60–80% on the cited commands.
- **MCP-era name aliases on CLI** (0.9.87) — `asd code_read`,
  `code_search`, `code_query`, `callers_of`, `callees_of` all route to
  the canonical CLI subcommand. Agents trained on older MCP-era docs
  no longer hit `unrecognized subcommand`.
- **`query_id` in responses** (0.9.89) — deterministic blake3-derived
  id (`Qf3a2b1c`) on `read` / `callers` / `callees` output so
  wrapper-side tools can dedup repeated queries.

### Fixed
- **`asd investigate` accepts unquoted multi-word queries** (0.9.86) —
  `asd investigate failing test store` now joins to one query. Crucible
  captured 3 consecutive turns lost to this UX trap.
- **Python relative-import call edges** (0.9.88) — `parse_imports`
  had an early-return on `relative_import` nodes that silently dropped
  every `from .foo import bar` site. Crucible's own package indexed
  71 symbols with **0 cross-module edges**; the fix resolves `.`/`..`
  prefixes against the importing file's module path. End-to-end
  reproducer landed as a unit test.

### Expected Crucible re-run outcomes
When Crucible pins 0.9.89, the three scenarios should flip:
- `asd-vs-baseline` — brief mode puts assisted ≤ baseline tokens.
- `inheritance-bug` — tie becomes clean assisted win.
- `cross-layer-tax` — assisted produces the reference fix
  (rates.py + display.py) instead of the proximate `apply_tax` patch.

Crucible posts the empirical delta report when the re-run completes.

---

## [v0.9.84] — 2026-05-19 — Plan B complete (compact conclusion sidecar)

### Migrating an existing repo

The committed sidecar shape changed in Plan B. Old layout was `.asd/v1/`
(tens of MB on real projects). New layout is `.asd/conclusions/*.jsonl`
(kilobytes). One-shot migration:

```sh
asd sidecar migrate           # writes .asd/conclusions/*.jsonl
git rm -r --cached .asd/v1    # drop the old bloat from tracking
git add .asd/conclusions/
git commit -m "plan-b: switch to compact conclusion sidecar"
```

Re-running `asd init` afterwards is safe — it scaffolds the new layout
and flips the git hooks (pre-commit → `asd conclusions export`;
post-merge / post-checkout → `asd conclusions import`).

### Added
- `asd conclusions list|export|import` — read, write, and round-trip the
  six conclusion classes (decisions, classifications, mappings, hazards,
  recipes, followups). Byte-stable JSONL; idempotent import keyed by
  `entry_id`.
- `asd sidecar migrate` — one-shot migration with savings report and
  `git rm --cached` instructions for the legacy `.asd/v1/` tree.
- Two new `LedgerKind` variants: `Mapping` (legacy → new coverage
  cross-refs) and `FollowUp` (open items tied to external task systems).
- Two optional `LedgerEntry` fields: `role` (classification tag) and
  `command` (canonical reproduction command).
- New MCP tools: `conclusions_list`, `conclusions_export`,
  `conclusions_import`.
- `field_lte` probe assertion kind (used by the new sidecar-size probe).

### Changed
- `asd init` now scaffolds `.asd/conclusions/` (tracked) and `.asd/cache/`
  (gitignored). Git hooks flipped from the old hydrate flow to the new
  conclusions round-trip.
- Updated `.gitignore` to add `.asd/cache/` alongside `.asd/v1/` (Plan A
  line preserved). `.asd/conclusions/` is intentionally not ignored.

---

## [v0.8.5] — 2026-05-05

### Added
- **Swift function signatures** — `asd index` now captures full Swift function
  signatures (parameter labels, types, return type, `async`/`throws`) from
  tree-sitter parse; stored in `symbol.signature`; shown in `asd read` and
  `asd context-for` output.
- **Extensible effect categories** — `EffectCategory` gains an `Other(String)`
  variant accepting any dot-namespaced string (e.g. `midi.send`,
  `audio.graph.connect`, `scheduler.restart`, `ui.state.mutate`).  Existing
  built-in categories are unchanged; new categories are user-declared.
- **Workflow ledger entry types** — three new `LedgerKind` variants:
  `invariant` (what must always be true), `ownership` (subsystem/team owner),
  `proof` (evidence an invariant holds). `hazard` was already present.
  All four are now available via `asd ledger append --kind`.
- **`asd context-for <qnames>`** — context assembly command for agent queries.
  Given one or more comma-separated qnames, returns a ranked package:
  symbol signature + location, direct callers/callees, declared + transitive
  effects, invariants, hazards, ownership, proofs, and other ledger entries.
  Accepts `--budget-tokens` and `--include-body`.

---

## [v0.8.0] — 2026-05-05

### Added
- **`asd callers <qname>`** — show direct or transitive callers of a symbol
  (`--depth N` for BFS expansion); output includes qname, file, and line.
- **`asd callees <qname>`** — same for callees.
- **Callers + callees in `asd read`** — every symbol read now includes its
  direct callers and callees resolved to qname + file:line.
- **`asd list effects --file <substr>`** — filter effects output by source
  file path substring (joins through symbol map).
- **`.asdignore` support** — place one directory-name pattern per line at
  the repo root to exclude custom paths from `asd index`.

### Fixed
- **Broken-pipe panic in `asd list`** — piping to `head` / `grep` now exits
  cleanly (SIGPIPE handled via `BrokenPipe` error check at process exit).
- **Stale `.claude/worktrees` symbols** — `.claude`, `.asd`, `.build`,
  `DerivedData`, `target`, `xcuserdata`, and other build/tool directories
  are now excluded from indexing by default.

---

## [v0.7.5] — 2026-05-05

### Fixed
- **O(N²) object storage in `asd index`** — `spec_set_json` per symbol created
  a growing Map node copy at every shared prefix on each write; 1,341-file repo
  produced a 59 GB DB. Now assembles complete subtree JSON in memory and writes
  each prefix with a single `spec_set_json` call. O(N) objects regardless of
  repo size.
- **O(N²) object storage in `asd hydrate`** — same root cause; fixed with the
  same bulk approach for symbols and effects.
- **Transitive DFS now fully in-memory** — uses in-memory `callees_of` map from
  Pass 2 instead of per-symbol `get_callees` repo reads.

### Added
- **Post-processing phase messages** — `asd index` prints phase markers to
  stderr after file parsing completes so large repos show progress during
  call graph and transitive propagation steps.
- **`asd list symbols/effects/ledger`** — enumerate indexed objects with
  optional filters.
- **`asd list stats`** — aggregate graph metrics: symbols by language/kind,
  effects by category and verification status, call graph edge counts, ledger totals.

### Fixed
- **`asd trace` help text** — clarified Python-only tracer constraint.
- **Always-on index log** — every run writes full per-file progress to
  `.asd/index.log`; `--verbose` tees to stderr; skipped files capped at 100
  on stderr.

---

## [v0.6.2] — 2026-05-05

### Added
- **`asd list symbols/effects/ledger`** — enumerate indexed objects with optional
  filters (`--lang`, `--kind`, `--file`, `--has-declared`, `--category`)
- **`asd list stats`** — aggregate graph metrics: symbol counts by language and
  kind, effect categories, verification status, call graph edge counts, ledger totals

### Fixed
- **`asd trace` help text** — clarified that the tracer is Python-only (uses
  `sys.settrace`); previously said "Run a Python program" which was misleading

---

## [v0.6.1] — 2026-05-05

### Added
- **Always-on index log** — every `asd index` run writes full per-file progress
  and all skipped files to `.asd/index.log`; `--verbose` tees that same output
  to stderr; skipped files capped at 100 on stderr with "…and N more" hint

---

## [v0.6.0] — 2026-05-05

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

## [v0.5.5] — 2026-05-05

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

## [v0.5.0] — 2026-05-04 (M19)

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

## [v0.4.5] — 2026-05-03 (M17–M18)

### Added
- OSS / commercial tier split: `approve`, `reject`, `withdraw` ledger operations gated behind `asd-pro`
- Audit log: hash-chained JSONL event stream for all ledger and policy mutations
- Audit tail: `asd audit tail` and `asd audit verify` CLI commands; `audit_tail` and `audit_verify` MCP tools
- Lens review UI: live streaming audit feed, tamper-evident badge, SPA routing

---

## [v0.4.0] — 2026-04-28 (M13–M16)

### Added
- Marketing and docs site (`agentstatedeveloper.dev`) built with Astro
- Lens review UI (`asd-serve`) with symbol inspector and ledger timeline
- 9-language semantic index: Python, TypeScript, Rust, Go, Java, C#, Ruby, Kotlin, Swift
- Swift and Kotlin adapters using bundled tree-sitter C grammars + `LanguageFn::from_raw`

---

## [v0.3.0] — 2026-04-20 (M1–M12)

### Added
- Core semantic index (tree-sitter), decision ledger, effect declarations, call graph
- Policy gate (file-backed JSON rules: allow, deny, require-approval)
- Ratification workflow (approve, reject, withdraw — commercial tier)
- MCP server (`asd-mcp`) exposing 14+ tools to coding agents
- HTTP server (`asd-serve`) for Lens UI
- CLI: `init`, `index`, `read`, `ledger`, `policy`, `verify-effects`, `trace`, `sync`, `hydrate`, `audit`, `hooks`
