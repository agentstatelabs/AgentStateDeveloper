#!/usr/bin/env bash
# Cut a release of AgentStateDeveloper by tagging and pushing.
#
#   scripts/release.sh v1.0.0
#
# This script BUILDS NOTHING. All five platform builds, the GitHub release,
# and the Homebrew formula are produced by .github/workflows/release.yml on
# GitHub-hosted runners (macos-14 / ubuntu-22.04 / windows-latest). Pushing
# the tag is the entire trigger.
#
# Why it no longer builds locally:
#   Until v0.9.38 this script cross-compiled via cross-rs + Docker and
#   published the tarballs itself, duplicating CI. On the v0.9.38 release
#   both publishers ran — CI published at 04:42, this script rebuilt the
#   same commit and clobbered four of five assets at 04:51, leaving one
#   release with two build provenances. There is now exactly one publisher.
#
# What it does:
#   1. Refuse if the working tree is dirty or the branch is behind origin.
#   2. Verify the workspace version in Cargo.toml matches the tag.
#   3. Tag $VERSION on HEAD (annotated) if not already there.
#   4. Push the tag to GitLab origin and the public GitHub mirror.
#   5. Print the Actions run to watch.
#
# Env overrides:
#   SKIP_SYNC_CHECK=1    — don't require being level with origin/main
#   ALLOW_VERSION_SKEW=1 — permit Cargo.toml != tag (not recommended)

set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" || ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $0 vMAJOR.MINOR.PATCH"
  echo "  example: $0 v1.0.0"
  exit 64
fi
VER_NUM="${VERSION#v}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
GITHUB_REMOTE="https://github.com/agentstatelabs/AgentStateDeveloper.git"
ACTIONS_REPO="agentstatelabs/AgentStateDeveloper"

step()  { printf '\n\033[1;36m▸ %s\033[0m\n' "$*"; }
ok()    { printf '\033[32m  ✓ %s\033[0m\n' "$*"; }
warn()  { printf '\033[33m  ⚠ %s\033[0m\n' "$*"; }
fail()  { printf '\033[31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

cd "$REPO_ROOT"

# ---------------------------------------------------------------------------
# 1. Preflight
# ---------------------------------------------------------------------------
step "preflight"

[[ -z "$(git status --porcelain)" ]] || fail "working tree dirty — commit or stash first"
ok "working tree clean"

if [[ "${SKIP_SYNC_CHECK:-0}" != "1" ]]; then
  if git fetch --quiet origin main 2>/dev/null; then
    behind="$(git rev-list --count HEAD..origin/main 2>/dev/null || echo 0)"
    [[ "$behind" == "0" ]] || \
      fail "branch is $behind commit(s) behind origin/main — pull first (or SKIP_SYNC_CHECK=1)"
    ok "level with origin/main"
  else
    warn "could not fetch origin (offline?) — skipping sync check"
  fi
fi

# The tag names what users install; if Cargo.toml disagrees, `asd --version`
# reports a number that exists nowhere else. Check rather than bump — the
# workspace version is owned by the normal commit flow, not by this script.
CUR="$(awk '/^\[workspace\.package\]/{f=1;next} f&&/^version = /{gsub(/[",]/,"",$3);print $3;exit}' Cargo.toml)"
if [[ "$CUR" != "$VER_NUM" ]]; then
  if [[ "${ALLOW_VERSION_SKEW:-0}" == "1" ]]; then
    warn "Cargo.toml is $CUR but tagging $VER_NUM (ALLOW_VERSION_SKEW=1)"
  else
    fail "Cargo.toml workspace version is $CUR, not $VER_NUM — bump and commit it first"
  fi
else
  ok "Cargo.toml workspace version is $VER_NUM"
fi

# ---------------------------------------------------------------------------
# 2. Tag
# ---------------------------------------------------------------------------
step "tag $VERSION on HEAD"

HEAD_SHA="$(git rev-parse HEAD)"
if ! git rev-parse "$VERSION" >/dev/null 2>&1; then
  git tag -a "$VERSION" -m "$VERSION"
  ok "tagged $VERSION at $(git rev-parse --short HEAD)"
else
  # ^{commit} is required: the tag above is ANNOTATED, so a bare rev-parse
  # yields the tag object and never equals HEAD's commit.
  EXISTING="$(git rev-parse "$VERSION^{commit}")"
  [[ "$EXISTING" == "$HEAD_SHA" ]] || \
    fail "tag $VERSION already points at $EXISTING, but HEAD is $HEAD_SHA"
  warn "tag $VERSION already on HEAD — reusing"
fi

# ---------------------------------------------------------------------------
# 3. Push — GitLab is origin; GitHub is the mirror whose Actions build it.
#    GitLab CI also mirrors tags, but push directly too so the release does
#    not silently wait on a mirror job.
# ---------------------------------------------------------------------------
step "push $VERSION"
git push origin "$VERSION" 2>&1 | tail -2 | sed 's/^/  /'
ok "pushed to GitLab origin"
git push "$GITHUB_REMOTE" "$VERSION" 2>&1 | tail -2 | sed 's/^/  /'
ok "pushed to GitHub mirror"

step "done — CI is building $VERSION"
echo "  watch:    gh run watch -R $ACTIONS_REPO"
echo "  runs:     https://github.com/$ACTIONS_REPO/actions"
echo "  release:  https://github.com/agentstatelabs/agentstatedeveloper-releases/releases/tag/$VERSION"
echo
echo "  CI publishes the tarballs and pushes the Homebrew formula."
echo "  Nothing further to run locally."
