//! `asd scopes [list]` — discoverability for named scope aliases defined in
//! `.asd/scopes.toml`. Most field users have not realized that `--scope foo`
//! and `--paths "Packages/AudioEngine/**"` already work on search,
//! prepare-change, investigate, impact, checklist, and since. This command
//! makes the available scopes visible and prints a usage hint when the file
//! is missing.
//!
//! See Plan A, t-005.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;

use agentstatedeveloper_core::load_scope_aliases;

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum ScopesCmd {
    /// List named scope aliases defined in `.asd/scopes.toml`.
    List(ListArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit JSON instead of the default human-readable table.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, cmd: ScopesCmd) -> Result<()> {
    match cmd {
        ScopesCmd::List(args) => run_list(cfg, args),
    }
}

fn run_list(cfg: &Config, args: ListArgs) -> Result<()> {
    let aliases = load_scope_aliases(&cfg.db_path);

    if args.json {
        let payload = json!({
            "scopes": aliases,
            "count": aliases.len(),
            "scopes_file": cfg
                .db_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("scopes.toml")
                .display()
                .to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if aliases.is_empty() {
        println!("No named scopes defined.");
        println!();
        println!("Define them in .asd/scopes.toml to make `--scope <name>` work across");
        println!("search, prepare-change, investigate, impact, checklist, and since:");
        println!();
        println!("    # .asd/scopes.toml");
        println!("    audio-engine   = [\"Packages/AudioEngine/**\"]");
        println!("    drift-pad      = [\"App/**/DriftPad*\", \"Packages/SequencerCore/**\"]");
        println!();
        println!("Without a scope file you can still pass globs directly:");
        println!("    asd search 'master volume' --paths 'Packages/AudioEngine/**'");
        return Ok(());
    }

    println!("Named scopes ({}):", aliases.len());
    let mut names: Vec<&String> = aliases.keys().collect();
    names.sort();
    for name in names {
        let globs = &aliases[name];
        println!("  {name}");
        for g in globs {
            println!("    - {g}");
        }
    }
    println!();
    println!("Use with: --scope <name>  (works on search, prepare-change, investigate,");
    println!("impact, checklist, since.)");
    Ok(())
}
