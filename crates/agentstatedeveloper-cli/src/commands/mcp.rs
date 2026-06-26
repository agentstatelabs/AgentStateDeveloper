//! `asd mcp` — install, uninstall, and inspect the asd-mcp server
//! registration in known agent tools.
//!
//! All tools currently registered share the de-facto-standard config shape: a
//! JSON file with a top-level `mcpServers` object keyed by server name, each
//! entry `{ command, args, env }`. New tools that follow that convention are a
//! one-line addition to [`TOOLS`]. Tools with a divergent shape (TOML configs,
//! a `context_servers`/`servers` key, or a different entry layout) need the
//! `Tool` model generalized first — tracked as a follow-up slice.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::config::Config;

/// The server name asd registers itself under inside `mcpServers`.
const SERVER_NAME: &str = "asd";

// ---------------------------------------------------------------------------
// Known tools
// ---------------------------------------------------------------------------

struct Tool {
    name: &'static str,
    config_path_fn: fn() -> Option<PathBuf>,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "claude-code",
        config_path_fn: claude_code_config_path,
    },
    Tool {
        name: "claude-desktop",
        config_path_fn: claude_desktop_config_path,
    },
    Tool {
        name: "cursor",
        config_path_fn: cursor_config_path,
    },
    Tool {
        name: "gemini-cli",
        config_path_fn: gemini_cli_config_path,
    },
    Tool {
        name: "windsurf",
        config_path_fn: windsurf_config_path,
    },
];

/// Comma-separated list of registered tool names, for help text and errors.
/// Derived from [`TOOLS`] so it can never drift out of sync.
fn valid_tool_names() -> String {
    TOOLS
        .iter()
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn claude_code_config_path() -> Option<PathBuf> {
    Some(home()?.join(".claude.json"))
}

fn claude_desktop_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return Some(home()?.join("Library/Application Support/Claude/claude_desktop_config.json"));
    #[cfg(target_os = "linux")]
    return Some(home()?.join(".config/Claude/claude_desktop_config.json"));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return None;
}

fn cursor_config_path() -> Option<PathBuf> {
    Some(home()?.join(".cursor/mcp.json"))
}

fn gemini_cli_config_path() -> Option<PathBuf> {
    Some(home()?.join(".gemini/settings.json"))
}

