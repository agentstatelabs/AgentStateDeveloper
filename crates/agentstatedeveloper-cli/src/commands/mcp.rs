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

/// Serialization format of a tool's config file. JSON/JSONC tools share the
/// `serde_json::Value` read/insert/write path (JSONC strips comments on read);
/// TOML tools are edited with `toml_edit` to preserve comments and ordering.
#[derive(Clone, Copy, PartialEq)]
enum ConfigFormat {
    Json,
    /// JSON with comments (e.g. Kilo Code's kilo.jsonc). Comments are stripped
    /// on read; the rewrite is plain JSON (comments are not preserved).
    Jsonc,
    Toml,
    /// A config we won't auto-edit safely (e.g. Aider's hand-maintained
    /// `.aider.conf.yml` — YAML with no comment-preserving editor). `asd mcp`
    /// prints manual setup guidance instead of writing the file.
    Manual,
}

/// Shape of a single server entry under the tool's server map. The server map
/// itself is an object keyed by server name; only the per-entry fields differ.
#[derive(Clone, Copy)]
enum EntryStyle {
    /// `{ command, args, env }` — the de-facto standard (Claude*, Cursor,
    /// Gemini CLI, Windsurf, Cline, Codex).
    Standard,
    /// Zed `context_servers`: standard fields plus `"source": "custom"`.
    ZedCustom,
    /// VS Code `servers`: standard fields plus `"type": "stdio"`.
    VsCodeStdio,
}

struct Tool {
    name: &'static str,
    config_path_fn: fn() -> Option<PathBuf>,
    format: ConfigFormat,
    /// Top-level key holding the server map.
    servers_key: &'static str,
    entry_style: EntryStyle,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "claude-code",
        config_path_fn: claude_code_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "claude-desktop",
        config_path_fn: claude_desktop_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "cursor",
        config_path_fn: cursor_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "gemini-cli",
        config_path_fn: gemini_cli_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "windsurf",
        config_path_fn: windsurf_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "zed",
        config_path_fn: zed_config_path,
        format: ConfigFormat::Json,
        servers_key: "context_servers",
        entry_style: EntryStyle::ZedCustom,
    },
    Tool {
        name: "vscode",
        config_path_fn: vscode_config_path,
        format: ConfigFormat::Json,
        servers_key: "servers",
        entry_style: EntryStyle::VsCodeStdio,
    },
    Tool {
        name: "cline",
        config_path_fn: cline_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "codex-cli",
        config_path_fn: codex_cli_config_path,
        format: ConfigFormat::Toml,
        servers_key: "mcp_servers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "kilo-code",
        config_path_fn: kilo_code_config_path,
        format: ConfigFormat::Jsonc,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "antigravity",
        config_path_fn: antigravity_config_path,
        format: ConfigFormat::Json,
        servers_key: "mcpServers",
        entry_style: EntryStyle::Standard,
    },
    Tool {
        name: "aider",
        config_path_fn: aider_config_path,
        format: ConfigFormat::Manual,
        servers_key: "mcp-servers",
        entry_style: EntryStyle::Standard,
    },
];

/// Comma-separated list of registered tool names, for help text and errors.
/// Derived from [`TOOLS`] so it can never drift out of sync.
fn valid_tool_names() -> String {
    TOOLS.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
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

fn zed_config_path() -> Option<PathBuf> {
    Some(home()?.join(".config/zed/settings.json"))
}

/// VS Code user-profile MCP config (`mcp.json`, top-level `servers`).
fn vscode_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return Some(home()?.join("Library/Application Support/Code/User/mcp.json"));
    #[cfg(target_os = "linux")]
    return Some(home()?.join(".config/Code/User/mcp.json"));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return None;
}

/// Cline stores MCP settings in the VS Code extension's globalStorage.
fn cline_config_path() -> Option<PathBuf> {
    let rel = "globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json";
    #[cfg(target_os = "macos")]
    return Some(
        home()?
            .join("Library/Application Support/Code/User")
            .join(rel),
    );
    #[cfg(target_os = "linux")]
    return Some(home()?.join(".config/Code/User").join(rel));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return None;
}

fn codex_cli_config_path() -> Option<PathBuf> {
    Some(home()?.join(".codex/config.toml"))
}

/// Kilo Code (v7.0.33+) uses a JSONC config under XDG config.
fn kilo_code_config_path() -> Option<PathBuf> {
    Some(home()?.join(".config/kilo/kilo.jsonc"))
}

