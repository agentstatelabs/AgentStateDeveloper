# Cutting an `asd` release

Releases are built locally (no CI minutes burned). The one-command flow is
`scripts/release.sh vMAJOR.MINOR.PATCH`.

## What ships where

- **Source:** `github.com/agentstatelabs/AgentStateDeveloper`.
- **Release artifacts:**
  `github.com/agentstatelabs/agentstatedeveloper-releases/releases`
  (a tarball per target, plus a release entry per version).
- **Homebrew tap:**
  `github.com/agentstatelabs/homebrew-agentstatedeveloper`.
  End-user command:
  `brew tap agentstatelabs/agentstatedeveloper && brew install asd`.

## Coordination with the other agent

A second agent works the asd repo in parallel and pushes per-commit
workspace-version bumps (`feat(1.0.NN): ...`). That means **`Cargo.toml`
moves constantly**.

The release script deliberately **does not** touch `Cargo.toml`. You pick
the commit you want to ship, tag it, and the script ships that commit's
binaries — whatever version they happen to embed.

The cleanest pattern is to do release work in a fresh worktree off
`origin/main`:

```sh
git worktree add /tmp/asd-release origin/main
cd /tmp/asd-release
scripts/release.sh vX.Y.Z
```

That keeps you out of the other agent's uncommitted working tree.

## One-time prereqs

```sh
rustup target add x86_64-apple-darwin
cargo install cross --git https://github.com/cross-rs/cross
# Docker Desktop installed; must be running when building Linux targets
gh auth status   # agentstatelabs needs Contents:write on the releases repo

# Tap clone must exist as a sibling of AgentStateDeveloper:
git clone https://github.com/agentstatelabs/homebrew-agentstatedeveloper.git \
  ../homebrew-agentstatedeveloper
```

## Cutting a release

```sh
scripts/release.sh v1.0.94
```

The script:

1. Refuses if the working tree is dirty.
2. Tags `vX.Y.Z` on HEAD if not already present, pushes the tag to GitLab.
3. Builds `asd`, `asd-mcp`, `asd-serve` for four targets:
   - `aarch64-apple-darwin`
   - `x86_64-apple-darwin`
   - `x86_64-unknown-linux-gnu`
   - `aarch64-unknown-linux-gnu`
4. Tarballs each as `asd-<ver>-<target>.tar.gz`.
5. Creates the GitHub release and uploads all four tarballs.
6. Patches `Formula/asd.rb` in the sibling tap clone, commits, pushes to
   GitLab. GitLab → GitHub mirror replicates within seconds.

About ~7 minutes wall-clock; less on incremental rebuilds.

## Partial / recovery flags

| env var | effect |
|---------|--------|
| `SKIP_TAG=1` | use HEAD as-is; don't tag |
| `SKIP_LINUX=1` | only macOS targets |
| `SKIP_FORMULA=1` | leave the brew tap alone |
| `ONLY_TARGETS=a,b` | comma-separated subset |

Example — re-upload just the linux x86 tarball without touching anything
else:

```sh
SKIP_TAG=1 SKIP_FORMULA=1 ONLY_TARGETS=x86_64-unknown-linux-gnu \
  scripts/release.sh v1.0.93
```

`gh release upload --clobber` makes uploads idempotent.

## Rolling back

```sh
gh release delete v<X> -R agentstatelabs/agentstatedeveloper-releases
git push origin :refs/tags/v<X>      # remove tag from source
# tap commit lives in homebrew-agentstatedeveloper; revert + push as usual
```
