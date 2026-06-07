# Homebrew tap

ASD distributes via Homebrew once the release workflow publishes a
rendered `asd.rb` to the `agentstatelabs/homebrew-tap` repository.

## For users

```bash
brew tap agentstatelabs/tap
brew install asd
```

Or in one step:

```bash
brew install agentstatelabs/tap/asd
```

Upgrades:

```bash
brew update
brew upgrade asd
```

Uninstall:

```bash
brew uninstall asd
brew untap agentstatelabs/tap
```

## For maintainers

1. **Create the tap repository once.** A GitHub repo at
   `agentstatelabs/homebrew-tap` with this structure:

   ```
   homebrew-tap/
   └── Formula/
       └── asd.rb
   ```

2. **Create a PAT** with repo write access to
   `agentstatelabs/homebrew-tap` and save it as the
   `HOMEBREW_TAP_TOKEN` secret on the main ASD repo.

3. **Enable the `homebrew-tap` release workflow.** On every tag push
   it should:
   - read the freshly-published GitHub release,
   - compute `sha256` sums for each macOS / Linux tarball,
   - render `asd.rb.template` with those values,
   - commit `Formula/asd.rb` to the tap repo.

## Release artifact naming

The template expects tarballs named with the Rust target triple:

| Platform | Tarball name |
|---|---|
| macOS arm64 | `asd-{VERSION}-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `asd-{VERSION}-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 | `asd-{VERSION}-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `asd-{VERSION}-aarch64-unknown-linux-gnu.tar.gz` |

Each tarball must contain three binaries at the top level:
`asd`, `asd-mcp`, `asd-serve`.

The curl-piped `install.sh` consumes a slightly different layout — flat
per-binary downloads at
`releases/download/{TAG}/{bin}-{TARGET}` without the tarball wrapper.
The release CI publishes both formats so users can pick either
distribution channel.

## Template substitutions

`asd.rb.template` uses `{{PLACEHOLDER}}` markers the workflow replaces:

| Placeholder | Example |
|---|---|
| `{{VERSION}}` | `1.1.13` |
| `{{URL_DARWIN_ARM64}}` | `https://github.com/agentstatelabs/asd/releases/download/v1.1.13/asd-1.1.13-aarch64-apple-darwin.tar.gz` |
| `{{SHA_DARWIN_ARM64}}` | `abc123…` |
| `{{URL_DARWIN_X86_64}}` | (matching Intel URL) |
| `{{SHA_DARWIN_X86_64}}` | (sha256) |
| `{{URL_LINUX_X86_64}}` | (Linux x86_64 URL) |
| `{{SHA_LINUX_X86_64}}` | (sha256) |
| `{{URL_LINUX_ARM64}}` | (Linux arm64 URL) |
| `{{SHA_LINUX_ARM64}}` | (sha256) |

The template does NOT include Windows — Homebrew is macOS and Linux only.
Windows users should use `install.ps1`.

## Why a tap and not Homebrew core?

ASD is on the BSL-1.1 license, which Homebrew-core would not accept
without further review. A tap keeps distribution clean and under
control, identical to CTXone's pattern.
