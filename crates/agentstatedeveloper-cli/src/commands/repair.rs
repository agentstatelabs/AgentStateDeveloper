//! `asd repair [--fix] [--json]` — scan for ASG integrity issues and optionally
//! apply safe auto-corrections.
//!
//! Without `--fix` the command is read-only (dry-run).  Use `--fix` to
//! automatically drop orphaned effect records and stale callee/caller refs.
//!
//! ## Example output (human-readable, dry-run)
//!
//! ```text
//! Scanning ASG for integrity issues…
//!
//! WARN  orphaned_effect  /asd/v1/effects/abc123
//!       symbol 'abc123' not found in index; effect record is orphaned  [auto-fixable]
//!
//! WARN  orphaned_callee_ref  /asd/v1/index/callees/def456
//!       callee 'xyz789' referenced by 'def456' no longer exists  [auto-fixable]
//!
//! 2 issues found (2 auto-fixable).  Run `asd repair --fix` to apply corrections.
//! ```

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{Engine, IssueSeverity, repair_asg, scan_asg, scan_sidecar};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct RepairArgs {
    /// Apply auto-fixable corrections (drop orphaned effects and stale
    /// callee/caller refs).  Without this flag the command is read-only.
    #[arg(long)]
    pub fix: bool,

    /// Emit a JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: RepairArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;

    // Plan K t-010: always run the sidecar inclusion-rule lint
    // alongside the ASG integrity scan. The sidecar issues are
    // read-only and never auto-fixed (intentional — flagged files
    // might be legitimate work in progress, deserve human review).
    let conclusions_dir = cfg
        .db_path
        .parent()
        .map(|p| p.join(".asd").join("conclusions"))
        .unwrap_or_else(|| std::path::PathBuf::from(".asd/conclusions"));
    let sidecar_issues = scan_sidecar(&conclusions_dir);

    if args.fix {
        let mut report = repair_asg(&engine.repo, &engine.ref_name, &cfg.agent_id, false)?;
        report.issues.extend(sidecar_issues.clone());
        report.issues_found += sidecar_issues.len();
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_report_human(&report, false);
        }
    } else {
        // Dry-run: scan only.
        let mut issues = scan_asg(&engine.repo, &engine.ref_name)?;
        issues.extend(sidecar_issues);
        let report = agentstatedeveloper_core::RepairReport {
            issues_found: issues.len(),
            fixes_applied: 0,
            dry_run: true,
            issues,
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_report_human(&report, true);
        }
    }

    Ok(())
}

fn print_report_human(report: &agentstatedeveloper_core::RepairReport, dry_run: bool) {
    if dry_run {
        eprintln!("Scanning ASG for integrity issues…\n");
    }

    if report.issues.is_empty() {
        println!("✓ No integrity issues found.");
        return;
    }

    for issue in &report.issues {
        let sev = match issue.severity {
            IssueSeverity::Warn => "WARN ",
            IssueSeverity::Error => "ERROR",
        };
        let fixable = if issue.auto_fixable {
            "  [auto-fixable]"
        } else {
            ""
        };
        println!("{sev}  {}  {}", issue.kind, issue.path);
        println!("      {}{}\n", issue.detail, fixable);
    }

    let auto_fixable = report.issues.iter().filter(|i| i.auto_fixable).count();

    if dry_run {
        if auto_fixable > 0 {
            println!(
                "{} issue(s) found ({} auto-fixable).  Run `asd repair --fix` to apply corrections.",
                report.issues_found, auto_fixable
            );
        } else {
            println!(
                "{} issue(s) found (none auto-fixable).  Manual intervention required.",
                report.issues_found
            );
        }
    } else {
        println!(
            "{} issue(s) found before repair.  {} correction(s) applied.  {} issue(s) remain.",
            report.issues_found,
            report.fixes_applied,
            report.issues.len()
        );
    }
}
