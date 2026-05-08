//! `asd status` — show index health, age, modified files, and sidecar lifecycle.

use anyhow::Result;
use clap::Args;

use agentstatedeveloper_core::{SearchFtsDb, format_age, sidecar_lifecycle_state, SidecarState};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Show source files modified since the last index run (requires git).
    #[arg(long)]
    pub show_dirty: bool,
}

pub fn run(cfg: &Config, args: StatusArgs) -> Result<()> {
    let fts = SearchFtsDb::open(&cfg.db_path)?;

    println!("ASD index status");
    println!("  db:       {}", cfg.db_path.display());

    if !fts.has_data() {
        println!("  state:    empty — run 'asd index <dir>' to build");
        return Ok(());
    }

    let count = fts.symbol_count();
    println!("  symbols:  {count}");

    match fts.last_indexed_at() {
        Some(ts) => {
            println!("  indexed:  {} (unix {})", format_age(ts), ts);

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let age_h = (now - ts).max(0) / 3600;
            if age_h >= 1 {
                println!("  warning:  index is {}h old — consider re-running 'asd index'", age_h);
            } else {
                println!("  state:    fresh");
            }
        }
        None => println!("  indexed:  unknown"),
    }

    // Sidecar lifecycle state — helps agents distinguish deliberate reset from
    // indexing failure and know whether `asd hydrate` still needs to run.
    let project_root = cfg.db_path.parent().unwrap_or(std::path::Path::new("."));
    let sidecar_state = sidecar_lifecycle_state(project_root);
    let sidecar_label = match sidecar_state {
        SidecarState::Missing   => "missing — run 'asd sync' to create",
        SidecarState::Present   => "present — run 'asd hydrate' to load into ASG",
        SidecarState::Hydrated  => "hydrated",
        SidecarState::FreshReset => "fresh-reset (deliberate reset — re-run 'asd index' + 'asd sync')",
    };
    println!("  sidecar:  {sidecar_label}");

    if args.show_dirty {
        print_dirty_files(cfg)?;
    }

    Ok(())
}

/// Run `git status --short --untracked-files=no` and print modified tracked
/// source files. Silently skips if git is unavailable.
fn print_dirty_files(cfg: &Config) -> Result<()> {
    let workspace = cfg.db_path.parent().unwrap_or(std::path::Path::new("."));
    let output = std::process::Command::new("git")
        .args(["status", "--short", "--untracked-files=no"])
        .current_dir(workspace)
        .output();

    let Ok(out) = output else { return Ok(()); };
    if !out.status.success() { return Ok(()); }

    let text = String::from_utf8_lossy(&out.stdout);
    let source_exts = [".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go", ".kt", ".java", ".rb", ".cs"];
    let dirty: Vec<&str> = text
        .lines()
        .filter(|l| source_exts.iter().any(|ext| l.ends_with(ext)))
        .collect();

    if dirty.is_empty() {
        println!("  dirty:    none (all tracked source files match index)");
    } else {
        println!("  dirty:    {} modified source file(s) since last commit:", dirty.len());
        for line in &dirty {
            println!("            {}", line.trim());
        }
    }
    Ok(())
}