/// Google Antigravity's global MCP config (separate from Gemini CLI's
/// settings.json, though both live under ~/.gemini).
fn antigravity_config_path() -> Option<PathBuf> {
    Some(home()?.join(".gemini/config/mcp_config.json"))
}

/// Aider's config — YAML, hand-maintained, edited manually (see ConfigFormat::Manual).
fn aider_config_path() -> Option<PathBuf> {
    Some(home()?.join(".aider.conf.yml"))
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

    /// Inject (or refresh) a managed ASD usage block into the repo's agent
    /// instruction files (AGENTS.md + CLAUDE.md by default).
    Instructions(InstructionsArgs),
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

#[derive(Debug, Args)]
pub struct InstructionsArgs {
    /// Instruction files to write, repo-relative. Defaults to AGENTS.md +
    /// CLAUDE.md (the most widely-read conventions). Repeatable.
    #[arg(long)]
    pub file: Vec<String>,

    /// Remove the managed ASD block instead of writing it.
    #[arg(long)]
    pub remove: bool,

    /// Also add a non-blocking Claude Code PreToolUse hook (on Grep/Glob) to
    /// the project's .claude/settings.json that nudges toward ASD's structured
    /// search. Repo-scoped, not global. Honors --remove.
    #[arg(long)]
    pub hooks: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(_cfg: &Config, cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::Install(args) => install(args),
        McpCmd::Uninstall(args) => uninstall(args),
        McpCmd::Status(_) => status(),
        McpCmd::Instructions(args) => instructions(args),
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

/// Read a JSON/JSONC config file, returning an empty object if it doesn't
/// exist. For JSONC, comments are stripped (string-aware) before parsing.
fn read_config(path: &Path, format: ConfigFormat) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let raw = if format == ConfigFormat::Jsonc {
        strip_jsonc_comments(&raw)
    } else {
        raw
    };
    serde_json::from_str(&raw).with_context(|| format!("parse JSON in {}", path.display()))
}

/// Strip `//` line and `/* */` block comments from JSONC, leaving comment-like
/// sequences inside string literals (e.g. `"http://x"`) untouched. UTF-8 safe.
fn strip_jsonc_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push('"');
        } else if c == '/' && chars.peek() == Some(&'/') {
            for n in chars.by_ref() {
                if n == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next(); // consume '*'
            let mut prev = '\0';
            for n in chars.by_ref() {
                if prev == '*' && n == '/' {
                    break;
                }
                prev = n;
            }
        } else {
            out.push(c);
        }
    }
    out
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

/// Build the server entry for a tool, applying any style-specific extra fields
/// on top of the standard `{ command, args, env }` body.
fn build_entry(style: EntryStyle, asd_mcp_bin: &Path, db_path: &Path) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "command": asd_mcp_bin.to_string_lossy(),
        "args": [],
        "env": {
            "ASD_DB": db_path.to_string_lossy()
        }
    });
    let obj = entry.as_object_mut().expect("entry is an object");
    match style {
        EntryStyle::Standard => {}
        EntryStyle::ZedCustom => {
            obj.insert("source".to_string(), serde_json::json!("custom"));
        }
        EntryStyle::VsCodeStdio => {
            obj.insert("type".to_string(), serde_json::json!("stdio"));
        }
    }
    entry
}

fn tools_to_process(tool_filter: Option<&str>) -> Vec<&'static Tool> {
    TOOLS
        .iter()
        .filter(|t| tool_filter.map_or(true, |f| t.name == f))
        .collect()
}

/// Insert (or overwrite) the asd server entry under `servers_key` in `cfg`.
/// Returns `true` if an asd entry was already present (i.e. this was an update).
fn upsert_server(
    cfg: &mut serde_json::Value,
    servers_key: &str,
    entry: &serde_json::Value,
) -> Result<bool> {
    let servers = cfg
        .as_object_mut()
        .context("config root is not an object")?
        .entry(servers_key)
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .with_context(|| format!("{servers_key} is not an object"))?;
    let already = servers.contains_key(SERVER_NAME);
    servers.insert(SERVER_NAME.to_string(), entry.clone());
    Ok(already)
}

