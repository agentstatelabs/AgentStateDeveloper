//! Paste-to-your-agent bootstrap (suite-onboarding t-006).
//!
//! Prints a block the user drops into whatever coding agent they're already in;
//! the agent then installs, indexes, and connects ASD itself — and is pointed
//! at CTXone for the team layer. The lowest-friction install path, rendered
//! from ASD's single [`crate::commands::skill::asd_skill_spec`] via the shared
//! engine.

use anyhow::Result;
use clap::Args;

#[derive(Debug, Args)]
pub struct BootstrapArgs {}

pub fn run(_args: BootstrapArgs) -> Result<()> {
    match agent_skillgen::render_bootstrap(&crate::commands::skill::asd_skill_spec()) {
        Some(block) => print!("{block}"),
        None => println!("No bootstrap steps are defined."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::commands::skill::asd_skill_spec;

    #[test]
    fn asd_bootstrap_renders_real_steps() {
        let block = agent_skillgen::render_bootstrap(&asd_skill_spec())
            .expect("ASD declares bootstrap steps");
        assert!(block.contains("asd index ."));
        assert!(block.contains("asd mcp install"));
        assert!(block.contains("CTXone"), "dual: points at the team layer");
        // Every step the agent is told to run is a real command surface.
        assert!(block.contains("brew install asd"));
    }
}
