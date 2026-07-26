# Contributing to AgentStateDeveloper

Thanks for your interest — issues and pull requests are welcome.

## How this project is developed

AgentStateDeveloper is developed on a private GitLab instance and **mirrored,
read-only, to GitHub**. GitHub is the public home — it's where you file issues
and open pull requests, and it always reflects the current `main` and release
tags — but the canonical history lives on GitLab.

One consequence matters for contributors: **GitHub's `main` is force-advanced
from GitLab on every change, so pull requests are never merged with the GitHub
"Merge" button** (that would be overwritten on the next sync). Instead, accepted
changes are applied on the GitLab side by the project owner and then
re-published to GitHub. Your commits and authorship are preserved, and the PR is
closed with a link to the landed commit. If your merge doesn't come from the
GitHub button, that's the mirror model working — not a rejection.

## Feature requests and bugs

Open a **GitHub Issue**. For bugs, include a minimal reproduction, the version
(`asd --version`), and your platform. For feature requests, lead with the use
case — the "why" is what gets a change prioritized.

## Pull requests

1. Open a focused, single-purpose PR against `main` on GitHub.
2. Make sure it builds and tests pass:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D clippy::correctness
   cargo build --workspace --locked
   cargo test --workspace --locked
   ```
3. A maintainer reviews it. **All changes are merged by the project owner**, who
   applies the change on GitLab; the mirror then brings it to GitHub and the PR
   is closed as landed.

## Licensing of contributions

AgentStateDeveloper is licensed under the Business Source License 1.1
(BUSL-1.1). By contributing, you agree your contributions are licensed under the
same terms. See [LICENSING.md](LICENSING.md) for the plain-English summary. For
substantial contributions we may ask you to sign a contributor agreement first —
open the PR and we'll follow up.

## Questions?

Open an issue or start a discussion.
