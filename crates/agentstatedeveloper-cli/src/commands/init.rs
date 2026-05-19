//! `asd init [--no-hooks]` — create (or reuse) an ASD repository, stamp
//! the schema-version marker, install git hooks, and update .gitignore.
//!
//! Git hooks are written to `.asd/hooks/` and activated via
//! `git config core.hooksPath .asd/hooks`. This means the hook scripts
//! travel with the repo and any contributor who runs `asd init` gets
//! them automatically. Pass `--no-hooks` to skip hook installation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde_json::json;

use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use agentstatedeveloper_core::{paths, Engine, ASD_SCHEMA_VERSION};

use crate::config::Config;

// ---------------------------------------------------------------------------
// Hook definitions — single source of truth for scripts + metadata
// ---------------------------------------------------------------------------

struct HookDef {
    filename: &'static str,
    trigger: &'static str,
    command: &'static str,
    purpose: &'static str,
    script: &'static str,
}

const HOOKS: &[HookDef] = &[
    HookDef {
        filename: "pre-commit",
        trigger: "git commit",
        command: "asd sync --prune",
        purpose: "flush ledger/effects to the local .asd/v1/ sidecar (not committed)",
        script: "#!/usr/bin/env sh
# ASD pre-commit hook — installed by `asd init`
# Flushes live ASG state into the local .asd/v1/ sidecar so it stays in
# sync with the worktree. The sidecar is gitignored (it is a derived
# cache that can grow to tens of MB); we do not stage it with the commit.
# A compact, committable conclusions schema is on the roadmap (Plan B).
set -e
asd sync --prune
",
    },
    HookDef {
        filename: "post-merge",
        trigger: "git merge / git pull",
        command: "asd hydrate && asd index .",
        purpose: "load new .asd/v1/ entries into local db and rebuild index",
        script: "#!/usr/bin/env sh
# ASD post-merge hook — installed by `asd init`
# Loads any new sidecar entries from the merged branch into the local
# ASG database, then rebuilds the derived semantic index.
set -e
asd hydrate
asd index .
",
    },
    HookDef {
        filename: "post-checkout",
        trigger: "git checkout / git switch",
        command: "asd hydrate && asd index .",
        purpose: "sync local db to the checked-out branch's sidecar state",
        script: "#!/usr/bin/env sh
# ASD post-checkout hook — installed by `asd init`
# Syncs the local ASG database to the sidecar state of the branch
# you just switched to, then rebuilds the derived semantic index.
# $3 is 1 for branch checkout, 0 for file checkout — only run on branch.
[ \"$3\" = \"1\" ] || exit 0
set -e
asd hydrate
asd index .
",
    },
    HookDef {
        filename: "post-commit",
        trigger: "git commit",
        command: "asd annotate-commit --write HEAD && asd index .",
        purpose: "attach semantic residue from commit message to touched symbols; rebuild index",
        script: "#!/usr/bin/env sh
# ASD post-commit hook — installed by `asd init`
# Attaches semantic residue from the commit message to touched symbols
# (decisions, invariants, hazards, proofs, validation outcomes), then
# rebuilds the FTS index so queries immediately reflect the new state.
SHA=$(git rev-parse HEAD 2>/dev/null)
if [ -n \"$SHA\" ]; then
    asd annotate-commit --write \"$SHA\" 2>/dev/null || true
fi
asd index . 2>/dev/null || true
",
    },
];

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Skip git hook installation. Hooks can be installed later by
    /// re-running `asd init` without this flag.
    #[arg(long, default_value_t = false)]
    pub no_hooks: bool,
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

pub fn run(cfg: &Config, args: InitArgs) -> Result<()> {
    // Ensure parent directory exists for the sqlite file.
    if let Some(parent) = cfg.db_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
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

    // Locate the project root (directory containing the db file, or cwd).
    let project_root = cfg
        .db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Update .gitignore.
    update_gitignore(&project_root)?;

    // Install hooks unless --no-hooks.
    if args.no_hooks {
        println!("\ngit hooks: skipped (--no-hooks). Re-run `asd init` to install.");
    } else {
        install_hooks(&project_root)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// .gitignore management
// ---------------------------------------------------------------------------

fn update_gitignore(root: &Path) -> Result<()> {
    let gi_path = root.join(".gitignore");
    let existing = if gi_path.exists() {
        fs::read_to_string(&gi_path).unwrap_or_default()
    } else {
        String::new()
    };

    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut changed = false;

    // Ensure the SQLite db is ignored.
    if !lines.iter().any(|l| l.trim() == ".asd-state.db") {
        lines.push(".asd-state.db".to_string());
        changed = true;
    }

    // Ensure the derived sidecar cache is ignored. It is large (tens of MB on
    // real projects), regenerable from the source tree via `asd index .`, and
    // not yet shaped for shared repo memory. A compact conclusions schema is
    // planned (Plan B); until then `.asd/v1/` stays local.
    if !lines.iter().any(|l| l.trim() == ".asd/v1/") {
        lines.push(".asd/v1/".to_string());
        changed = true;
    }

    if changed {
        let content = lines.join("\n") + "\n";
        fs::write(&gi_path, content)
            .with_context(|| format!("failed to write {}", gi_path.display()))?;
        println!(
            ".gitignore: updated (.asd-state.db and .asd/v1/ ignored — both are local derived state)"
        );
    }

    // If the sidecar is already tracked from a prior install, tell the user
    // how to untrack it. Don't run `git rm` ourselves — destructive ops belong
    // to the user.
    let sidecar_tracked = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", ".asd/v1"])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if sidecar_tracked {
        println!();
        println!("  NOTE: .asd/v1/ is currently tracked in git from a prior install.");
        println!("  To untrack it without deleting your local copy, run:");
        println!("      git rm -r --cached .asd/v1");
        println!("      git commit -m 'stop tracking .asd/v1/ sidecar (regenerable cache)'");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

fn install_hooks(root: &Path) -> Result<()> {
    let hooks_dir = root.join(".asd/hooks");
    fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("failed to create {}", hooks_dir.display()))?;

    let mut installed: Vec<&HookDef> = Vec::new();

    for hook in HOOKS {
        let path = hooks_dir.join(hook.filename);
        fs::write(&path, hook.script)
            .with_context(|| format!("failed to write hook {}", path.display()))?;
        // Make executable (owner + group + other execute bits).
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
        installed.push(hook);
    }

    // Point git at our hooks directory.
    let status = std::process::Command::new("git")
        .args(["config", "core.hooksPath", ".asd/hooks"])
        .current_dir(root)
        .status();

    let hooks_path_set = matches!(status, Ok(s) if s.success());

    // Print the hook table.
    println!("\nASD git hooks installed (.asd/hooks/):\n");
    for hook in &installed {
        println!("  {:<16} trigger:  {}", hook.filename, hook.trigger);
        println!("  {:<16} command:  {}", "", hook.command);
        println!("  {:<16} purpose:  {}", "", hook.purpose);
        println!();
    }

    if hooks_path_set {
        println!("  core.hooksPath → .asd/hooks  (hooks are now active)");
    } else {
        println!("  WARNING: could not set core.hooksPath automatically.");
        println!("  Run manually: git config core.hooksPath .asd/hooks");
    }

    println!("\n  To skip hook installation: asd init --no-hooks");
    println!("  To review hooks later:     asd hooks");

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper used by `asd hooks` to read installed hook status
// ---------------------------------------------------------------------------

pub struct HookStatus {
    pub filename: &'static str,
    pub trigger: &'static str,
    pub command: &'static str,
    pub purpose: &'static str,
    pub installed: bool,
}

pub fn hook_statuses(root: &Path) -> Vec<HookStatus> {
    HOOKS
        .iter()
        .map(|h| HookStatus {
            filename: h.filename,
            trigger: h.trigger,
            command: h.command,
            purpose: h.purpose,
            installed: root.join(".asd/hooks").join(h.filename).exists(),
        })
        .collect()
}

pub fn hooks_path_is_set(root: &Path) -> bool {
    std::process::Command::new("git")
        .args(["config", "--get", "core.hooksPath"])
        .current_dir(root)
        .output()
        .map(|o| {
            let val = String::from_utf8_lossy(&o.stdout);
            val.trim() == ".asd/hooks"
        })
        .unwrap_or(false)
}

pub fn find_project_root(db_path: &PathBuf) -> PathBuf {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

#[cfg(test)]
mod gitignore_tests {
    //! Regression probe for Plan A t-003: the `.asd/v1/` sidecar must be
    //! gitignored, not tracked. If this test fails, init has reverted to the
    //! pre-Plan-A "ride sidecar in git" model.

    use super::update_gitignore;
    use tempfile::tempdir;

    #[test]
    fn fresh_repo_gets_sidecar_and_db_ignored() {
        let tmp = tempdir().unwrap();
        update_gitignore(tmp.path()).unwrap();
        let gi = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l.trim() == ".asd-state.db"));
        assert!(gi.lines().any(|l| l.trim() == ".asd/v1/"));
    }

    #[test]
    fn pre_existing_gitignore_is_preserved() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "node_modules/\n*.log\n").unwrap();
        update_gitignore(tmp.path()).unwrap();
        let gi = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(gi.contains("node_modules/"));
        assert!(gi.contains("*.log"));
        assert!(gi.lines().any(|l| l.trim() == ".asd/v1/"));
    }

    #[test]
    fn idempotent_on_already_correct_gitignore() {
        let tmp = tempdir().unwrap();
        let initial = ".asd-state.db\n.asd/v1/\n";
        std::fs::write(tmp.path().join(".gitignore"), initial).unwrap();
        update_gitignore(tmp.path()).unwrap();
        let gi = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(gi, initial);
    }
}
