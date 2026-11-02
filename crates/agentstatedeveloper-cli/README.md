# agentstatedeveloper-cli

The `asd` command-line tool for AgentStateDeveloper. ASD gives every function a
decision ledger, an effect declaration and a call graph. Coding agents query
all three, and they are checked into git so they travel with every clone.

```sh
cargo install agentstatedeveloper-cli   # installs `asd`
asd init
asd index .
asd mcp install                         # register asd-mcp with your agents
```

`asd mcp install` registers the `asd-mcp` server from
[`agentstatedeveloper-mcp`](https://crates.io/crates/agentstatedeveloper-mcp),
so install that crate too. The crate also exposes its command set as a
library.

Part of [AgentStateDeveloper](https://github.com/agentstatelabs/AgentStateDeveloper). Most users want the `asd` CLI
(`cargo install agentstatedeveloper-cli`) or the release binaries described
in the project README.

## License

[Business Source License 1.1](https://github.com/agentstatelabs/AgentStateDeveloper/blob/main/LICENSE) (SPDX: `BUSL-1.1`). Each version
converts to the Apache License 2.0 on its Change Date; see the license for the
Additional Use Grant and the exact terms.
