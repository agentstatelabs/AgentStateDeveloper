//! `asd` — AgentStateDeveloper OSS CLI. Thin wrapper over the library.

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = agentstatedeveloper_cli::Cli::parse();
    agentstatedeveloper_cli::run(cli)
}
