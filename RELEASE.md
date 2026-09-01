# Cutting an `asd` release

Releases are built **by CI**, not locally. The one-command flow is
`scripts/release.sh vMAJOR.MINOR.PATCH`, and pushing the tag is the entire
trigger — the script itself builds nothing.

> Local building was retired in v0.9.38. Before that this script
> cross-compiled and published the tarballs itself, duplicating CI; on the
> v0.9.38 release both publishers ran and clobbered four of five assets,
> leaving one release with two build provenances. There is now exactly one
> publisher.

## What ships where

- **Source:** `github.com/agentstatelabs/AgentStateDeveloper`.
- **Release artifacts:**
  `github.com/agentstatelabs/agentstatedeveloper-releases/releases`
  (a tarball per target, plus a release entry per version).
- **Homebrew tap:**
  `github.com/agentstatelabs/homebrew-agentstatedeveloper`.
  End-user command:
  `brew tap agentstatelabs/agentstatedeveloper && brew install asd`.

## The version must already be committed

The release script **does not** touch `Cargo.toml`; the workspace version is
owned by the normal commit flow. But it does **verify** that the version in
`Cargo.toml` matches the tag, and refuses otherwise (`ALLOW_VERSION_SKEW=1`
overrides, not recommended).

So bump the workspace version and land it on `main` *before* tagging.

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
glab auth status   # push rights on the GitLab origin — that is the whole trigger
```

No Rust cross-targets, no `cross`, no Docker and no tap clone: those were
prerequisites of the retired local-build path.

## Cutting a release

```sh
scripts/release.sh v1.1.0
```

The script:

1. Refuses if the working tree is dirty or the branch is behind origin.
2. Verifies the workspace version in `Cargo.toml` matches the tag.
3. Tags `vX.Y.Z` on HEAD (annotated) if not already present.
4. Pushes the tag to GitLab. GitLab CI mirrors it to GitHub through the
   leak-scan gate, which fires `.github/workflows/release.yml`.
5. Prints the Actions run to watch.

Everything after that is CI: the five platform builds, the GitHub release,
and the Homebrew formula. The formula is rendered **GitLab-side** by the
`homebrew` job in `.gitlab-ci.yml` (see `scripts/publish-homebrew.sh`), which
commits it to the tap on GitLab; the tap's own publish job mirrors it to the
GitHub tap that `brew tap` reads. Never write the GitHub tap directly.

No sibling tap clone is needed any more.

## After the release: bump the site footer

The marketing site carries a hardcoded version string that nothing derives
from this repo. It will not update itself.

In `AgentStateDeveloper-site`, `src/layouts/Site.astro`, the `footer-bottom`
line:

```
AgentStateDeveloper v1.0.0 · BSL 1.1 → Apache 2.0 · © {year} AgentStateLabs, LLC.
```

Bump it, commit, push. The site deploys in two hops (GitLab CI mirrors to
GitHub, GitHub Actions builds Pages), so confirm the live page rather than
the pipeline — a green pipeline only means the mirror landed.

This is not hypothetical: agentstategraph.dev advertised `0.9.21` while the
real release was `0.9.24`, stale by three patches, because this step had no
home in a checklist.

## Partial / recovery flags

| env var | effect |
|---------|--------|
| `SKIP_SYNC_CHECK=1` | don't require being level with `origin/main` |
| `ALLOW_VERSION_SKEW=1` | permit `Cargo.toml` != tag (not recommended) |

These are the only two the script reads. `SKIP_TAG`, `SKIP_LINUX`,
`SKIP_FORMULA` and `ONLY_TARGETS` were documented here long after they stopped
existing — they belonged to the retired local-build path. Re-running a build
now means re-running the CI workflow, not passing flags to this script.

## Rolling back

```sh
gh release delete v<X> -R agentstatelabs/agentstatedeveloper-releases
git push origin :refs/tags/v<X>      # remove tag from source
# tap commit lives in homebrew-agentstatedeveloper; revert + push as usual
```
