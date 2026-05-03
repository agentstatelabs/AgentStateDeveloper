//! `asd-pro` — AgentStateDeveloper commercial binary (Team + Enterprise tier).
//!
//! Thin wrapper over the OSS `agentstatedeveloper-cli` library. Before
//! dispatching it installs two commercial overrides:
//!
//! 1. [`JsonlFileSink`] as the audit sink when `--audit-log` / `ASD_AUDIT_LOG`
//!    is configured (Enterprise tier).
//! 2. [`RatifyOpsImpl`] so that every `open_engine` call automatically wires
//!    in the real approve/reject/withdraw (Team tier).
//!
//! All OSS subcommands are dispatched unchanged through the shared cli library.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use agentstatedeveloper_audit_pro::JsonlFileSink;
use agentstatedeveloper_cli::{Cli, set_audit_sink_override, set_ratify_ops_override};
use agentstatedeveloper_ratify::RatifyOpsImpl;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = agentstatedeveloper_cli::config_from_cli(&cli);

    // Enterprise tier: hash-chained audit sink.
    if let Some(ref path) = cfg.audit_log_path {
        set_audit_sink_override(Arc::new(JsonlFileSink::new(path.clone())));
    }

    // Team tier: real ledger approve/reject/withdraw via RatifyOpsImpl.
    set_ratify_ops_override(Arc::new(RatifyOpsImpl));

    agentstatedeveloper_cli::run(cli)
}
