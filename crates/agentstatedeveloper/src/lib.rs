//! AgentStateDeveloper: code-level context and an audit overlay for
//! agent-authored code.
//!
//! This is the library entry point. It re-exports everything from
//! [`agentstatedeveloper-core`](https://docs.rs/agentstatedeveloper-core)
//! (the engine, schema, stores and index pipeline) and
//! [`default_adapters`], which returns every built-in language adapter.
//!
//! ```
//! let adapters = agentstatedeveloper::default_adapters();
//! assert!(!adapters.is_empty());
//! ```
//!
//! The `asd` command-line tool is in `agentstatedeveloper-cli`, and the MCP
//! server is in `agentstatedeveloper-mcp`.

pub use agentstatedeveloper_adapters::default_adapters;
pub use agentstatedeveloper_core::*;
