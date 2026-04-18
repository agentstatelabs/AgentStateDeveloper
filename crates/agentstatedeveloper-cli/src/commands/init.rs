//! `asd init` — create (or reuse) an ASD repository and stamp the
//! ASD schema-version marker at `/asd/v1/meta/schema-version`.

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use agentstatedeveloper_core::{paths, Engine, ASD_SCHEMA_VERSION};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct InitArgs {}

pub fn run(cfg: &Config, _args: InitArgs) -> Result<()> {
    // Ensure parent directory exists for the sqlite file.
    if let Some(parent) = cfg.db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    let engine = Engine::open_sqlite(&cfg.db_path)?;

    let path = paths::schema_version_path();
    let value = json!(ASD_SCHEMA_VERSION);
    let opts = CommitOptions::new(
        &cfg.agent_id,
        IntentCategory::Checkpoint,
        format!("stamp asd schema-version {}", ASD_SCHEMA_VERSION),
    );
    engine.repo.set_json(&engine.ref_name, &path, &value, opts)?;

    println!("initialized at {}", cfg.db_path.display());
    Ok(())
}
