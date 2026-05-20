//! `asd sidecar migrate` — one-shot migration from the old `.asd/v1/`
//! sidecar to the Plan B `.asd/conclusions/` layout.
//!
//! Ledger entries already live in the ASG repo (not the sidecar), so the
//! actual "drain" is just an `asd conclusions export` call. The work this
//! command adds is operational:
//!
//!   1. run export → write `.asd/conclusions/*.jsonl`
//!   2. measure both the new bytes and the old `.asd/v1/` bytes
//!   3. print the `git rm -r --cached .asd/v1` instructions the user needs
//!      to actually drop the old sidecar from tracking
//!
//! Lossy by design: anything derivable from source (symbols, effects,
//! call edges, code blobs) is not preserved. Only conclusion entries
//! survive, in their compact committable form.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::json;

use agentstatedeveloper_core::{conclusions_export, Engine};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum SidecarCmd {
    /// Migrate from the legacy .asd/v1/ sidecar to .asd/conclusions/*.jsonl.
    Migrate(MigrateArgs),
}

#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Output directory for the JSONL files. Defaults to `.asd/conclusions/`
    /// relative to the database parent directory.
    #[arg(long)]
    pub out: Option<std::path::PathBuf>,
}

pub fn run(cfg: &Config, cmd: SidecarCmd) -> Result<()> {
    match cmd {
        SidecarCmd::Migrate(args) => migrate(cfg, args),
    }
}

fn migrate(cfg: &Config, args: MigrateArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let project_root = cfg
        .db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let out_dir = args.out.unwrap_or_else(|| project_root.join(".asd/conclusions"));

    let counts = conclusions_export::export_all(&engine, &out_dir)?;
    let total_entries: usize = counts.iter().map(|(_, n, _)| n).sum();
    let new_bytes: u64 = counts.iter().map(|(_, _, b)| b).sum();

    let legacy_dir = project_root.join(".asd/v1");
    let legacy_present = legacy_dir.exists();
    let legacy_bytes = if legacy_present {
        dir_byte_count(&legacy_dir)
    } else {
        0
    };

    let tracked_in_git = legacy_present && git_tracks(&project_root, ".asd/v1");

    let mut next_steps: Vec<String> = Vec::new();
    if tracked_in_git {
        next_steps.push("git rm -r --cached .asd/v1".to_string());
        next_steps.push(
            "git commit -m 'plan-b: drop legacy .asd/v1/ sidecar (regenerable cache)'".to_string(),
        );
    } else if legacy_present {
        next_steps.push("rm -rf .asd/v1  # local-only directory; safe to delete".to_string());
    }
    next_steps.push("git add .asd/conclusions/".to_string());

    let payload = json!({
        "out_dir": out_dir.display().to_string(),
        "exported": {
            "total_entries": total_entries,
            "total_bytes": new_bytes,
            "per_class": counts.iter().map(|(stem, n, b)| json!({
                "class": stem,
                "entries": n,
                "bytes": b,
            })).collect::<Vec<_>>(),
        },
        "legacy_sidecar": {
            "path": legacy_dir.display().to_string(),
            "present": legacy_present,
            "tracked_in_git": tracked_in_git,
            "estimated_bytes": legacy_bytes,
        },
        "savings_bytes": legacy_bytes.saturating_sub(new_bytes),
        "next_steps": next_steps,
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

/// Recursively count bytes under `dir`. Errors are silently swallowed —
/// this is a best-effort report, not an audit.
fn dir_byte_count(dir: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = entry.metadata() {
            if meta.is_dir() {
                total = total.saturating_add(dir_byte_count(&path));
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

fn git_tracks(root: &std::path::Path, rel: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", rel])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn dir_byte_count_sums_nested_files() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        std::fs::write(tmp.path().join("a/x.txt"), "hello").unwrap();
        std::fs::write(tmp.path().join("a/b/y.txt"), "world!").unwrap();
        assert_eq!(dir_byte_count(tmp.path()), 11);
    }

    #[test]
    fn dir_byte_count_returns_zero_for_missing_dir() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert_eq!(dir_byte_count(&missing), 0);
    }
}
