//! Minimal solo-developer config. Resolves the ASD database path from
//! (in priority order): CLI flag, `ASD_DB` env var, or `./.asd-state.db`.

use std::path::PathBuf;

/// Default agent id recorded on ASG commits produced by the CLI.
pub const DEFAULT_AGENT_ID: &str = "asd-cli-user";

#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub agent_id: String,
}

impl Config {
    /// Resolve config from an optional explicit DB path.
    pub fn resolve(explicit_db: Option<PathBuf>) -> Self {
        let db_path = explicit_db
            .or_else(|| std::env::var_os("ASD_DB").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("./.asd-state.db"));
        Self {
            db_path,
            agent_id: DEFAULT_AGENT_ID.to_string(),
        }
    }
}
