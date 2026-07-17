#!/usr/bin/env bash
# Cut a new local release of AgentStateDeveloper end-to-end.
#
#   scripts/release.sh v1.0.94
#
# What it does:
#   1. Refuses if the working tree is dirty (uses HEAD as-is — does NOT bump
#      Cargo.toml. The asd workspace version is managed elsewhere on a
#      per-commit cadence; just tag whatever HEAD already is.)
#   2. Tag $VERSION on HEAD if not already there, push to GitLab origin.
#   3. cargo build --release for four targets:
#        aarch64-apple-darwin   (native)
#        x86_64-apple-darwin    (cargo --target)
#        x86_64-unknown-linux-gnu  (via cross-rs, needs Docker running)
#        aarch64-unknown-linux-gnu (via cross-rs, needs Docker running)
#   4. Tarball each as asd-<ver>-<target>.tar.gz containing asd, asd-mcp,
#      asd-serve flat under one top-level dir.
#   5. gh release create on agentstatelabs/agentstatedeveloper-releases and
#      attach all four tarballs.
#   6. Patch the four URL + sha256 pairs in
#      Apps/homebrew-agentstatedeveloper/Formula/asd.rb, commit + push to
#      GitLab. The mirror replicates to GitHub within seconds.
#
# Note on the other agent:
#   The asd repo has a second active agent who keeps the workspace version
#   churning. This script does not touch Cargo.toml. Tag the commit whose
#   binary you want to ship.
#
# Prereqs (one-time):
#   - rustup target add x86_64-apple-darwin
#   - cargo install cross --git https://github.com/cross-rs/cross
#   - Docker Desktop installed (must be running when cross builds Linux)
#   - gh authenticated as agentstatelabs
#   - Apps/homebrew-agentstatedeveloper sibling clone exists with origin = GitLab
#
# Env overrides for partial runs:
#   SKIP_TAG=1           — don't tag (use when re-running for an existing tag)
#   SKIP_LINUX=1         — only macOS targets
#   SKIP_FORMULA=1       — leave the brew tap alone
#   ONLY_TARGETS=a,b     — comma-separated subset

set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" || ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $0 vMAJOR.MINOR.PATCH"
  echo "  example: $0 v1.0.94"
  exit 64
fi
VER_NUM="${VERSION#v}"

# ---------------------------------------------------------------------------
# Paths and helpers
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TAP_ROOT="$(cd "$REPO_ROOT/../homebrew-agentstatedeveloper" 2>/dev/null && pwd || \
            cd "$REPO_ROOT/../../homebrew-agentstatedeveloper" 2>/dev/null && pwd || \
            echo "$HOME/homebrew-agentstatedeveloper")"
RELEASE_REPO="agentstatelabs/agentstatedeveloper-releases"
TAP_REMOTE="https://github.com/agentstatelabs/homebrew-agentstatedeveloper.git"

ALL_TARGETS=(
  aarch64-apple-darwin
  x86_64-apple-darwin
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
)

if [[ -n "${ONLY_TARGETS:-}" ]]; then
  IFS=',' read -ra TARGETS <<<"$ONLY_TARGETS"
elif [[ "${SKIP_LINUX:-0}" == "1" ]]; then
  TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
else
  TARGETS=("${ALL_TARGETS[@]}")
fi

step()  { printf '\n\033[1;36m▸ %s\033[0m\n' "$*"; }
ok()    { printf '\033[32m  ✓ %s\033[0m\n' "$*"; }
warn()  { printf '\033[33m  ⚠ %s\033[0m\n' "$*"; }
fail()  { printf '\033[31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 1. Tag the source repo (no Cargo.toml bump — the other agent owns versions)
# ---------------------------------------------------------------------------
cd "$REPO_ROOT"

if [[ "${SKIP_TAG:-0}" != "1" ]]; then
  step "tag $VERSION on HEAD"
  if [[ -n "$(git status --porcelain)" ]]; then
    fail "working tree dirty — commit or stash first (this script does not touch Cargo.toml)"
  fi
  if ! git rev-parse "$VERSION" >/dev/null 2>&1; then
    git tag -a "$VERSION" -m "$VERSION"
    ok "tagged $VERSION at $(git rev-parse --short HEAD)"
  else
    EXISTING=$(git rev-parse "$VERSION")
    HEAD_SHA=$(git rev-parse HEAD)
    if [[ "$EXISTING" != "$HEAD_SHA" ]]; then
      fail "tag $VERSION already exists pointing at $EXISTING, but HEAD is $HEAD_SHA"
    fi
    warn "tag $VERSION already on HEAD"
  fi
  git push origin "$VERSION" 2>&1 | tail -3 | sed 's/^/  /'
else
  ok "SKIP_TAG=1 — using HEAD; no tag"
fi

# ---------------------------------------------------------------------------
# 2. Build all requested targets
# ---------------------------------------------------------------------------
STAGE="$(mktemp -d -t asd-release-$VERSION.XXXXXX)"
trap 'rm -rf "$STAGE"' EXIT
# bash 3.2-compatible (no associative arrays).
SHAS=()

build_target() {
  local target="$1"
  step "build $target"
  case "$target" in
    *apple-darwin)
      cargo build --release --target "$target" \
        --bin asd --bin asd-mcp --bin asd-serve
      ;;
    *linux-gnu)
      command -v cross >/dev/null || fail "cross not installed (cargo install cross)"
      docker info >/dev/null 2>&1 || fail "Docker not running (open -a Docker)"
      cross build --release --target "$target" \
        --bin asd --bin asd-mcp --bin asd-serve
      ;;
    *)
      fail "unknown target: $target"
      ;;
  esac
}