fn windsurf_config_path() -> Option<PathBuf> {
    Some(home()?.join(".codeium/windsurf/mcp_config.json"))
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// Register asd-mcp in all detected agent tool configs.
    Install(InstallArgs),

    /// Remove asd-mcp from all detected agent tool configs.
    Uninstall(UninstallArgs),

    /// Show asd-mcp registration status across known agent tools.
    Status(StatusArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Path to the ASD database to wire into the MCP server.
    /// Defaults to <current-dir>/.asd-state.db (resolved to an absolute path).
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Only install for a specific tool (see `asd mcp status` for the full
    /// list of known tools). Installs into all detected tools when omitted.
    #[arg(long)]
    pub tool: Option<String>,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Only uninstall from a specific tool.
    #[arg(long)]
    pub tool: Option<String>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(_cfg: &Config, cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Install(args) => install(args),
        McpCmd::Uninstall(args) => uninstall(args),
        McpCmd::Status(_) => status(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Locate the asd-mcp binary: prefer the one next to the running asd binary,
/// then ~/.cargo/bin/asd-mcp, then whatever is on PATH.
fn find_asd_mcp() -> Option<PathBuf> {
    // Same directory as the current executable (e.g. ~/.cargo/bin/).
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("asd-mcp");
        if sibling.exists() {
            return Some(sibling);
        }
    }
    // Explicit ~/.cargo/bin fallback.
    if let Some(home) = home() {
        let p = home.join(".cargo/bin/asd-mcp");
        if p.exists() {
            return Some(p);
        }
    }
    // PATH search.
    which_asd_mcp()
}

fn which_asd_mcp() -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .find_map(|dir| {
            let p = PathBuf::from(dir).join("asd-mcp");
            p.exists().then_some(p)
        })
}

fn resolve_db(override_db: Option<PathBuf>) -> Result<PathBuf> {
    let p = match override_db {
        Some(p) => p,
        None => std::env::current_dir()
            .context("cannot determine current directory")?
            .join(".asd-state.db"),
    };
    // Canonicalize if it exists; otherwise just make it absolute.
    if p.exists() {
        p.canonicalize().context("canonicalize db path")
    } else {
        if p.is_absolute() {
            Ok(p)
        } else {
            Ok(std::env::current_dir()?.join(p))
        }
    }
}

/// Read a JSON config file, returning an empty object if it doesn't exist.
fn read_config(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse JSON in {}", path.display()))
}

/// Write a JSON config file, creating parent directories as needed.
fn write_config(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dirs for {}", path.display()))?;
    }
    let out = serde_json::to_string_pretty(value)?;
    std::fs::write(path, out).with_context(|| format!("write {}", path.display()))
}

fn mcp_entry(asd_mcp_bin: &Path, db_path: &Path) -> serde_json::Value {
    serde_json::json!({
        "command": asd_mcp_bin.to_string_lossy(),
        "args": [],
        "env": {
            "ASD_DB": db_path.to_string_lossy()
        }
    })
}

fn tools_to_process(tool_filter: Option<&str>) -> Vec<&'static Tool> {
    TOOLS
        .iter()
        .filter(|t| tool_filter.map_or(true, |f| t.name == f))
        .collect()
}

/// Insert (or overwrite) the asd server entry under `mcpServers` in `cfg`.
/// Returns `true` if an asd entry was already present (i.e. this was an update).
fn upsert_server(cfg: &mut serde_json::Value, entry: &serde_json::Value) -> Result<bool> {
    let servers = cfg
        .as_object_mut()
        .context("config root is not an object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .context("mcpServers is not an object")?;
    let already = servers.contains_key(SERVER_NAME);
    servers.insert(SERVER_NAME.to_string(), entry.clone());
    Ok(already)
}

/// Remove the asd server entry from `mcpServers` in `cfg`.
/// Returns `true` if an entry was present and removed.
fn remove_server(cfg: &mut serde_json::Value) -> bool {
    cfg.get_mut("mcpServers")
        .and_then(|s| s.as_object_mut())
        .map(|m| m.remove(SERVER_NAME).is_some())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

fn install(args: InstallArgs) -> Result<()> {
    let asd_mcp = find_asd_mcp().ok_or_else(|| {
        anyhow::anyhow!(
            "asd-mcp binary not found. Install it with:\n  \
             cargo install --path crates/agentstatedeveloper-mcp"
        )
    })?;

    let db_path = resolve_db(args.db)?;
    let tools = tools_to_process(args.tool.as_deref());

    if tools.is_empty() {
        anyhow::bail!("unknown tool {:?}; valid: {}", args.tool, valid_tool_names());
    }

    let entry = mcp_entry(&asd_mcp, &db_path);
    let mut installed = 0usize;

    for tool in &tools {
        let Some(cfg_path) = (tool.config_path_fn)() else {
            continue;
        };

        let mut cfg = read_config(&cfg_path)?;
        let already = upsert_server(&mut cfg, &entry)?;
        write_config(&cfg_path, &cfg)?;

        if already {
            eprintln!("  {} updated  {}", tool.name, cfg_path.display());
        } else {
            eprintln!("  {} installed  {}", tool.name, cfg_path.display());
        }
        installed += 1;
    }

    if installed == 0 {
        eprintln!("No matching tool config files found.");
        eprintln!("Checked:");
        for tool in &tools {
            if let Some(p) = (tool.config_path_fn)() {
                eprintln!("  {} → {}", tool.name, p.display());
            }
        }
    } else {
        eprintln!();
        eprintln!("  asd-mcp binary:  {}", asd_mcp.display());
        eprintln!("  ASD_DB:          {}", db_path.display());
        eprintln!();
        eprintln!("Restart your agent tool(s) to activate the MCP server.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

fn uninstall(args: UninstallArgs) -> Result<()> {
    let tools = tools_to_process(args.tool.as_deref());

    if tools.is_empty() {
        anyhow::bail!("unknown tool {:?}; valid: {}", args.tool, valid_tool_names());
    }

    let mut removed = 0usize;

    for tool in &tools {
        let Some(cfg_path) = (tool.config_path_fn)() else {
            continue;
        };
        if !cfg_path.exists() {
            continue;
        }

        let mut cfg = read_config(&cfg_path)?;
        let was_present = remove_server(&mut cfg);

        if was_present {
            write_config(&cfg_path, &cfg)?;
            eprintln!("  {} removed  {}", tool.name, cfg_path.display());
            removed += 1;
        } else {
            eprintln!("  {} not registered  (skipped)", tool.name);
        }
    }

    if removed > 0 {
        eprintln!();
        eprintln!("Restart your agent tool(s) to deactivate the MCP server.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status() -> Result<()> {
    let bin = find_asd_mcp();

    eprintln!(
        "asd-mcp binary: {}",
        match &bin {
            Some(p) => p.to_string_lossy().into_owned(),
            None => "NOT FOUND — install with: cargo install --path crates/agentstatedeveloper-mcp"
                .to_string(),
        }
    );
    eprintln!();

    for tool in TOOLS {
        let Some(cfg_path) = (tool.config_path_fn)() else {
            eprintln!("  ✗ {}  (unsupported platform)", tool.name);
            continue;
        };

        if !cfg_path.exists() {
            eprintln!(
                "  ✗ {}  config not found: {}",
                tool.name,
                cfg_path.display()
            );
            continue;
        }

        let cfg = read_config(&cfg_path).unwrap_or_default();
        let entry = cfg.get("mcpServers").and_then(|s| s.get(SERVER_NAME));

        match entry {
            Some(e) => {
                let cmd = e.get("command").and_then(|v| v.as_str()).unwrap_or("?");
                let db = e
                    .get("env")
                    .and_then(|v| v.get("ASD_DB"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                eprintln!("  ✓ {}  command={}  ASD_DB={}", tool.name, cmd, db);
            }
            None => {
                eprintln!(
                    "  ✗ {}  not registered  ({})",
                    tool.name,
                    cfg_path.display()
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate tool name in TOOLS");
    }

    #[test]
    fn registry_includes_new_wave() {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        for expected in ["claude-code", "claude-desktop", "cursor", "gemini-cli", "windsurf"] {
            assert!(names.contains(&expected), "TOOLS missing {expected}");
        }
        // valid_tool_names() is derived from TOOLS, so it must list them too.
        let listed = valid_tool_names();
        assert!(listed.contains("gemini-cli") && listed.contains("windsurf"));
    }

    #[test]
    fn mcp_entry_has_expected_shape() {
        let entry = mcp_entry(Path::new("/usr/local/bin/asd-mcp"), Path::new("/repo/.asd-state.db"));
        assert_eq!(entry["command"], "/usr/local/bin/asd-mcp");
        assert!(entry["args"].as_array().unwrap().is_empty());
        assert_eq!(entry["env"]["ASD_DB"], "/repo/.asd-state.db");
    }

    #[test]
    fn upsert_into_empty_config_creates_entry() {
        let mut cfg = serde_json::json!({});
        let entry = mcp_entry(Path::new("/bin/asd-mcp"), Path::new("/db"));
        let already = upsert_server(&mut cfg, &entry).unwrap();
        assert!(!already, "fresh insert should not report already-present");
        assert_eq!(cfg["mcpServers"]["asd"]["command"], "/bin/asd-mcp");
    }

    #[test]
    fn upsert_is_idempotent_and_reports_update() {
        let mut cfg = serde_json::json!({});
        let entry = mcp_entry(Path::new("/bin/asd-mcp"), Path::new("/db"));
        assert!(!upsert_server(&mut cfg, &entry).unwrap());
        // Second insert overwrites and reports the prior presence.
        assert!(upsert_server(&mut cfg, &entry).unwrap());
        assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn upsert_preserves_sibling_servers() {
        let mut cfg = serde_json::json!({
            "mcpServers": { "other": { "command": "x" } },
            "someOtherKey": 1
        });
        let entry = mcp_entry(Path::new("/bin/asd-mcp"), Path::new("/db"));
        upsert_server(&mut cfg, &entry).unwrap();
        assert_eq!(cfg["mcpServers"]["other"]["command"], "x");
        assert_eq!(cfg["someOtherKey"], 1);
        assert!(cfg["mcpServers"]["asd"].is_object());
    }

    #[test]
    fn remove_returns_false_when_absent_true_when_present() {
        let mut cfg = serde_json::json!({});
        assert!(!remove_server(&mut cfg), "nothing to remove");
        let entry = mcp_entry(Path::new("/bin/asd-mcp"), Path::new("/db"));
        upsert_server(&mut cfg, &entry).unwrap();
        assert!(remove_server(&mut cfg), "should remove the asd entry");
        assert!(!remove_server(&mut cfg), "second remove is a no-op");
    }
}
