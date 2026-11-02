# agentstatedeveloper

Code-level context and an audit overlay for agent-authored code. ASD gives
every function a decision ledger, an effect declaration and a call graph.
Coding agents query all three, and they are checked into git so they travel
with every clone.

This crate is the library entry point. It re-exports
[`agentstatedeveloper-core`](https://crates.io/crates/agentstatedeveloper-core)
and `default_adapters()` from
[`agentstatedeveloper-adapters`](https://crates.io/crates/agentstatedeveloper-adapters).

```sh
cargo add agentstatedeveloper
```

| Crate | What it is |
|---|---|
| [`agentstatedeveloper-cli`](https://crates.io/crates/agentstatedeveloper-cli) | The `asd` command-line tool |
| [`agentstatedeveloper-mcp`](https://crates.io/crates/agentstatedeveloper-mcp) | The `asd-mcp` and `asd-serve` servers |
| [`agentstatedeveloper-core`](https://crates.io/crates/agentstatedeveloper-core) | Engine, schema and stores |
| [`agentstatedeveloper-adapters`](https://crates.io/crates/agentstatedeveloper-adapters) | The built-in language adapters |

Part of [AgentStateDeveloper](https://github.com/agentstatelabs/AgentStateDeveloper). Most users want the `asd` CLI
(`cargo install agentstatedeveloper-cli`) or the release binaries described
in the project README.

## License

[Business Source License 1.1](https://github.com/agentstatelabs/AgentStateDeveloper/blob/main/LICENSE) (SPDX: `BUSL-1.1`). Each version
converts to the Apache License 2.0 on its Change Date; see the license for the
Additional Use Grant and the exact terms.
