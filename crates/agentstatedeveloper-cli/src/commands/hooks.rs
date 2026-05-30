//! `asd hooks` — show installed ASD git hooks, their triggers, commands,
//! and current status. Hooks are installed by `asd init` and live in
//! `.asd/hooks/` with `core.hooksPath` pointing git at that directory.

use anyhow::Result;
use clap::Args;

use crate::commands::init::{find_project_root, hook_statuses, hooks_path_is_set};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct HooksArgs {}

pub fn run(cfg: &Config, _args: HooksArgs) -> Result<()> {
    let root = find_project_root(&cfg.db_path);
    let statuses = hook_statuses(&root);
    let path_set = hooks_path_is_set(&root);

    println!("ASD git hooks (.asd/hooks/):\n");

    for h in &statuses {
        let indicator = if h.installed { "✓" } else { "✗" };
        println!("  {} {:<16} trigger:  {}", indicator, h.filename, h.trigger);
        println!("    {:<16} command:  {}", "", h.command);
        println!("    {:<16} purpose:  {}", "", h.purpose);
        println!();
    }

    let all_installed = statuses.iter().all(|h| h.installed);

    println!(
        "  core.hooksPath: {}",
        if path_set {
            ".asd/hooks  (active)"
        } else {
            "not set  (hooks are installed but NOT active)"
        }
    );

    if !all_installed || !path_set {
        println!("\n  To install/repair hooks: asd init");
    }

    Ok(())
}
