#!/usr/bin/env bash
#
# wait-pipeline — wait for the CI pipeline belonging to a SPECIFIC COMMIT.
#
# Why this exists, and why you must not "simplify" it back to a branch query:
#
#   glab ci status --branch <b>
#
# answers "the latest pipeline on this ref", NOT "the pipeline for this commit".
# Immediately after a push those differ: GitLab has not created the new pipeline
# yet, so the branch query returns the PREVIOUS commit's terminal state. Any
# automation that pushes and then polls in the same breath reads a stale
# success and can merge a SHA whose pipeline never ran.
#
# Observed for real on MR !29: a watcher reported success after one minute.
# That success was pipeline #1250 for the PRE-REBASE sha abe2d3d, while the
# real pipeline #1256 for e29581c was still pending.
#
# The fix is to ask for pipelines belonging to the sha, and to treat an empty
# result as "not created yet, keep waiting" rather than as a terminal state.
# That distinction is the whole point of this script.
#
# This repo force-pushes branches routinely to reconcile the conclusions
# sidecar (see DESIGN.md, "The conclusions sidecar is regenerated, not
# appended"), so push-then-poll is the normal path here, not an edge case.
#
# Usage:
#   wait-pipeline.sh <sha> [--timeout SECONDS] [--interval SECONDS]
#   wait-pipeline.sh --mr <iid> [...]        resolve the sha from an MR head
#
# Exit: 0 success, 1 failed/canceled, 2 usage, 3 timed out.

set -euo pipefail

sha=""
mr=""
timeout=3600
interval=30

while [ $# -gt 0 ]; do
  case "$1" in
    --mr)       mr="${2:-}";       shift 2 ;;
    --timeout)  timeout="${2:-}";  shift 2 ;;
    --interval) interval="${2:-}"; shift 2 ;;
    -h|--help)  sed -n '2,30p' "$0"; exit 2 ;;
    -*)         echo "wait-pipeline: unknown flag $1" >&2; exit 2 ;;
    *)          sha="$1";          shift ;;
  esac
done

if [ -n "$mr" ]; then
  sha=$(glab mr view "$mr" --output json | python3 -c 'import sys,json; print(json.load(sys.stdin)["sha"])')
  echo "wait-pipeline: !$mr head is $sha"
fi

if [ -z "$sha" ]; then
  echo "wait-pipeline: need a sha or --mr <iid>" >&2
  exit 2
fi

deadline=$(( $(date +%s) + timeout ))

while :; do
  # An empty list means GitLab has not created the pipeline yet. That is NOT
  # a terminal state — it is the exact window the branch query gets wrong.
  status=$(glab api "projects/:id/pipelines?sha=$sha" 2>/dev/null \
    | python3 -c 'import sys,json; ps=json.load(sys.stdin); print(ps[0]["status"] if ps else "not-created")' \
    2>/dev/null || echo "unreachable")

  case "$status" in
    success)
      echo "wait-pipeline: $sha success"
      exit 0 ;;
    failed|canceled)
      echo "wait-pipeline: $sha $status" >&2
      exit 1 ;;
    not-created|unreachable|created|waiting_for_resource|preparing|pending|running|manual|scheduled)
      : ;;
    *)
      # Unknown status: keep waiting rather than guessing it is terminal.
      : ;;
  esac

  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "wait-pipeline: timed out after ${timeout}s waiting on $sha (last: $status)" >&2
    exit 3
  fi

  echo "wait-pipeline: $sha $status — waiting ${interval}s"
  sleep "$interval"
done
