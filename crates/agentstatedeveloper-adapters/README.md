# agentstatedeveloper-adapters

The default language adapter bundle for AgentStateDeveloper.
`default_adapters()` returns one instance of every built-in adapter: Python,
TypeScript/JavaScript, Rust, Go, Java, C#, Ruby, Kotlin and Swift. The CLI
(`asd index`) and the MCP server (the `reindex` tool) both use it, so they
always index with the same set.

Part of [AgentStateDeveloper](https://github.com/agentstatelabs/AgentStateDeveloper). Most users want the `asd` CLI
(`cargo install agentstatedeveloper-cli`) or the release binaries described
in the project README.

## License

[Business Source License 1.1](https://github.com/agentstatelabs/AgentStateDeveloper/blob/main/LICENSE) (SPDX: `BUSL-1.1`). Each version
converts to the Apache License 2.0 on its Change Date; see the license for the
Additional Use Grant and the exact terms.
