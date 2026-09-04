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

  ```sh
  brew tap agentstatelabs/agentstatedeveloper
  brew trust agentstatelabs/agentstatedeveloper   # Homebrew 6.0+ only
  brew install asd
  ```

  The `brew trust` step is **not optional on Homebrew 6.0+**: it refuses to
  load a formula from an untrusted third-party tap, so without it `brew
  install asd` fails with *"Refusing to load formula ... from untrusted
  tap"*. An install that predates the gate keeps working, which is why this
  only shows up for new users — verify with a real uninstall/reinstall, not
  by upgrading an existing one.

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

## After the release: verify it is installable

CI runs `scripts/verify-release.sh` automatically on every tag (the
`verify-install` job, after `homebrew`). It checks the shell installer end to
end in a clean container, the tap formula against the published assets, and
that the documented commands are current.

Two limits worth knowing:

- It runs in `.post`, **after** publish. A failure alarms; it does not stop the
  release shipping. Verifying that a *published* artifact installs cannot
  happen before publishing it.
- It cannot exercise real Homebrew (the container has none), so the
  client-side gate that broke v1.1.0 — Homebrew 6.0 refusing untrusted taps —
  is only caught indirectly, by asserting the docs carry the `brew trust` step.

So on a Mac, once per release, run the destructive check that a container
cannot:

```sh
scripts/verify-release.sh vX.Y.Z --brew-clean
```

It untrusts the tap, uninstalls asd, runs the documented command exactly as a
new user would, and reinstalls. **Verify with an uninstall/reinstall, never an
upgrade** — an upgrade bypasses the trust gate and every other first-contact
failure, which is precisely how the v1.1.0 break reached users.

## Windows

Two different things cover Windows, and they prove different things.

**Automatic, on every tag.** `release.yml`'s `verify-windows` job (`needs:
publish`) installs the *real* published release on a Windows runner via
`scripts/verify-release-windows.ps1`, on **both** PowerShell 7 and Windows
PowerShell 5.1. It asserts a clean runner, that the tarball's sha256 matches
the published `SHA256SUMS`, that the three `.exe` files land, and that
`asd.exe --version` reports the tag. Like `verify-install`, it runs after
publish, so it alarms rather than prevents.

It is the only check on the Windows tarball's sum: the Homebrew formula pins a
sha256 per target but has **no Windows bottle**, so `verify-release.sh`'s
formula-vs-`SHA256SUMS` comparison skips that target entirely.

**Automatic, on changes to the installer.** `windows-installer.yml` runs
`scripts/test-install-ps1.ps1` against a local fixture release — 8 cases on
both shells, covering the checksum policy (match / tampered / absent / not
listed) and the encoding invariant. It proves `install.ps1`'s *logic*; it
cannot prove a real release installs, because the fixture supplies its own
tarball and sums.

### Re-proving the check still bites

A verification job that cannot fail is worse than none, because it is trusted.
Once in a while — and after any change to
`scripts/verify-release-windows.ps1` — run **Actions → verify-release-windows
→ Run workflow** against these three tags and confirm each behaves as stated:

| tag | expect | exercises |
|---|---|---|
| `v1.2.0` | **pass** | a good release; guards against a check that always fails |
| `v1.1.0` | **fail** | tarball present, sums absent — genuinely predates `SHA256SUMS`, so it must be caught as *"ships no checksums"*, not as propagation lag |
| `v9.9.9` | **fail** | neither asset — bad tag, the only case where raising the wait budget is the right advice |

A green on `v1.2.0` alone is not evidence. The `v1.1.0` red is the one that
shows the check can fail at all.

Not exercised by any of these, and worth knowing: the *sums present, tarball
absent* branch (no Windows target built) has no real tag to trigger it, and the
script's own "installer took the soft no-sums path" assertion is unreachable
while the asset gate stops first. `install.ps1`'s soft path itself is covered
by the `nosums` fixture case.

Two PowerShell rules that cost a round each, so they are written down rather
than rediscovered:

- **Keep every `.ps1` pure ASCII.** Windows PowerShell 5.1 reads a script in
  the system ANSI codepage unless it has a UTF-8 BOM, and box-drawing
  characters decode into smart quotes, which are *string delimiters*. This
  once made `install.ps1` completely unparseable under 5.1 while PowerShell 7
  read it fine. `test-install-ps1.ps1` now asserts the invariant.
- **Always run both shells.** 7 passed every case on the commit where 5.1 could
  not parse the file at all.

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
