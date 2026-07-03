//! Agent Skill installation (suite-onboarding t-002).
//!
//! Renders ASD's onboarding [`SkillSpec`] via the shared `agent-skillgen`
//! engine and places a `SKILL.md` into each skill-capable host's skills
//! directory. Complements `asd mcp install` (MCP config) and `asd mcp
//! instructions` (always-on block) — this adds the actual Agent Skill file.
//!
//! The always-on block today still comes from `mcp.rs::instruction_body`;
//! unifying it onto this same spec is suite-onboarding t-007.

use std::path::{Path, PathBuf};

use agent_skillgen::{PLATFORMS, SkillSpec, render_skill};
use anyhow::{Context, Result};
use clap::Args;

/// ASD's onboarding content — the single source the shared engine renders into
/// per-agent skill files (and, later, the always-on block).
pub fn asd_skill_spec() -> SkillSpec {
    SkillSpec::new(
        "AgentStateDeveloper",
        "asd",
        "Code-level context, impact analysis, and change scoping for coding agents.",
        env!("CARGO_PKG_VERSION"),
    )
    .rule("Before a non-trivial edit, run `asd prepare-change <area>` for the scoped files, blast radius, and invariants.")
    .rule("Before assuming a symbol's behavior, get `asd context-for <symbol>` instead of guessing from grep.")
    .rule("To see what a change breaks, run `asd impact <symbol>`.")
    .rule("Prefer `asd search` over raw grep for symbol-level lookups.")
    .rule("After editing code, run `asd reindex` so the index stays current.")
    .command("asd prepare-change", "scope a change: files, impact, invariants")
    .command("asd context-for", "focused context for one symbol")
    .command("asd impact", "downstream blast radius of a change")
    .command("asd search", "structural symbol search")
    .sibling(
        "CTXone",
        "the team layer — shares decisions, plans, and memory across the team; install the `ctx` CLI to enable it.",
    )
}

/// Where to install: user-wide (home) or repo-local (project).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Home,
    Project,
}

#[derive(Debug, Args)]
pub struct SkillArgs {
    /// Install project-scoped (into the repo) instead of the default
    /// home-scoped (user-wide). Not every host has a project location.
    #[arg(long)]
    pub project: bool,

    /// Only install for one host key (e.g. `claude-code`). All skill-capable
    /// hosts when omitted.
    #[arg(long)]
    pub tool: Option<String>,

    /// Remove installed skill files instead of writing them.
    #[arg(long)]
    pub remove: bool,

    /// Print what would happen without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
}

/// What happened for one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Wrote,
    Removed,
    WouldWrite,
    WouldRemove,
    Skipped,
}

/// Placement outcome for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub tool: &'static str,
    pub path: PathBuf,
    pub action: Action,
}

pub fn run(args: SkillArgs) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("cannot resolve $HOME")?;
    let root = std::env::current_dir().context("cannot resolve current directory")?;
    let scope = if args.project {
        SkillScope::Project
    } else {
        SkillScope::Home
    };

    let placed = place_skills(
        &asd_skill_spec(),
        &home,
        &root,
        scope,
        args.tool.as_deref(),
        args.remove,
        args.dry_run,
    )?;

    if placed.is_empty() {
        println!("No skill-capable hosts matched.");
        return Ok(());
    }
    for p in &placed {
        let verb = match p.action {
            Action::Wrote => "installed",
            Action::Removed => "removed",
            Action::WouldWrite => "would install",
            Action::WouldRemove => "would remove",
            Action::Skipped => "skipped",
        };
        println!("  {verb:>14}  {:<12}  {}", p.tool, p.path.display());
    }
    Ok(())
}

/// Render + place ASD's `SKILL.md` into each skill-capable host. Resolves paths
/// against the passed `home`/`root` (not the process env) so callers and tests
/// can target any directory.
pub fn place_skills(
    spec: &SkillSpec,
    home: &Path,
    root: &Path,
    scope: SkillScope,
    tool_filter: Option<&str>,
    remove: bool,
    dry_run: bool,
) -> Result<Vec<Placed>> {
    let mut out = Vec::new();
    for p in PLATFORMS {
        if let Some(f) = tool_filter {
            if p.key != f {
                continue;
            }
        }
        let Some(skill) = p.skill else { continue };
        let dir = match scope {
            SkillScope::Home => skill.home_dir_under(&spec.slug, home),
            SkillScope::Project => match skill.project_dir(&spec.slug, root) {
                Some(d) => d,
                None => continue, // host has no project-scoped skill location
            },
        };
        let path = dir.join("SKILL.md");

        if remove {
            let action = if dry_run {
                Action::WouldRemove
            } else if path.exists() {
                std::fs::remove_file(&path).with_context(|| format!("remove {path:?}"))?;
                let _ = std::fs::remove_dir(&dir); // best-effort if now empty
                Action::Removed
            } else {
                Action::Skipped
            };
            out.push(Placed {
                tool: p.key,
                path,
                action,
            });
            continue;
        }

        let Some(content) = render_skill(spec, p) else {
            continue;
        };
        let action = if dry_run {
            Action::WouldWrite
        } else {
            std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {dir:?}"))?;
            std::fs::write(&path, &content).with_context(|| format!("write {path:?}"))?;
            Action::Wrote
        };
        out.push(Placed {
            tool: p.key,
            path,
            action,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_skillgen::{platform, render_skill};

    #[test]
    fn asd_spec_renders_real_content() {
        let spec = asd_skill_spec();
        let md = render_skill(&spec, platform("claude-code").unwrap()).unwrap();
        assert!(md.contains("AgentStateDeveloper"));
        assert!(md.contains("asd prepare-change"));
        assert!(md.contains("CTXone"), "sibling cross-promo must be present");
        assert!(md.starts_with("---\nname: asd\n"));
    }

    #[test]
    fn places_home_scoped_skill_files() {
        let tmp = tempfile::tempdir().unwrap();
        let placed = place_skills(
            &asd_skill_spec(),
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            None,
            false,
            false,
        )
        .unwrap();
        assert!(!placed.is_empty());
        let claude = tmp.path().join(".claude/skills/asd/SKILL.md");
        assert!(claude.exists(), "expected {claude:?}");
        assert!(
            std::fs::read_to_string(&claude)
                .unwrap()
                .contains("Loaded by Claude Code")
        );
        for pl in &placed {
            assert_eq!(pl.action, Action::Wrote);
            assert!(pl.path.exists(), "{:?} not written", pl.path);
        }
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let placed = place_skills(
            &asd_skill_spec(),
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!placed.is_empty());
        for pl in &placed {
            assert_eq!(pl.action, Action::WouldWrite);
            assert!(!pl.path.exists());
        }
    }

    #[test]
    fn remove_deletes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = asd_skill_spec();
        place_skills(
            &spec,
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            Some("claude-code"),
            false,
            false,
        )
        .unwrap();
        let path = tmp.path().join(".claude/skills/asd/SKILL.md");
        assert!(path.exists());
        let placed = place_skills(
            &spec,
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            Some("claude-code"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(placed[0].action, Action::Removed);
        assert!(!path.exists());
    }
}