/// Remove the asd server entry from `servers_key` in `cfg`.
/// Returns `true` if an entry was present and removed.
fn remove_server(cfg: &mut serde_json::Value, servers_key: &str) -> bool {
    cfg.get_mut(servers_key)
        .and_then(|s| s.as_object_mut())
        .map(|m| m.remove(SERVER_NAME).is_some())
        .unwrap_or(false)
}

// --- TOML path (toml_edit, format-preserving) ------------------------------

/// Insert/overwrite the asd server entry in a TOML config, preserving the rest
/// of the document (comments, ordering, other tables). Returns whether an asd
/// entry already existed. Used for Codex CLI's `~/.codex/config.toml`.
fn toml_upsert_server(
    path: &Path,
    servers_key: &str,
    asd_mcp: &Path,
    db_path: &Path,
) -> Result<bool> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};

    let mut doc = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?
            .parse::<DocumentMut>()
            .with_context(|| format!("parse TOML in {}", path.display()))?
    } else {
        DocumentMut::new()
    };

    let servers = doc
        .as_table_mut()
        .entry(servers_key)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .with_context(|| format!("{servers_key} is not a table"))?;
    // Render as `[mcp_servers.asd]` headers rather than an explicit, empty
    // `[mcp_servers]` parent.
    servers.set_implicit(true);

    let already = servers.contains_key(SERVER_NAME);

    let mut entry = Table::new();
    entry["command"] = value(asd_mcp.to_string_lossy().into_owned());
    entry["args"] = value(Array::new());
    let mut env = Table::new();
    env["ASD_DB"] = value(db_path.to_string_lossy().into_owned());
    entry["env"] = Item::Table(env);
    servers[SERVER_NAME] = Item::Table(entry);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dirs for {}", path.display()))?;
    }
    std::fs::write(path, doc.to_string()).with_context(|| format!("write {}", path.display()))?;
    Ok(already)
}

