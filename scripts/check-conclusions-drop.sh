#!/usr/bin/env bash
#
# check-conclusions-drop — fail a pipeline whose branch would DELETE conclusion
# records that already exist on the target branch.
#
# Why this exists
# ---------------
# `.asd/conclusions/*.jsonl` is regenerated wholesale by the pre-commit hook
# from whatever is in the LOCAL `.asd-state.db` at commit time. It is not
# append-only in effect. So a branch cut before someone else's MR merged
# carries a snapshot that predates it, and merging that branch silently
# REVERTS the records landed in between.
#
# The reconciliation designed for this — `merge_jsonl`, bound in
# .gitattributes as `merge=asdconclusions` — is a CUSTOM git merge driver.
# Custom drivers live in per-clone git config, registered by `asd init`.
# GitLab's server-side merge has no ASD binary and never runs `asd init`, so
# it cannot apply the driver: every merge through the web UI or API falls back
# to a plain text merge, and a clean-looking merge quietly drops records.
#
# Observed for real on MR !25, which would have reverted 20 records added by
# !24. Nothing flagged it. This turns that into a red pipeline.
#
# What it does NOT do
# -------------------
# Adding records is normal and never flagged. Only removal is, because removal
# is almost never intended — and when it is, say so explicitly (see ESCAPE
# HATCH below) rather than loosening the check.
#
# Deliberately needs no ASD binary: a set difference over parsed ids, so the
# job stays a few seconds on a tiny image rather than a Rust build.
#
# ESCAPE HATCH
#   Set ALLOW_CONCLUSIONS_DROP=1 as a CI variable for one run, or put
#   [conclusions-drop-ok] in the HEAD commit message, when a removal is
#   deliberate (a superseded conclusion, a purge). Both are auditable after
#   the fact, which "just make the check advisory" is not.
#
# Usage: check-conclusions-drop.sh [target-ref]   (default: origin/main)
set -euo pipefail

TARGET="${1:-origin/main}"
DIR=".asd/conclusions"

# On the target branch itself there is nothing to compare against.
CURRENT="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '')"
if [ "$CURRENT" = "${TARGET#origin/}" ]; then
  echo ">> on ${CURRENT}; nothing to compare against — skipping"
  exit 0
fi

if ! git rev-parse --verify --quiet "$TARGET" >/dev/null; then
  echo ">> ${TARGET} not available locally; fetching"
  git fetch --quiet origin "${TARGET#origin/}" || {
    echo "ERROR: cannot resolve ${TARGET}; is GIT_DEPTH=0 set on this job?" >&2
    exit 2
  }
fi

# Collect "<class>\t<id>" pairs for a ref, so a record dropped from one class
# is not masked by an unrelated addition in another.
ids_at() {
  local ref="$1"
  # NUL-delimited: a path with a space would silently split under word
  # expansion, and a class file quietly skipped is a missed drop.
  git ls-tree -r -z --name-only "$ref" -- "$DIR" 2>/dev/null |
  while IFS= read -r -d '' f; do
    case "$f" in *.jsonl) ;; *) continue ;; esac
    git show "${ref}:${f}" 2>/dev/null | python3 -c '
import json, sys, os
cls = os.path.basename(sys.argv[1]).removesuffix(".jsonl")
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        rec = json.loads(line)
    except json.JSONDecodeError:
        # A malformed line is its own problem; do not let it mask a drop.
        print(f"{cls}\t<unparseable>")
        continue
    if "id" in rec:
        print(f"{cls}\t" + str(rec["id"]))
' "$f"
  done
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
ids_at "$TARGET" | sort -u > "$TMP/target"
ids_at HEAD       | sort -u > "$TMP/head"
comm -23 "$TMP/target" "$TMP/head" > "$TMP/dropped"

DROPPED="$(wc -l < "$TMP/dropped" | tr -d ' ')"
ADDED="$(comm -13 "$TMP/target" "$TMP/head" | wc -l | tr -d ' ')"
echo ">> conclusions vs ${TARGET}: +${ADDED} added, -${DROPPED} dropped"

if [ "$DROPPED" -eq 0 ]; then
  echo ">> no records would be lost"
  exit 0
fi

echo
echo "The following conclusion records exist on ${TARGET} but NOT on this branch."
echo "Merging as-is would delete them:"
echo
sed 's/^/    /' "$TMP/dropped" | head -40
[ "$DROPPED" -gt 40 ] && echo "    … and $((DROPPED - 40)) more"
echo

if [ "${ALLOW_CONCLUSIONS_DROP:-}" = "1" ]; then
  echo ">> ALLOW_CONCLUSIONS_DROP=1 — treating this as deliberate."
  exit 0
fi
if git log -1 --pretty=%B | grep -qF '[conclusions-drop-ok]'; then
  echo ">> [conclusions-drop-ok] in the commit message — treating this as deliberate."
  exit 0
fi

cat >&2 <<MSG
ERROR: this branch would drop ${DROPPED} conclusion record(s) from ${TARGET}.

Almost always this means the branch is STALE: the sidecar is regenerated from
your local database, and yours predates records that landed on ${TARGET} while
you were working. Fix it locally, where the union merge driver exists:

    git fetch origin && git rebase ${TARGET}

Then re-check. If the rebase alone does not clear it — your local DB may simply
not hold those records — union the target's sidecar in explicitly:

    git show ${TARGET}:.asd/conclusions/decisions.jsonl > /tmp/theirs.jsonl
    cp .asd/conclusions/decisions.jsonl /tmp/base.jsonl
    asd conclusions merge /tmp/base.jsonl .asd/conclusions/decisions.jsonl /tmp/theirs.jsonl
    git commit --amend --no-edit .asd/conclusions/decisions.jsonl

Never hand-edit the JSONL to silence this.

If the removal IS deliberate, set ALLOW_CONCLUSIONS_DROP=1 on the run or put
[conclusions-drop-ok] in the commit message.
MSG
exit 1
