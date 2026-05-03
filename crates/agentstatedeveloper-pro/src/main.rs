//! `asd-pro` — AgentStateDeveloper commercial binary (Team + Enterprise tier).
//!
//! Thin wrapper over the OSS `agentstatedeveloper-cli` library. Before
//! dispatching, it:
//!
//! 1. Installs [`JsonlFileSink`] as the audit sink when `--audit-log` /
//!    `ASD_AUDIT_LOG` is configured (Enterprise tier).
//! 2. Uses [`RatifyLedgerStore`] for ledger operations, enabling
//!    `ledger approve`, `ledger reject`, and `ledger withdraw` (Team tier).
//!
//! All OSS subcommands (`init`, `index`, `read`, `sync`, `hydrate`, `trace`,
//! `audit tail`) are handled by the shared library unchanged.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use agentstatedeveloper_audit_pro::JsonlFileSink;
use agentstatedeveloper_cli::{Cli, set_audit_sink_override};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Install the hash-chained audit sink if a log path is configured.
    // Must happen before any subcommand dispatch so the engine picks it up.
    let cfg = agentstatedeveloper_cli::config_from_cli(&cli);
    if let Some(ref path) = cfg.audit_log_path {
        let sink = Arc::new(JsonlFileSink::new(path.clone()));
        set_audit_sink_override(sink);
    }

    // Dispatch — OSS subcommands go through the shared library;
    // ratify operations (approve/reject/withdraw) are wired via
    // RatifyLedgerStore inside open_engine_public when the CLI lib
    // calls them. TODO(asd-pro): swap in RatifyLedgerStore for the
    // ledger store once the Engine exposes a ledger-store override hook.
    agentstatedeveloper_cli::run(cli)
}