tarball_target() {
  local target="$1"
  local stem="asd-$VERSION-$target"
  local src="$REPO_ROOT/target/$target/release"
  local d="$STAGE/$stem"
  mkdir -p "$d"
  cp "$src/asd" "$src/asd-mcp" "$src/asd-serve" "$d/"
  ( cd "$STAGE" && tar -czf "$stem.tar.gz" "$stem" )
  shasum -a 256 "$STAGE/$stem.tar.gz" | awk '{print $1}'
}

for i in "${!TARGETS[@]}"; do
  t="${TARGETS[$i]}"
  build_target "$t"
  SHAS[$i]=$(tarball_target "$t")
  ok "asd-$VERSION-$t.tar.gz  sha=${SHAS[$i]}"
done

# ---------------------------------------------------------------------------
# 3. GitHub release
# ---------------------------------------------------------------------------
step "GitHub release $VERSION on $RELEASE_REPO"
PRIOR_ACCOUNT=$(gh auth status 2>&1 | awk '/Active account: true/{f=1} /Logged in/{a=$NF} END{print a}' || echo "")
gh auth switch -u agentstatelabs >/dev/null

if gh release view "$VERSION" -R "$RELEASE_REPO" >/dev/null 2>&1; then
  ok "release $VERSION already exists; appending assets"
else
  gh release create "$VERSION" -R "$RELEASE_REPO" \
    --title "asd $VERSION" \
    --notes "Release built locally via scripts/release.sh. Binaries: asd, asd-mcp, asd-serve." \
    >/dev/null
  ok "created release"
fi

for t in "${TARGETS[@]}"; do
  asset="$STAGE/asd-$VERSION-$t.tar.gz"
  gh release upload "$VERSION" -R "$RELEASE_REPO" --clobber "$asset" >/dev/null
  ok "uploaded $(basename "$asset")"
done

if [[ -n "$PRIOR_ACCOUNT" && "$PRIOR_ACCOUNT" != "agentstatelabs" ]]; then
  gh auth switch -u "$PRIOR_ACCOUNT" >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# 4. Patch + push the brew formula
# ---------------------------------------------------------------------------
if [[ "${SKIP_FORMULA:-0}" == "1" ]]; then
  ok "SKIP_FORMULA=1 — leaving tap alone"
  exit 0
fi

step "update tap formula"
[[ -d "$TAP_ROOT" ]] || fail "tap clone not at $TAP_ROOT — clone it: \`git clone $TAP_REMOTE $TAP_ROOT\`"

cd "$TAP_ROOT"
git pull --ff-only origin main >/dev/null 2>&1 || true

FORMULA="$TAP_ROOT/Formula/asd.rb"
[[ -f "$FORMULA" ]] || fail "formula not at $FORMULA"

python3 - <<PY
import re
p = "$FORMULA"
src = open(p).read()
ver = "$VER_NUM"
src = re.sub(r'^(\s*version\s+)"[^"]*"', rf'\1"{ver}"', src, count=1, flags=re.M)
shas = [
$(for i in "${!TARGETS[@]}"; do printf '    ("%s", "%s"),\n' "${TARGETS[$i]}" "${SHAS[$i]}"; done)
]
for target, sha in shas:
    pat = re.compile(
        r'(url "https://github\.com/agentstatelabs/agentstatedeveloper-releases/releases/download/)[^/]+/(asd-)[^"]+(-' +
        re.escape(target) + r'\.tar\.gz"\s+sha256 ")[^"]*(")'
    )
    repl = lambda m: m.group(1) + 'v' + ver + '/' + m.group(2) + 'v' + ver + m.group(3) + sha + m.group(4)
    src, n = pat.subn(repl, src, count=1)
    if n == 0:
        raise SystemExit(f"could not find URL+sha pair for {target} in formula")
open(p, "w").write(src)
PY

if [[ -n "$(git status --porcelain Formula/asd.rb)" ]]; then
  git add Formula/asd.rb
  git -c user.email="agentstatelabs@users.noreply.github.com" \
      -c user.name="agentstatelabs" \
      commit -m "asd $VER_NUM: bump version + sha256s" >/dev/null
  git push origin main 2>&1 | tail -2 | sed 's/^/  /'
  ok "formula updated and pushed (mirror replicates within ~seconds)"
else
  ok "formula already at $VERSION with matching sha256s"
fi

step "done — $VERSION shipped"
echo "  release:   https://github.com/$RELEASE_REPO/releases/tag/$VERSION"
echo "  install:   brew tap agentstatelabs/agentstatedeveloper && brew install asd"
