//! Agent Skill installation (suite-onboarding t-002).
//!
//! Renders ASD's onboarding [`SkillSpec`] via the shared `agent-skillgen`
//! engine and places a `SKILL.md` into each skill-capable host's skills
//! directory. Complements `asd mcp install` (MCP config) and `asd mcp
//! instructions` (always-on block) — this adds the actual Agent Skill file.
//!
//! `mcp.rs::instruction_body` renders the always-on block from this same
//! `asd_skill_spec()` via the engine (t-007) — one source for both surfaces.

use std::path::{Path, PathBuf};

use agent_skillgen::{
    Action, SkillScope, SkillSpec, SkillState, already_nudged, binary_on_path, install_suite,
    place_skills, record_nudge, should_nudge, skill_status,
};
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
    .rule("To see what a change breaks, run `asd impact <symbol>` before editing.")
    .rule("To orient in an unfamiliar area, run `asd architecture` for languages, packages, layers, routes, and hotspots.")
    .rule("Prefer `asd search` over raw grep for symbol-level lookups.")
    .rule("After editing code, run `asd reindex` so the index stays current.")
    .command("asd prepare-change", "scope a change: files, impact, invariants")
    .command("asd context-for", "focused context for one symbol")
    .command("asd impact", "downstream blast radius of a change")
    .command("asd architecture", "orient: languages, packages, layers, hotspots")
    .command("asd search", "structural symbol search")
    .sibling(
        "CTXone",
        "ctx",
        "use it to share decisions, plans, and memory across your team (the `ctx` CLI).",
    )
    .bootstrap_step(
        "brew install asd",
        "install ASD (macOS/Linux; or run the install.sh script)",
    )
    .bootstrap_step("asd index .", "index this repository")
    .bootstrap_step(
        "asd mcp install && asd skill",
        "register the MCP server + agent skill",
    )
    .bootstrap_step("asd status", "verify the index is built")
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

    /// Report the install state of each host's skill without changing anything.
    #[arg(long)]
    pub status: bool,

    /// Suppress the one-time suggestion to add the sibling product (CTXone).
    /// Also suppressed by the ASD_NO_SUGGEST env var.
    #[arg(long)]
    pub no_nudge: bool,

    /// Print ASD's onboarding SkillSpec as JSON and exit (for the sibling CLI
    /// to render the combined suite skill). Internal cross-CLI contract.
    #[arg(long)]
    pub emit_spec: bool,

    /// Print what would happen without touching the filesystem.
    #[arg(long)]
    pub dry_run: bool,
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
    let spec = asd_skill_spec();

    if args.emit_spec {
        println!("{}", spec.to_json());
        return Ok(());
    }

    if args.status {
        let states = skill_status(&spec, &home, &root, scope, args.tool.as_deref());
        if states.is_empty() {
            println!("No skill-capable hosts matched.");
        }
        for (tool, state) in &states {
            println!("  {:<12}  {}", tool, describe_state(state));
        }
        return Ok(());
    }

    let placed = place_skills(
        &spec,
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
            Action::SkippedNewer => "skipped (newer on disk)",
        };
        println!("  {verb:>22}  {:<12}  {}", p.tool, p.path.display());
    }

    // When the sibling is installed, also install the canonical combined suite
    // skill (idempotent — either product produces byte-identical content).
    if !args.remove {
        maybe_install_combined(&spec, &home, &root, scope, args.dry_run);
    }

    // Self-verify + one-time nudge — only after a real install.
    if !args.dry_run && !args.remove {
        print_verify_summary(&spec, &home, &root, scope, args.tool.as_deref());
        let suppress = args.no_nudge || std::env::var_os("ASD_NO_SUGGEST").is_some();
        if let Some(msg) = maybe_nudge_sibling(&spec, &asd_state_dir(&home), suppress) {
            println!("{msg}");
        }
    }
    Ok(())
}

/// Re-read each host's skill state after an install and print a green/red
/// verdict, so the user knows the install actually took (t-008). For the full
/// index health check, `asd health` remains the deeper probe.
fn print_verify_summary(
    spec: &SkillSpec,
    home: &Path,
    root: &Path,
    scope: SkillScope,
    filter: Option<&str>,
) {
    let states = skill_status(spec, home, root, scope, filter);
    if states.is_empty() {
        return;
    }
    let total = states.len();
    let current = states
        .iter()
        .filter(|(_, s)| matches!(s, SkillState::Current { .. }))
        .count();
    if current == total {
        println!("\n✓ verified {current}/{total} skills installed and current");
    } else {
        println!("\n⚠ verified {current}/{total} current — issues:");
        for (t, s) in &states {
            if !matches!(s, SkillState::Current { .. }) {
                println!("    {t:<12}  {}", describe_state(s));
            }
        }
    }
}

/// ASD's cross-run state directory (also home to `repos.toml`).
fn asd_state_dir(home: &Path) -> PathBuf {
    home.join(".config").join("asd")
}

