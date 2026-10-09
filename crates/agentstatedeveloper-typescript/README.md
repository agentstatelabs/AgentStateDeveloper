# agentstatedeveloper-typescript

The TypeScript / JavaScript language adapter for AgentStateDeveloper. It implements
`LanguageAdapter` from
[`agentstatedeveloper-core`](https://crates.io/crates/agentstatedeveloper-core)
on top of `tree-sitter-typescript`: it parses TypeScript / JavaScript sources into symbols with qualified names, builds
call edges, infers effects, and detects cross-service endpoints.

You normally get it through
[`agentstatedeveloper-adapters`](https://crates.io/crates/agentstatedeveloper-adapters),
which bundles every built-in adapter.

Part of [AgentStateDeveloper](https://github.com/agentstatelabs/AgentStateDeveloper). Most users want the `asd` CLI
(`cargo install agentstatedeveloper-cli`) or the release binaries described
in the project README.

## License

[Business Source License 1.1](https://github.com/agentstatelabs/AgentStateDeveloper/blob/main/LICENSE) (SPDX: `BUSL-1.1`). Each version
converts to the Apache License 2.0 on its Change Date; see the license for the
Additional Use Grant and the exact terms.
