//! Minimal solo-developer config. Resolves the ASD database path from
//! (in priority order): CLI flag, `ASD_DB` env var, or `./.asd-state.db`.
//! Policy file from `--policy <path>` or `ASD_POLICY` env var (optional).

use std::path::PathBuf;

/// Default agent id recorded on ASG commits produced by the CLI.
pub const DEFAULT_AGENT_ID: &str = "asd-cli-user";

#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub agent_id: String,
    pub policy_path: Option<PathBuf>,
    pub audit_log_path: Option<PathBuf>,
    /// Plan D t-001: brief output mode. Set via `--brief` flag or
    /// `ASD_FORMAT=brief` env var. When true, commands project their
    /// JSON output down to load-bearing fields for ~60-80% token cut.
    pub brief: bool,
}

impl Config {
    /// Resolve config from optional explicit paths.
    pub fn resolve(
        explicit_db: Option<PathBuf>,
        explicit_policy: Option<PathBuf>,
        explicit_audit_log: Option<PathBuf>,
    ) -> Self {
        Self::resolve_with_brief(explicit_db, explicit_policy, explicit_audit_log, false)
    }

    pub fn resolve_with_brief(
        explicit_db: Option<PathBuf>,
        explicit_policy: Option<PathBuf>,
        explicit_audit_log: Option<PathBuf>,
        brief_flag: bool,
    ) -> Self {
        let db_path = explicit_db
            .or_else(|| std::env::var_os("ASD_DB").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("./.asd-state.db"));
        let policy_path = explicit_policy
            .or_else(|| std::env::var_os("ASD_POLICY").map(PathBuf::from));
        let audit_log_path = explicit_audit_log
            .or_else(|| std::env::var_os("ASD_AUDIT_LOG").map(PathBuf::from));
        let brief = brief_flag
            || std::env::var("ASD_FORMAT")
                .map(|v| v.eq_ignore_ascii_case("brief"))
                .unwrap_or(false);
        Self {
            db_path,
            agent_id: DEFAULT_AGENT_ID.to_string(),
            policy_path,
            audit_log_path,
            brief,
        }
    }
}