/// If the sibling CLI is on PATH, fetch its spec (`<bin> skill --emit-spec`) and
/// install the canonical combined suite skill. Best-effort — any failure is
/// silent (the per-product skills already installed fine).
fn maybe_install_combined(
    spec: &SkillSpec,
    home: &Path,
    root: &Path,
    scope: SkillScope,
    dry_run: bool,
) {
    let Some(sib) = spec.sibling.as_ref() else {
        return;
    };
    if !binary_on_path(&sib.bin) {
        return;
    }
    let Ok(output) = std::process::Command::new(&sib.bin)
        .args(["skill", "--emit-spec"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Some(sib_spec) = SkillSpec::from_json(&String::from_utf8_lossy(&output.stdout)) else {
        return;
    };
    if let Ok((name, placed)) = install_suite(spec, &sib_spec, home, root, scope, dry_run) {
        if !placed.is_empty() {
            let verb = if dry_run {
                "would install"
            } else {
                "installed"
            };
            println!(
                "\n✓ {verb} combined suite skill `{name}` to {} host(s)",
                placed.len()
            );
        }
    }
}

/// The one-time sibling suggestion, or `None` when it shouldn't show (sibling
/// already installed, already shown, or suppressed). Records the nudge when it
/// returns `Some`, so it never repeats. Never blocks — it only gates a printed
/// line.
pub fn maybe_nudge_sibling(spec: &SkillSpec, state_dir: &Path, suppressed: bool) -> Option<String> {
    let sib = spec.sibling.as_ref()?;
    let present = binary_on_path(&sib.bin);
    if !should_nudge(present, already_nudged(state_dir, &sib.bin), suppressed) {
        return None;
    }
    // Best-effort — a failed marker write shouldn't error the install.
    let _ = record_nudge(state_dir, &sib.bin);
    Some(format!(
        "\nTip: pair ASD with {} — {}\n(Shown once; suppress with --no-nudge or ASD_NO_SUGGEST=1.)",
        sib.product, sib.pitch
    ))
}

fn describe_state(state: &SkillState) -> String {
    match state {
        SkillState::NotInstalled => "not installed".to_string(),
        SkillState::Missing => "SKILL.md missing — run `asd skill` to repair".to_string(),
        SkillState::Unstamped => "installed, no version stamp".to_string(),
        SkillState::Current { version } => format!("current ({version})"),
        SkillState::Stale { installed, package } => {
            format!("stale ({installed} < {package}) — run `asd skill` to update")
        }
        SkillState::Newer { installed, package } => {
            format!("newer on disk ({installed} > {package}) — not overwriting")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_skillgen::{
        Action, STAMP_FILE, SkillScope, SkillSpec, SkillState, place_skills, platform,
        render_skill, skill_status, write_stamp,
    };

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

    #[test]
    fn install_writes_version_stamp() {
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
        let stamp = tmp.path().join(".claude/skills/asd").join(STAMP_FILE);
        assert!(stamp.exists(), "version stamp not written");
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap().trim(),
            spec.version
        );
    }

    #[test]
    fn refuses_to_downgrade_a_newer_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = asd_skill_spec();
        let dir = tmp.path().join(".claude/skills/asd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "NEWER CONTENT").unwrap();
        write_stamp(&dir, "999.0.0").unwrap(); // pretend a much newer install exists

        let placed = place_skills(
            &spec,
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            Some("claude-code"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(placed[0].action, Action::SkippedNewer);
        // content must NOT be overwritten
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "NEWER CONTENT"
        );
    }

    #[test]
    fn status_transitions_not_installed_to_current() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = asd_skill_spec();
        let before = skill_status(
            &spec,
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            Some("claude-code"),
        );
        assert_eq!(before[0].1, SkillState::NotInstalled);
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
        let after = skill_status(
            &spec,
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            Some("claude-code"),
        );
        assert!(
            matches!(after[0].1, SkillState::Current { .. }),
            "got {:?}",
            after[0].1
        );
    }

    #[test]
    fn nudge_shows_once_then_records() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = SkillSpec::new("X", "x", "t", "1.0.0").sibling(
            "Sibling",
            "nonexistent-binary-xyz-404",
            "get the sibling",
        );
        // sibling absent → shows once and records the marker
        assert!(maybe_nudge_sibling(&spec, tmp.path(), false).is_some());
        // second call → already nudged → silent
        assert!(maybe_nudge_sibling(&spec, tmp.path(), false).is_none());
    }

    #[test]
    fn nudge_suppressed_records_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = SkillSpec::new("X", "x", "t", "1.0.0").sibling(
            "Sibling",
            "nonexistent-binary-xyz-404",
            "get the sibling",
        );
        // suppressed → no message, and no marker written…
        assert!(maybe_nudge_sibling(&spec, tmp.path(), true).is_none());
        // …so a later un-suppressed call still shows.
        assert!(maybe_nudge_sibling(&spec, tmp.path(), false).is_some());
    }

    #[test]
    fn verify_all_current_after_full_install() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = asd_skill_spec();
        place_skills(
            &spec,
            tmp.path(),
            tmp.path(),
            SkillScope::Home,
            None,
            false,
            false,
        )
        .unwrap();
        let states = skill_status(&spec, tmp.path(), tmp.path(), SkillScope::Home, None);
        assert!(!states.is_empty());
        assert!(
            states
                .iter()
                .all(|(_, s)| matches!(s, SkillState::Current { .. })),
            "post-install verify should show all current: {states:?}"
        );
    }
}
