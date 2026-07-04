//! `asd watch` — keep the index fresh automatically (suite-onboarding t-009).
//!
//! Watches the repo for source changes and re-indexes, debounced. Pairs with
//! the always-on nudge (t-007) and the stale warnings `prepare-change`/`status`
//! already emit — this is the hands-off option so the index never drifts.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use notify::{RecursiveMode, Watcher};

use crate::commands::index::{self, IndexArgs};
use crate::config::Config;

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Directory to watch and re-index. Defaults to the current directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Debounce window (ms) — coalesce a burst of edits into one reindex.
    #[arg(long, default_value_t = 1500)]
    pub debounce_ms: u64,
}

/// Should a changed path trigger a reindex? Skips VCS/build/index-internal
/// paths and non-source files, so editor churn and the index's own writes don't
/// cause a reindex loop.
pub fn is_relevant_change(path: &Path) -> bool {
    // Match on path *components* (not substrings) so a leading `target/` is
    // caught the same as a nested `/target/`.
    const SKIP: &[&str] = &[".git", "target", "node_modules", ".asd", ".asd-state.db"];
    for comp in path.components() {
        if let std::path::Component::Normal(os) = comp {
            if os.to_str().is_some_and(|n| SKIP.contains(&n)) {
                return false;
            }
        }
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "py" | "ts"
                | "tsx"
                | "mts"
                | "cts"
                | "rs"
                | "go"
                | "java"
                | "cs"
                | "rb"
                | "kt"
                | "kts"
                | "swift"
        )
    )
}

pub fn run(cfg: &Config, args: WatchArgs) -> Result<()> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("create filesystem watcher")?;
    watcher
        .watch(&args.path, RecursiveMode::Recursive)
        .with_context(|| format!("watch {}", args.path.display()))?;

    println!(
        "asd watch: watching {} (Ctrl-C to stop)",
        args.path.display()
    );
    reindex(cfg, &args.path); // initial pass so we start fresh

    let debounce = Duration::from_millis(args.debounce_ms);
    let mut pending: Option<Instant> = None;
    loop {
        let timeout = pending
            .map(|t| debounce.saturating_sub(t.elapsed()))
            .unwrap_or_else(|| Duration::from_secs(3600));
        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if event.paths.iter().any(|p| is_relevant_change(p)) {
                    pending = Some(Instant::now());
                }
            }
            Ok(Err(_)) => {} // watcher hiccup — ignore, keep watching
            Err(RecvTimeoutError::Timeout) => {
                if pending.take_if(|t| t.elapsed() >= debounce).is_some() {
                    reindex(cfg, &args.path);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn reindex(cfg: &Config, path: &Path) {
    print!("asd watch: reindexing… ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    match index::run(
        cfg,
        IndexArgs {
            path: path.to_path_buf(),
            verbose: false,
        },
    ) {
        Ok(()) => println!("done"),
        Err(e) => println!("failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_files_are_relevant() {
        assert!(is_relevant_change(Path::new("src/foo.rs")));
        assert!(is_relevant_change(Path::new("app/models/user.py")));
        assert!(is_relevant_change(Path::new("a/b/Handler.kt")));
    }

    #[test]
    fn noise_is_ignored() {
        assert!(!is_relevant_change(Path::new("target/debug/foo.rs")));
        assert!(!is_relevant_change(Path::new(".git/index")));
        assert!(!is_relevant_change(Path::new("node_modules/x/y.ts")));
        assert!(!is_relevant_change(Path::new(".asd/index.log")));
        assert!(!is_relevant_change(Path::new("./.asd-state.db")));
        assert!(!is_relevant_change(Path::new("README.md")));
        assert!(!is_relevant_change(Path::new("Cargo.toml")));
    }
}
