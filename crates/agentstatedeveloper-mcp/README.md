# agentstatedeveloper-mcp

The MCP server for AgentStateDeveloper. It installs two binaries:

- `asd-mcp`: exposes the asd tools to any MCP client over stdio, including
  Claude Code, Cursor, Codex, Gemini CLI and Zed.
- `asd-serve`: serves the same index and audit overlay over HTTP, for the
  browser UI.

```sh
cargo install agentstatedeveloper-mcp
```

Register `asd-mcp` with your agents using `asd mcp install` from
[`agentstatedeveloper-cli`](https://crates.io/crates/agentstatedeveloper-cli).

Part of [AgentStateDeveloper](https://github.com/agentstatelabs/AgentStateDeveloper). Most users want the `asd` CLI
(`cargo install agentstatedeveloper-cli`) or the release binaries described
in the project README.

## License

[Business Source License 1.1](https://github.com/agentstatelabs/AgentStateDeveloper/blob/main/LICENSE) (SPDX: `BUSL-1.1`). Each version
converts to the Apache License 2.0 on its Change Date; see the license for the
Additional Use Grant and the exact terms.