/// Remove the asd server entry from a TOML config, preserving the rest.
/// Returns `true` if an entry was present and removed.
fn toml_remove_server(path: &Path, servers_key: &str) -> Result<bool> {
    use toml_edit::DocumentMut;

    if !path.exists() {
        return Ok(false);
    }
    let mut doc = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .parse::<DocumentMut>()
        .with_context(|| format!("parse TOML in {}", path.display()))?;
    let removed = doc
        .get_mut(servers_key)
        .and_then(|i| i.as_table_mut())
        .map(|t| t.remove(SERVER_NAME).is_some())
        .unwrap_or(false);
    if removed {
        std::fs::write(path, doc.to_string())
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(removed)
}

/// Read the registered asd entry's `(command, ASD_DB)` from a config file of
/// either format, if present. Read-only — used by `status`.
fn read_registered_entry(
    path: &Path,
    format: ConfigFormat,
    servers_key: &str,
) -> Option<(String, String)> {
    match format {
        ConfigFormat::Json | ConfigFormat::Jsonc => {
            let cfg = read_config(path, format).ok()?;
            let e = cfg.get(servers_key)?.get(SERVER_NAME)?;
            let cmd = e.get("command")?.as_str()?.to_string();
            let db = e.get("env")?.get("ASD_DB")?.as_str()?.to_string();
            Some((cmd, db))
        }
        ConfigFormat::Toml => {
            let doc = std::fs::read_to_string(path)
                .ok()?
                .parse::<toml_edit::DocumentMut>()
                .ok()?;
            let e = doc.get(servers_key)?.get(SERVER_NAME)?;
            let cmd = e.get("command")?.as_str()?.to_string();
            let db = e.get("env")?.get("ASD_DB")?.as_str()?.to_string();
            Some((cmd, db))
        }
        ConfigFormat::Manual => None,
    }
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
        anyhow::bail!(
            "unknown tool {:?}; valid: {}",
            args.tool,
            valid_tool_names()
        );
    }

    let mut installed = 0usize;
    let mut manual = 0usize;

    for tool in &tools {
        let Some(cfg_path) = (tool.config_path_fn)() else {
            continue;
        };

        // Tools we won't auto-edit: print manual setup guidance and move on.
        if tool.format == ConfigFormat::Manual {
            manual += 1;
            eprintln!("  {} — manual setup (not auto-edited):", tool.name);
            eprintln!("      add an MCP server to {} with", cfg_path.display());
            eprintln!("        command: {}", asd_mcp.display());
            eprintln!("        env ASD_DB: {}", db_path.display());
            eprintln!(
                "      (key `{}`; see the tool's MCP docs for the exact schema)",
                tool.servers_key
            );
            continue;
        }

        let already = match tool.format {
            ConfigFormat::Json | ConfigFormat::Jsonc => {
                let entry = build_entry(tool.entry_style, &asd_mcp, &db_path);
                let mut cfg = read_config(&cfg_path, tool.format)?;
                let already = upsert_server(&mut cfg, tool.servers_key, &entry)?;
                write_config(&cfg_path, &cfg)?;
                already
            }
            ConfigFormat::Toml => {
                toml_upsert_server(&cfg_path, tool.servers_key, &asd_mcp, &db_path)?
            }
            ConfigFormat::Manual => unreachable!("handled above"),
        };

        if already {
            eprintln!("  {} updated  {}", tool.name, cfg_path.display());
        } else {
            eprintln!("  {} installed  {}", tool.name, cfg_path.display());
        }
        installed += 1;
    }

    if installed == 0 && manual == 0 {
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
        anyhow::bail!(
            "unknown tool {:?}; valid: {}",
            args.tool,
            valid_tool_names()
        );
    }

    let mut removed = 0usize;

    for tool in &tools {
        let Some(cfg_path) = (tool.config_path_fn)() else {
            continue;
        };
        if tool.format == ConfigFormat::Manual {
            eprintln!(
                "  {} — remove the asd MCP server manually from {}",
                tool.name,
                cfg_path.display()
            );
            continue;
        }
        if !cfg_path.exists() {
            continue;
        }

        let was_present = match tool.format {
            ConfigFormat::Json | ConfigFormat::Jsonc => {
                let mut cfg = read_config(&cfg_path, tool.format)?;
                let present = remove_server(&mut cfg, tool.servers_key);
                if present {
                    write_config(&cfg_path, &cfg)?;
                }
                present
            }
            ConfigFormat::Toml => toml_remove_server(&cfg_path, tool.servers_key)?,
            ConfigFormat::Manual => unreachable!("handled above"),
        };

        if was_present {
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

        if tool.format == ConfigFormat::Manual {
            eprintln!("  ⚙ {}  manual setup ({})", tool.name, cfg_path.display());
            continue;
        }

        if !cfg_path.exists() {
            eprintln!(
                "  ✗ {}  config not found: {}",
                tool.name,
                cfg_path.display()
            );
            continue;
        }

        match read_registered_entry(&cfg_path, tool.format, tool.servers_key) {
            Some((cmd, db)) => {
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
// instructions — inject a managed ASD usage block into agent instruction files
// ---------------------------------------------------------------------------

// Marker aligns with the shared engine's rendered block (`<!-- asd:begin -->`),
// so `render_always_on` output IS the managed block. `upsert_block` keys on
// these. (Migration note: the older descriptive marker is not recognized; the
// `asd mcp instructions` feature is new enough that no deployed block relies on
// it.)
const BLOCK_BEGIN: &str = "<!-- asd:begin -->";
const BLOCK_END: &str = "<!-- asd:end -->";

/// The managed instruction block — rendered from ASD's single onboarding
/// `SkillSpec` via the shared engine (suite-onboarding t-007), so the always-on
/// block and the installed `SKILL.md` train the agent from one source.
fn instruction_body() -> String {
    agent_skillgen::render_always_on(&crate::commands::skill::asd_skill_spec())
}

/// Insert or replace the managed block in `content`. Returns the new content.
/// When `remove` is true, the block is stripped instead.
fn upsert_block(content: &str, block: &str, remove: bool) -> String {
    if let (Some(start), Some(end_idx)) = (content.find(BLOCK_BEGIN), content.find(BLOCK_END)) {
        // Replace from BEGIN through END (plus the marker text).
        let end = end_idx + BLOCK_END.len();
        let before = content[..start].trim_end_matches('\n');
        let after = content[end..].trim_start_matches('\n');
        let mut out = String::new();
        if !before.is_empty() {
            out.push_str(before);
            out.push_str("\n\n");
        }
        if !remove {
            out.push_str(block);
        }
        if !after.is_empty() {
            if !remove {
                out.push('\n');
            }
            out.push_str(after);
            out.push('\n');
        }
        out
    } else if remove {
        content.to_string()
    } else if content.trim().is_empty() {
        block.to_string()
    } else {
        format!("{}\n\n{block}", content.trim_end_matches('\n'))
    }
}

fn instructions(args: InstructionsArgs) -> Result<()> {
    let files = if args.file.is_empty() {
        vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()]
    } else {
        args.file
    };
    let block = instruction_body();
    let cwd = std::env::current_dir().context("cannot determine current directory")?;

    for rel in &files {
        let path = cwd.join(rel);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let had_block = existing.contains(BLOCK_BEGIN);
        let updated = upsert_block(&existing, &block, args.remove);

        if updated == existing {
            eprintln!("  {rel} — no change");
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dirs for {}", path.display()))?;
        }
        std::fs::write(&path, &updated).with_context(|| format!("write {}", path.display()))?;
        let verb = if args.remove {
            "removed ASD block from"
        } else if had_block {
            "refreshed ASD block in"
        } else {
            "added ASD block to"
        };
        eprintln!("  {verb} {rel}");
    }

    if args.hooks {
        let path = cwd.join(".claude/settings.json");
        let mut settings = read_config(&path, ConfigFormat::Json)?;
        if upsert_claude_hook(&mut settings, args.remove) {
            write_config(&path, &settings)?;
            let verb = if args.remove { "removed" } else { "added" };
            eprintln!("  {verb} ASD PreToolUse hook in .claude/settings.json");
        } else {
            eprintln!("  .claude/settings.json — no change");
        }
    }
    Ok(())
}

/// Substring identifying ASD's hook entry (for idempotency / removal).
const HOOK_MARKER: &str = "asd search / asd context-for";

fn asd_hook_entry() -> serde_json::Value {
    serde_json::json!({
        "matcher": "Grep|Glob",
        "hooks": [{
            "type": "command",
            "command": "echo 'ASD indexes this repo — prefer asd search / asd context-for for \
                        structured answers, often cheaper than raw grep/glob.'"
        }]
    })
}

/// Add or remove ASD's non-blocking PreToolUse hook in a Claude settings value.
/// Returns whether anything changed.
fn upsert_claude_hook(settings: &mut serde_json::Value, remove: bool) -> bool {
    let Some(obj) = settings.as_object_mut() else {
        return false;
    };
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return false;
    };
    let pre = hooks
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    let Some(arr) = pre.as_array_mut() else {
        return false;
    };
    let existing = arr.iter().position(|e| e.to_string().contains(HOOK_MARKER));
    if remove {
        if let Some(i) = existing {
            arr.remove(i);
            return true;
        }
        return false;
    }
    if existing.is_some() {
        return false;
    }
    arr.push(asd_hook_entry());
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_block_appends_to_existing_content() {
        let block = instruction_body();
        let out = upsert_block("# My project\n\nNotes here.\n", &block, false);
        assert!(out.starts_with("# My project"));
        assert!(out.contains(BLOCK_BEGIN) && out.contains(BLOCK_END));
        assert!(out.contains("asd context-for"));
    }

    #[test]
    fn upsert_block_is_idempotent() {
        let block = instruction_body();
        let once = upsert_block("# P\n", &block, false);
        let twice = upsert_block(&once, &block, false);
        assert_eq!(once, twice, "re-running must not duplicate the block");
        assert_eq!(twice.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn upsert_block_refreshes_in_place() {
        // An old/edited block between markers is replaced, surrounding text kept.
        let stale = format!("# P\n\n{BLOCK_BEGIN}\nOLD\n{BLOCK_END}\n\n## After\n");
        let out = upsert_block(&stale, &instruction_body(), false);
        assert!(out.contains("# P") && out.contains("## After"));
        assert!(!out.contains("OLD"));
        assert!(out.contains("asd architecture"));
        assert_eq!(out.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn upsert_block_remove_strips_it() {
        let with = upsert_block("# P\n\nbody\n", &instruction_body(), false);
        let without = upsert_block(&with, &instruction_body(), true);
        assert!(!without.contains(BLOCK_BEGIN));
        assert!(without.contains("# P") && without.contains("body"));
    }

    #[test]
    fn claude_hook_add_idempotent_and_preserves_other_hooks() {
        let mut s = serde_json::json!({
            "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [] } ] },
            "model": "x"
        });
        assert!(upsert_claude_hook(&mut s, false), "first add changes");
        assert!(!upsert_claude_hook(&mut s, false), "second add is a no-op");
        let arr = s["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "ours added alongside the existing Bash hook");
        assert_eq!(s["model"], "x", "unrelated settings preserved");
    }

    #[test]
    fn claude_hook_remove() {
        let mut s = serde_json::json!({});
        upsert_claude_hook(&mut s, false);
        assert!(upsert_claude_hook(&mut s, true), "remove changes");
        assert!(s["hooks"]["PreToolUse"].as_array().unwrap().is_empty());
        assert!(
            !upsert_claude_hook(&mut s, true),
            "second remove is a no-op"
        );
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate tool name in TOOLS");
    }

    #[test]
    fn registry_includes_all_tools() {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        for expected in [
            "claude-code",
            "claude-desktop",
            "cursor",
            "gemini-cli",
            "windsurf",
            "zed",
            "vscode",
            "cline",
            "codex-cli",
            "kilo-code",
            "antigravity",
            "aider",
        ] {
            assert!(names.contains(&expected), "TOOLS missing {expected}");
        }
        // valid_tool_names() is derived from TOOLS, so it must list them too.
        let listed = valid_tool_names();
        assert!(listed.contains("zed") && listed.contains("vscode") && listed.contains("cline"));
        assert!(listed.contains("codex-cli") && listed.contains("kilo-code"));
        assert!(listed.contains("antigravity") && listed.contains("aider"));
    }

    #[test]
    fn jsonc_comments_stripped_but_strings_preserved() {
        let src =
            "{\n  // line comment\n  \"url\": \"http://x/a\", /* block */\n  \"k\": \"a//b\"\n}";
        let parsed: serde_json::Value = serde_json::from_str(&strip_jsonc_comments(src)).unwrap();
        // The "//" inside string values must survive.
        assert_eq!(parsed["url"], "http://x/a");
        assert_eq!(parsed["k"], "a//b");
    }

    #[test]
    fn jsonc_round_trip_preserves_sibling_servers() {
        // A commented kilo.jsonc with an existing server: read (strip), upsert, write.
        let dir = std::env::temp_dir().join(format!("asd_mcp_jsonc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kilo.jsonc");
        std::fs::write(
            &path,
            "{\n  // my config\n  \"mcpServers\": { \"other\": { \"command\": \"x\" } }\n}",
        )
        .unwrap();
        let mut cfg = read_config(&path, ConfigFormat::Jsonc).unwrap();
        upsert_server(&mut cfg, "mcpServers", &std_entry()).unwrap();
        write_config(&path, &cfg).unwrap();
        // Re-read: both servers present (comments are gone, which is expected).
        let back = read_config(&path, ConfigFormat::Jsonc).unwrap();
        assert!(back["mcpServers"]["other"].is_object());
        assert!(back["mcpServers"]["asd"].is_object());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn std_entry() -> serde_json::Value {
        build_entry(
            EntryStyle::Standard,
            Path::new("/bin/asd-mcp"),
            Path::new("/db"),
        )
    }

    #[test]
    fn standard_entry_has_expected_shape() {
        let entry = build_entry(
            EntryStyle::Standard,
            Path::new("/usr/local/bin/asd-mcp"),
            Path::new("/repo/.asd-state.db"),
        );
        assert_eq!(entry["command"], "/usr/local/bin/asd-mcp");
        assert!(entry["args"].as_array().unwrap().is_empty());
        assert_eq!(entry["env"]["ASD_DB"], "/repo/.asd-state.db");
        // No style markers on the standard entry.
        assert!(entry.get("source").is_none() && entry.get("type").is_none());
    }

    #[test]
    fn divergent_entry_styles_add_their_marker() {
        let zed = build_entry(
            EntryStyle::ZedCustom,
            Path::new("/bin/asd-mcp"),
            Path::new("/db"),
        );
        assert_eq!(zed["source"], "custom");
        assert_eq!(zed["command"], "/bin/asd-mcp"); // standard body still present

        let vscode = build_entry(
            EntryStyle::VsCodeStdio,
            Path::new("/bin/asd-mcp"),
            Path::new("/db"),
        );
        assert_eq!(vscode["type"], "stdio");
        assert_eq!(vscode["env"]["ASD_DB"], "/db");
    }

    #[test]
    fn upsert_uses_the_tools_servers_key() {
        // Zed nests under context_servers, not mcpServers.
        let mut cfg = serde_json::json!({});
        let entry = build_entry(
            EntryStyle::ZedCustom,
            Path::new("/bin/asd-mcp"),
            Path::new("/db"),
        );
        let already = upsert_server(&mut cfg, "context_servers", &entry).unwrap();
        assert!(!already);
        assert_eq!(cfg["context_servers"]["asd"]["source"], "custom");
        assert!(cfg.get("mcpServers").is_none(), "must not touch mcpServers");
    }

    #[test]
    fn upsert_into_empty_config_creates_entry() {
        let mut cfg = serde_json::json!({});
        let already = upsert_server(&mut cfg, "mcpServers", &std_entry()).unwrap();
        assert!(!already, "fresh insert should not report already-present");
        assert_eq!(cfg["mcpServers"]["asd"]["command"], "/bin/asd-mcp");
    }

    #[test]
    fn upsert_is_idempotent_and_reports_update() {
        let mut cfg = serde_json::json!({});
        assert!(!upsert_server(&mut cfg, "mcpServers", &std_entry()).unwrap());
        // Second insert overwrites and reports the prior presence.
        assert!(upsert_server(&mut cfg, "mcpServers", &std_entry()).unwrap());
        assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn upsert_preserves_sibling_servers() {
        let mut cfg = serde_json::json!({
            "servers": { "other": { "command": "x" } },
            "someOtherKey": 1
        });
        upsert_server(&mut cfg, "servers", &std_entry()).unwrap();
        assert_eq!(cfg["servers"]["other"]["command"], "x");
        assert_eq!(cfg["someOtherKey"], 1);
        assert!(cfg["servers"]["asd"].is_object());
    }

    #[test]
    fn remove_returns_false_when_absent_true_when_present() {
        let mut cfg = serde_json::json!({});
        assert!(!remove_server(&mut cfg, "mcpServers"), "nothing to remove");
        upsert_server(&mut cfg, "mcpServers", &std_entry()).unwrap();
        assert!(
            remove_server(&mut cfg, "mcpServers"),
            "should remove the asd entry"
        );
        assert!(
            !remove_server(&mut cfg, "mcpServers"),
            "second remove is a no-op"
        );
    }

    fn temp_toml(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("asd_mcp_test_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn toml_upsert_preserves_comments_and_siblings() {
        let path = temp_toml("preserve");
        std::fs::write(
            &path,
            "# hand-written codex config\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();

        let already = toml_upsert_server(
            &path,
            "mcp_servers",
            Path::new("/bin/asd-mcp"),
            Path::new("/db"),
        )
        .unwrap();
        assert!(!already, "fresh insert");

        let txt = std::fs::read_to_string(&path).unwrap();
        assert!(txt.contains("# hand-written codex config"), "comment kept");
        assert!(txt.contains("model = \"o3\""), "sibling setting kept");
        assert!(txt.contains("[mcp_servers.other]"), "sibling server kept");
        assert!(txt.contains("[mcp_servers.asd]"), "asd table written");
        assert!(txt.contains("ASD_DB"), "env written");

        // Read-only accessor sees the registration.
        let (cmd, db) = read_registered_entry(&path, ConfigFormat::Toml, "mcp_servers").unwrap();
        assert_eq!(cmd, "/bin/asd-mcp");
        assert_eq!(db, "/db");

        // Idempotent: second upsert reports already-present.
        assert!(
            toml_upsert_server(
                &path,
                "mcp_servers",
                Path::new("/bin/asd-mcp"),
                Path::new("/db")
            )
            .unwrap()
        );

        // Remove leaves the rest of the document intact.
        assert!(toml_remove_server(&path, "mcp_servers").unwrap());
        let txt2 = std::fs::read_to_string(&path).unwrap();
        assert!(!txt2.contains("[mcp_servers.asd]"), "asd removed");
        assert!(
            txt2.contains("# hand-written codex config"),
            "comment survives removal"
        );
        assert!(
            txt2.contains("[mcp_servers.other]"),
            "sibling survives removal"
        );
        assert!(
            !toml_remove_server(&path, "mcp_servers").unwrap(),
            "second remove no-op"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn toml_upsert_creates_file_when_absent() {
        let path = temp_toml("create");
        let _ = std::fs::remove_file(&path);
        toml_upsert_server(
            &path,
            "mcp_servers",
            Path::new("/bin/asd-mcp"),
            Path::new("/db"),
        )
        .unwrap();
        let (cmd, _db) = read_registered_entry(&path, ConfigFormat::Toml, "mcp_servers").unwrap();
        assert_eq!(cmd, "/bin/asd-mcp");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
