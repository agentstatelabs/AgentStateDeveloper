#!/usr/bin/env bash
#
# verify-release — prove a published release is actually installable.
#
# Everything this checks was found BY HAND on v1.1.0, and only because that
# release was verified with a genuine uninstall/reinstall instead of an
# upgrade. Two real breaks had already shipped:
#
#   1. Homebrew 6.0 refuses formulae from untrusted third-party taps, so the
#      documented `brew tap … && brew install asd` failed for every NEW user.
#      Invisible to anyone who already had asd — an existing install and
#      `brew upgrade` both bypass the gate entirely.
#   2. install.sh installed downloaded tarballs with no checksum verification
#      while the formula pinned sha256 per target. Both paths "worked", so
#      the asymmetry went unnoticed.
#
# The lesson is structural: a check that upgrades an existing install
# reproduces the blind spot rather than closing it. Every check here either
# runs against a clean location or asserts its own preconditions.
#
# Usage:
#   verify-release.sh <tag> [--shell] [--formula] [--docs] [--brew-clean]
#
#   (no mode flags)  run --shell, --formula and --docs
#   --shell          install via install.sh into a temp dir and verify
#   --formula        check the tap formula against the published assets
#   --docs           check the documented install commands are current
#   --brew-clean     DESTRUCTIVE: untrust the tap, uninstall asd, then run
#                    the documented command exactly as a new user would.
#                    Requires Homebrew. Reinstalls at the end. Never part of
#                    the default set, because it removes a working install.
#
# Exit: 0 all selected checks passed, 1 a check failed, 2 usage.

set -uo pipefail

TAG="${1:-}"
shift || true
if [[ -z "$TAG" || ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $0 vMAJOR.MINOR.PATCH [--shell] [--formula] [--docs] [--brew-clean]" >&2
  exit 2
fi
VER="${TAG#v}"

RELEASES_REPO="${ASD_RELEASES_REPO:-agentstatelabs/agentstatedeveloper-releases}"
TAP_RAW="https://raw.githubusercontent.com/agentstatelabs/homebrew-agentstatedeveloper/main/Formula/asd.rb"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DO_SHELL=0; DO_FORMULA=0; DO_DOCS=0; DO_BREW=0
while [ $# -gt 0 ]; do
  case "$1" in
    --shell)      DO_SHELL=1 ;;
    --formula)    DO_FORMULA=1 ;;
    --docs)       DO_DOCS=1 ;;
    --brew-clean) DO_BREW=1 ;;
    *) echo "verify-release: unknown flag $1" >&2; exit 2 ;;
  esac
  shift
done
if [ $((DO_SHELL + DO_FORMULA + DO_DOCS + DO_BREW)) -eq 0 ]; then
  DO_SHELL=1; DO_FORMULA=1; DO_DOCS=1
fi

FAILED=0
pass() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2; FAILED=1; }
note() { printf '       %s\n' "$1"; }
head_() { printf '\n\033[1m%s\033[0m\n' "$1"; }

sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# ── shell installer ─────────────────────────────────────────────────────────
if [ "$DO_SHELL" -eq 1 ]; then
  head_ "shell installer ($TAG)"
  TMPDIR_S=$(mktemp -d -t asd-verify.XXXXXX)
  trap 'rm -rf "$TMPDIR_S"' EXIT

  # The published script is what a user actually pipes into sh. If it has
  # drifted from the repo copy, the thing being tested is not the thing being
  # shipped.
  if curl -fsSL -o "$TMPDIR_S/published.sh" \
      "https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/install.sh" 2>/dev/null; then
    if [ "$(sha_of "$TMPDIR_S/published.sh")" = "$(sha_of "$REPO_ROOT/install.sh")" ]; then
      pass "published install.sh matches the repo copy"
    else
      fail "published install.sh has DRIFTED from the repo copy"
    fi
  else
    note "could not fetch the published install.sh (skipping drift check)"
  fi

  # Clean target: an empty dir, so nothing pre-existing can mask a failure.
  DEST="$TMPDIR_S/bin"; mkdir -p "$DEST"
  OUT="$TMPDIR_S/install.out"
  if ASD_VERSION="$TAG" INSTALL_DIR="$DEST" sh "$REPO_ROOT/install.sh" >"$OUT" 2>&1; then
    pass "install.sh exited 0"
  else
    fail "install.sh exited non-zero"
    sed 's/^/       | /' "$OUT" | tail -15 >&2
  fi

  for b in asd asd-mcp asd-serve; do
    if [ -x "$DEST/$b" ]; then pass "installed $b"; else fail "missing $b"; fi
  done

  if [ -x "$DEST/asd" ]; then
    GOT=$("$DEST/asd" --version 2>/dev/null | head -1)
    if printf '%s' "$GOT" | grep -q "$VER"; then
      pass "asd --version reports $VER ($GOT)"
    else
      fail "asd --version says '$GOT', expected $VER"
    fi
  fi

  # Checksum verification is the point of the SHA256SUMS work. A release that
  # publishes sums must VERIFY, not warn — a warning here means the publish
  # step silently did not run and the guard is inert.
  if curl -fsI "https://github.com/${RELEASES_REPO}/releases/download/${TAG}/SHA256SUMS" >/dev/null 2>&1; then
    if grep -q "checksum verified" "$OUT"; then
      pass "checksum verified against the published SHA256SUMS"
    else
      fail "SHA256SUMS is published for $TAG but the installer did not verify against it"
    fi
  else
    note "no SHA256SUMS published for $TAG — installer takes the warn path"
    if grep -q "No SHA256SUMS published" "$OUT"; then
      pass "installer warned about the missing sums (expected for pre-SHA256SUMS releases)"
    else
      fail "no sums published, but the installer did not say so"
    fi
  fi
fi

