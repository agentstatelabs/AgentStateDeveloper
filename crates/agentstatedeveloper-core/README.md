# agentstatedeveloper-core

Core types, traits and storage for AgentStateDeveloper, independent of any
programming language:

- The schema: symbols, effects, ledger entries, feedback and scratch notes.
- The `LanguageAdapter` trait that language crates implement.
- The `Engine`, plus the index, effect, ledger and feedback stores. All of them
  are backed by [AgentStateGraph](https://crates.io/crates/agentstategraph).
- Search, context assembly, change preparation, cross-service endpoint
  matching, and repair.

Part of [AgentStateDeveloper](https://github.com/agentstatelabs/AgentStateDeveloper). Most users want the `asd` CLI
(`cargo install agentstatedeveloper-cli`) or the release binaries described
in the project README.

## License

[Business Source License 1.1](https://github.com/agentstatelabs/AgentStateDeveloper/blob/main/LICENSE) (SPDX: `BUSL-1.1`). Each version
converts to the Apache License 2.0 on its Change Date; see the license for the
Additional Use Grant and the exact terms.
