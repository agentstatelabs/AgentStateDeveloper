//! `asd` — AgentStateDeveloper OSS CLI. Thin wrapper over the library.

use clap::Parser;

fn main() {
    let cli = agentstatedeveloper_cli::Cli::parse();
    if let Err(e) = agentstatedeveloper_cli::run(cli) {
        // Silently exit on broken pipe (e.g. `asd list symbols | head`).
        for cause in e.chain() {
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                if io.kind() == std::io::ErrorKind::BrokenPipe {
                    std::process::exit(0);
                }
            }
        }
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
