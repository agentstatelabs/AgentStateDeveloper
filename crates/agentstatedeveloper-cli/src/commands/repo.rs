//! `asd repo {add,list,use,rm,show}` — manage the shared ASD repo registry
//! at `~/.config/asd/repos.toml`. See `docs/repo-registry.md`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;

use agentstatedeveloper_core::registry::Registry;
use agentstatedeveloper_core::{Engine, ServiceManifest, endpoints_from_tree, federated_edges};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum RepoCmd {
    /// Register a repo. `name` defaults to the directory stem of `path`;
    /// `path` defaults to `<cwd>/.asd-state.db`.
    Add(AddArgs),
    /// List every registered repo with an active marker.
    List(ListArgs),
    /// Set the active repo.
    Use(UseArgs),
    /// Remove a repo.
    Rm(RmArgs),
    /// Print the active repo's name and path.
    Show(ShowArgs),
    /// Cross-repo service edges across all registered repos: a client call in
    /// one repo matched to a route served by another, keyed by contract hash
    /// (Plan Q t-005). Index each repo first so its contracts are current.
    Edges(EdgesArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Repo name. Defaults to the directory stem of `path`.
    pub name: Option<String>,
    /// Absolute path to the `.asd-state.db` file.
    /// Defaults to `<cwd>/.asd-state.db`.
    pub path: Option<PathBuf>,
    /// Set this repo as active after registering.
    #[arg(long)]
    pub activate: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit JSON instead of the default human-readable table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UseArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Emit JSON instead of the default plain-text line.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct EdgesArgs {
    /// Machine-readable JSON.
    #[arg(long)]
    pub agent: bool,
    /// Also include in-repo edges (default: cross-repo edges only).
    #[arg(long)]
    pub include_in_repo: bool,
}

pub fn run(_cfg: &Config, cmd: RepoCmd) -> Result<()> {
    match cmd {
        RepoCmd::Add(args) => run_add(args),
        RepoCmd::List(args) => run_list(args),
        RepoCmd::Use(args) => run_use(args),
        RepoCmd::Rm(args) => run_rm(args),
        RepoCmd::Show(args) => run_show(args),
        RepoCmd::Edges(args) => run_edges(args),
    }
}

fn run_add(args: AddArgs) -> Result<()> {
    let path = match args.path {
        Some(p) => absolutize(&p)?,
        None => std::env::current_dir()
            .context("could not read current directory")?
            .join(".asd-state.db"),
    };
    let name = match args.name {
        Some(n) => n,
        None => default_name_for(&path)
            .context("could not derive a default name from the path; pass one explicitly")?,
    };

    let mut reg = Registry::load().context("loading registry")?;
    reg.register(&name, &path)
        .with_context(|| format!("registering {name}"))?;
    if args.activate {
        reg.set_active(&name)?;
    }
    reg.save().context("saving registry")?;

    println!("Registered {name} -> {}", path.display());
    if args.activate {
        println!("Active: {name}");
    } else if reg.active().is_none() {
        println!("Tip: run `asd repo use {name}` to make it the active repo.");
    }
    Ok(())
}

fn run_list(args: ListArgs) -> Result<()> {
    let reg = Registry::load().context("loading registry")?;
    let entries = reg.list();
    let active = reg.active().map(|e| e.name.clone());

    if args.json {
        let payload = serde_json::json!({
            "active": active,
            "repos": entries
                .iter()
                .map(|e| serde_json::json!({
                    "name": e.name,
                    "path": e.path.display().to_string(),
                    "active": Some(&e.name) == active.as_ref(),
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No repos registered.");
        println!();
        println!("Register one with:");
        println!("    asd repo add              # uses cwd + dir name");
        println!("    asd repo add myapp /path/to/.asd-state.db");
        return Ok(());
    }

    let name_width = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    for e in entries {
        let marker = if Some(&e.name) == active.as_ref() {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {:width$}  {}",
            e.name,
            e.path.display(),
            width = name_width
        );
    }
    if active.is_none() {
        println!();
        println!("No active repo. Run `asd repo use <name>`.");
    }
    Ok(())
}

fn run_use(args: UseArgs) -> Result<()> {
    let mut reg = Registry::load().context("loading registry")?;
    reg.set_active(&args.name)?;
    reg.save().context("saving registry")?;
    let path = reg
        .active()
        .map(|e| e.path.display().to_string())
        .unwrap_or_default();
    println!("Active: {} -> {}", args.name, path);
    Ok(())
}

fn run_rm(args: RmArgs) -> Result<()> {
    let mut reg = Registry::load().context("loading registry")?;
    reg.remove(&args.name)?;
    reg.save().context("saving registry")?;
    println!("Removed {}", args.name);
    Ok(())
}

fn run_show(args: ShowArgs) -> Result<()> {
    let reg = Registry::load().context("loading registry")?;
    match reg.active() {
        Some(e) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({ "name": e.name, "path": e.path.display().to_string() })
                );
            } else {
                println!("{} -> {}", e.name, e.path.display());
            }
        }
        None => {
            if args.json {
                println!("{}", serde_json::json!({ "name": null, "path": null }));
            } else {
                println!("No active repo.");
            }
        }
    }
    Ok(())
}

fn run_edges(args: EdgesArgs) -> Result<()> {
    let reg = Registry::load().context("loading registry")?;
    let entries = reg.list();
    if entries.is_empty() {
        println!("No repos registered. Add a couple with `asd repo add`, then re-index each.");
        return Ok(());
    }

    // Load each registered repo's detected endpoints from its index.
    let mut manifests = Vec::new();
    let mut loaded: Vec<(String, usize)> = Vec::new();
    for e in &entries {
        match Engine::open_sqlite(&e.path) {
            Ok(engine) => {
                let tree = engine
                    .repo
                    .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
                    .unwrap_or(Value::Null);
                let endpoints = endpoints_from_tree(&tree);
                let repo_id = endpoints
                    .first()
                    .map(|ep| ep.repo_id.clone())
                    .unwrap_or_else(|| e.name.clone());
                loaded.push((e.name.clone(), endpoints.len()));
                manifests.push(ServiceManifest { repo_id, endpoints });
            }
            Err(err) => eprintln!("skip {}: {err}", e.name),
        }
    }

    let edges = federated_edges(&manifests, !args.include_in_repo);

    if args.agent {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "repos": loaded.iter().map(|(n, c)| json!({ "name": n, "endpoints": c })).collect::<Vec<_>>(),
                "edges": edges,
            }))?
        );
        return Ok(());
    }

    println!(
        "{} repos loaded ({} endpoints total).",
        loaded.len(),
        loaded.iter().map(|(_, c)| c).sum::<usize>()
    );
    let label = if args.include_in_repo {
        "edge(s)"
    } else {
        "cross-repo edge(s)"
    };
    if edges.is_empty() {
        println!("No {label}. (Index each repo first so its contracts are current.)");
        return Ok(());
    }
    println!("{} {label}:", edges.len());
    for ed in &edges {
        let scope = if ed.cross_repo { "  [cross-repo]" } else { "" };
        println!(
            "  {} → {}   {}{}",
            ed.from.repo_id, ed.to.repo_id, ed.contract, scope
        );
        println!(
            "      {}  →  {}",
            short_qname(&ed.from.qname),
            short_qname(&ed.to.qname)
        );
    }
    Ok(())
}

fn short_qname(q: &str) -> &str {
    q.rsplit(['.', ':']).next().unwrap_or(q)
}

fn default_name_for(path: &std::path::Path) -> Option<String> {
    // The db lives at `<repo-root>/.asd-state.db`, so the parent dir's file
    // name is the project name. Fall back to the file stem for unusual paths.
    let candidate = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })?;
    let sanitized: String = candidate
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn absolutize(p: &std::path::Path) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().context("could not read current directory")?;
    Ok(cwd.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn default_name_uses_parent_dir() {
        let p = PathBuf::from("/Users/user/code/myapp/.asd-state.db");
        assert_eq!(default_name_for(&p).as_deref(), Some("myapp"));
    }

    #[test]
    fn default_name_sanitizes_unsafe_chars() {
        let p = PathBuf::from("/tmp/foo bar.baz/.asd-state.db");
        assert_eq!(default_name_for(&p).as_deref(), Some("foo-bar-baz"));
    }
}
