# Homebrew tap

ASD distributes via Homebrew. The release pipeline publishes binaries to
`agentstatelabs/agentstatedeveloper-releases` and an automated workflow
renders `Formula/asd.rb` into `agentstatelabs/homebrew-agentstatedeveloper`.

## For users

```bash
brew tap agentstatelabs/agentstatedeveloper
brew install asd
```

Or in one step:

```bash
brew install agentstatelabs/agentstatedeveloper/asd
```

Upgrades:

```bash
brew update
brew upgrade asd
```

Uninstall:

```bash
brew uninstall asd
brew untap agentstatelabs/agentstatedeveloper
```

## Pipeline architecture

```
  GitLab origin  ──CI: fmt/clippy/build/test (.gitlab-ci.yml)
       │
       └─ mirror ──► agentstatelabs/AgentStateDeveloper (private GitHub)
                          │
                          └─ tag push (v*) ──► release.yml
                                                 │
                                                 ├─ builds 4 platform tarballs
                                                 ├─ publishes to ──► agentstatelabs/agentstatedeveloper-releases (public)
                                                 └─ triggers ───────► homebrew-tap.yml
                                                                          │
                                                                          ├─ sha256 each tarball
                                                                          ├─ render asd.rb.template
                                                                          └─ commit to ──► agentstatelabs/homebrew-agentstatedeveloper (public)
```

## Release artifact naming

Each tag publishes one tarball per platform target to
`agentstatelabs/agentstatedeveloper-releases`:

| Platform | Tarball name |
|---|---|
| macOS arm64 | `asd-{TAG}-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `asd-{TAG}-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 | `asd-{TAG}-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `asd-{TAG}-aarch64-unknown-linux-gnu.tar.gz` |

(`{TAG}` includes the leading `v`, e.g. `v1.1.14`.)

Each tarball contains a single top-level directory
`asd-{TAG}-{TARGET}/` with three executables at its root:
`asd`, `asd-mcp`, `asd-serve`. `tar -xzf … --strip-components=1`
flattens it for the curl-piped installer; Homebrew's auto-strip handles
it natively.

## Required secrets

The workflows expect two tokens on the main repo
(`agentstatelabs/AgentStateDeveloper`):

| Secret | Used by | What it is | Scope |
|---|---|---|---|
| `RELEASES_REPO_TOKEN` | `release.yml` (publish job) | GitHub fine-grained PAT | `repo Contents: read/write` on `agentstatelabs/agentstatedeveloper-releases` |
| `HOMEBREW_TAP_TOKEN`  | `homebrew-tap.yml` | GitHub fine-grained PAT | `repo Contents: read/write` on `agentstatelabs/homebrew-agentstatedeveloper` |

The ASG git dependencies are pulled from public GitHub
(`github.com/agentstatelabs/agentstategraph`), so no dependency-auth
token is required.

## Template substitutions

`asd.rb.template` uses `{{PLACEHOLDER}}` markers the workflow replaces:

| Placeholder | Example |
|---|---|
| `{{VERSION}}` | `1.1.14` |
| `{{TAG}}` | `v1.1.14` |
| `{{URL_DARWIN_ARM64}}` | `https://github.com/agentstatelabs/agentstatedeveloper-releases/releases/download/v1.1.14/asd-v1.1.14-aarch64-apple-darwin.tar.gz` |
| `{{SHA_DARWIN_ARM64}}` | (sha256 sum, 64 hex chars) |
| `{{URL_DARWIN_X86_64}}` / `{{SHA_DARWIN_X86_64}}` | (matching Intel pair) |
| `{{URL_LINUX_X86_64}}` / `{{SHA_LINUX_X86_64}}` | (Linux x86_64 pair) |
| `{{URL_LINUX_ARM64}}` / `{{SHA_LINUX_ARM64}}` | (Linux arm64 pair) |

Windows is NOT in the template — Homebrew is macOS/Linux only. Windows
users use `install.ps1`. (Windows builds are not yet in the release
matrix; add when needed.)

## Why a tap and not Homebrew core?

ASD is on the BSL-1.1 license, which Homebrew-core would not accept
without further review. A tap keeps distribution clean and under our
control, identical to CTXone's pattern.
