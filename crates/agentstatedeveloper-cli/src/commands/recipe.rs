//! `asd recipe <recipe-name> <query>` — structured change-intent plans
//! (Plan C t-004). First concrete recipe: classify-test-migration. Sets
//! the pattern for future recipes (Plan C+).

use anyhow::Result;
use clap::{Args, Subcommand};

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, FtsFilters, SearchFtsDb, recipes::classify_test_migration,
};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum RecipeCmd {
    /// Classify test-tier symbols matching a query into migration actions
    /// (Delete / Gate / Run / KeepAsCovered / Review) based on their
    /// role-tagged ledger entries.
    #[command(name = "classify-test-migration")]
    ClassifyTestMigration(ClassifyArgs),
}

#[derive(Debug, Args)]
pub struct ClassifyArgs {
    /// Search query — finds candidate test symbols.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,

    /// Number of top candidate symbols to classify (default: 50).
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

pub fn run(cfg: &Config, cmd: RecipeCmd) -> Result<()> {
    match cmd {
        RecipeCmd::ClassifyTestMigration(args) => classify(cfg, args),
    }
}

fn classify(cfg: &Config, args: ClassifyArgs) -> Result<()> {
    let query = args.query.join(" ");
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let index = AsgIndexStore::from_engine(&engine);

    // Use the FTS index to find test-tier candidates that match the query.
    let fts = SearchFtsDb::open(&cfg.db_path).ok();
    let candidate_qnames: Vec<String> = if let Some(fts) = fts {
        let filters = FtsFilters {
            kind: None,
            language: None,
            include_tests: true,
            tests_only: true,
            exclude_terms: vec![],
            paths_filter: vec![],
        };
        fts.search(&query, &filters, args.limit)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.qname)
            .collect()
    } else {
        Vec::new()
    };

    let recipe = classify_test_migration(&engine, &index, &candidate_qnames, &query);
    println!("{}", serde_json::to_string_pretty(&recipe)?);
    Ok(())
}