# ── tap formula vs published assets ─────────────────────────────────────────
if [ "$DO_FORMULA" -eq 1 ]; then
  head_ "homebrew formula ($TAG)"
  TMPDIR_F=$(mktemp -d -t asd-formula.XXXXXX)

  # The homebrew job pushes the formula to the GitLab tap, whose own publish
  # job mirrors it to the GitHub tap read here. Running straight after that
  # job therefore races the mirror. ASD_VERIFY_WAIT gives the mirror a bounded
  # budget (seconds) to land the expected version; 0 means check once.
  WAIT="${ASD_VERIFY_WAIT:-0}"
  WAITED=0
  while :; do
    curl -fsSL -o "$TMPDIR_F/asd.rb" "$TAP_RAW" 2>/dev/null || true
    if [ -s "$TMPDIR_F/asd.rb" ] \
       && [ "$(grep -m1 'version "' "$TMPDIR_F/asd.rb" | sed 's/.*"\(.*\)".*/\1/')" = "$VER" ]; then
      break
    fi
    [ "$WAITED" -ge "$WAIT" ] && break
    sleep 15
    WAITED=$((WAITED + 15))
  done
  [ "$WAITED" -gt 0 ] && note "waited ${WAITED}s for the tap mirror"

  if [ -s "$TMPDIR_F/asd.rb" ]; then
    pass "fetched the tap formula"

    # A formula that does not parse installs for nobody. Cheap to catch here;
    # it was caught by accident once, on a corrupted local checkout.
    if command -v ruby >/dev/null 2>&1; then
      if ruby -c "$TMPDIR_F/asd.rb" >/dev/null 2>&1; then
        pass "formula is valid ruby"
      else
        fail "formula does NOT parse as ruby"
      fi
    fi
    if grep -qE '^(<<<<<<<|=======|>>>>>>>)' "$TMPDIR_F/asd.rb"; then
      fail "formula contains git conflict markers"
    else
      pass "formula has no conflict markers"
    fi

    FVER=$(grep -m1 'version "' "$TMPDIR_F/asd.rb" | sed 's/.*"\(.*\)".*/\1/')
    if [ "$FVER" = "$VER" ]; then
      pass "formula version is $VER"
    else
      fail "formula version is '$FVER', expected $VER"
    fi

    # Cross-check the formula's pins against the release's own SHA256SUMS.
    # Both derive from the same artifacts, so disagreement means one of the
    # two publishers is looking at different bytes.
    if curl -fsSL -o "$TMPDIR_F/SHA256SUMS" \
        "https://github.com/${RELEASES_REPO}/releases/download/${TAG}/SHA256SUMS" 2>/dev/null; then
      MISMATCH=0; CHECKED=0
      while read -r sum name; do
        [ -n "$name" ] || continue
        PIN=$(grep -A1 -F "$name" "$TMPDIR_F/asd.rb" | grep sha256 | head -1 | sed 's/.*"\(.*\)".*/\1/')
        [ -n "$PIN" ] || continue
        CHECKED=$((CHECKED + 1))
        if [ "$PIN" != "$sum" ]; then
          MISMATCH=1
          note "$name: formula $PIN vs sums $sum"
        fi
      done < "$TMPDIR_F/SHA256SUMS"
      if [ "$CHECKED" -eq 0 ]; then
        note "no overlapping targets between formula and SHA256SUMS"
      elif [ "$MISMATCH" -eq 0 ]; then
        pass "formula sha256 pins agree with SHA256SUMS ($CHECKED targets)"
      else
        fail "formula sha256 pins DISAGREE with SHA256SUMS"
      fi
    else
      note "no SHA256SUMS to cross-check against (pre-SHA256SUMS release)"
    fi

    # Every url the formula names must actually resolve.
    while read -r url; do
      if curl -fsI "$url" >/dev/null 2>&1; then
        pass "asset exists: $(basename "$url")"
      else
        fail "asset MISSING: $url"
      fi
    done < <(grep -o 'https://[^"]*\.tar\.gz' "$TMPDIR_F/asd.rb" | sort -u)
  else
    fail "could not fetch the tap formula from $TAP_RAW"
  fi
  rm -rf "$TMPDIR_F"
fi

# ── documented commands ─────────────────────────────────────────────────────
if [ "$DO_DOCS" -eq 1 ]; then
  head_ "documented install commands"
  # Homebrew 6.0's trust gate broke the documented command for every new user.
  # The docs are what breaks, so assert on the docs.
  for f in README.md RELEASE.md packaging/homebrew/README.md; do
    p="$REPO_ROOT/$f"
    [ -f "$p" ] || continue
    if grep -q "brew install asd\|brew install agentstatelabs" "$p"; then
      if grep -q "brew trust" "$p"; then
        pass "$f documents the brew trust step"
      else
        fail "$f gives a brew install command without 'brew trust' — Homebrew 6.0+ refuses untrusted taps"
      fi
    fi
  done
fi

# ── destructive brew check ──────────────────────────────────────────────────
if [ "$DO_BREW" -eq 1 ]; then
  head_ "brew clean-install ($TAG) — DESTRUCTIVE"
  if ! command -v brew >/dev/null 2>&1; then
    fail "--brew-clean requires Homebrew"
  else
    HAD=$(brew list --versions asd 2>/dev/null | awk '{print $2}')
    note "current install: ${HAD:-none} (will be restored)"
    brew untrust agentstatelabs/agentstatedeveloper >/dev/null 2>&1 || true
    brew uninstall asd >/dev/null 2>&1 || true

    # Preconditions, asserted rather than assumed: this is the whole point.
    if command -v asd >/dev/null 2>&1; then
      fail "asd still on PATH after uninstall — not a clean precondition"
    else
      pass "precondition: no asd on PATH"
    fi

    if brew tap-info agentstatelabs/agentstatedeveloper 2>/dev/null | grep -q Untrusted; then
      pass "precondition: tap is untrusted"
    else
      note "tap trust state unclear; continuing"
    fi

    # The documented command, as a new user runs it.
    if brew tap agentstatelabs/agentstatedeveloper >/dev/null 2>&1 \
       && brew trust agentstatelabs/agentstatedeveloper >/dev/null 2>&1 \
       && brew install asd >/dev/null 2>&1; then
      GOT=$(asd --version 2>/dev/null | head -1)
      if printf '%s' "$GOT" | grep -q "$VER"; then
        pass "documented command installs $VER ($GOT)"
      else
        fail "installed, but version is '$GOT', expected $VER"
      fi
    else
      fail "the documented brew command FAILED on a clean precondition"
    fi
  fi
fi

echo
if [ "$FAILED" -eq 0 ]; then
  printf '\033[32mverify-release: all checks passed for %s\033[0m\n' "$TAG"
else
  printf '\033[31mverify-release: FAILURES for %s\033[0m\n' "$TAG" >&2
fi
exit "$FAILED"
