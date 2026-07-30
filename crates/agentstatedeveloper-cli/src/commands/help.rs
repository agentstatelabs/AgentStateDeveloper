//! `asd help [topic]` — on-demand instruction disclosure.
//!
//! Returns compiled-in feature docs (synopsis, syntax, params, examples,
//! gotchas) for one asd feature, or the full catalog. Docs come from
//! [`agentstatedeveloper_core::help`], version-pinned to the running binary, so
//! the CLI and the `asd-mcp` `help` tool return byte-identical payloads.
//!
//! `--publish` writes this binary's manifest into the shared cross-tool index
//! (`$AGENTSTATE_HELP_INDEX`, default `$HOME/.config/agentstate/
//! help-index.json`) so a unified `help` can discover asd's features alongside
//! ctx's. The index is tool-keyed; publishing asd never clobbers ctx's slice.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::help;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct HelpArgs {
    /// Feature name (e.g. "impact") or a phrase (e.g. "blast radius").
    pub topic: Option<String>,

    /// Machine-readable JSON output.
    #[arg(long)]
    pub agent: bool,

    /// Print this binary's manifest (feature -> synopsis -> owner -> version).
    #[arg(long)]
    pub manifest: bool,

    /// Publish this binary's manifest into the shared cross-tool help index,
    /// merging alongside any other tool's entry. Prints the path written.
    #[arg(long)]
    pub publish: bool,

    /// Resolve locally only — do not proxy an unknown topic to the other tool.
    /// Used internally to break the proxy chain (single-hop guard).
    #[arg(long, hide = true)]
    pub no_proxy: bool,
}

pub fn run(_cfg: &Config, args: HelpArgs) -> Result<()> {
    if args.publish {
        let path = publish_help_index()?;
        println!("Published asd help manifest to {}", path.display());
        return Ok(());
    }

    let value = if args.manifest {
        help::manifest()
    } else {
        help::resolve(args.topic.as_deref(), !args.no_proxy)
    };

    if args.agent || args.manifest {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    render_help(&value);
    Ok(())
}

/// Resolve the shared help index path: `$AGENTSTATE_HELP_INDEX`, else
/// `$HOME/.config/agentstate/help-index.json` — same literal `$HOME/.config`
/// convention as asd's `~/.config/asd/repos.toml` registry.
fn help_index_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AGENTSTATE_HELP_INDEX") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/agentstate/help-index.json"))
}

/// Merge this binary's manifest into the shared index, preserving other tools'
/// slices. Returns the path written. Exposed so `asd mcp install` can refresh
/// the index as a best-effort setup step.
pub(crate) fn publish_help_index() -> Result<PathBuf> {
    let manifest = help::manifest();
    let tool = manifest
        .get("tool")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("manifest missing `tool`"))?
        .to_string();
    let path = help_index_path().ok_or_else(|| anyhow!("cannot resolve HOME"))?;

    // Read + merge; tolerate a missing or malformed file by starting fresh so a
    // corrupt index never blocks publishing.
    let mut index = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({ "schema": 1, "tools": {} }));
    if !index.get("tools").map(|t| t.is_object()).unwrap_or(false) {
        index["tools"] = json!({});
    }
    index["schema"] = json!(1);
    index["tools"][&tool] = json!({
        "version": manifest.get("version").cloned().unwrap_or(Value::Null),
        "features": manifest.get("features").cloned().unwrap_or_else(|| json!([])),
    });

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&index)?),
    )
    .map_err(|e| anyhow!("writing {}: {e}", path.display()))?;
    Ok(path)
}

/// Render a help response as readable text (catalog / one feature /
/// disambiguation / not-found).
fn render_help(v: &Value) {
    // Catalog.
    if let Some(groups) = v.get("groups").and_then(|g| g.as_object()) {
        println!(
            "asd help — {} features (v{}). Call `asd help <feature>` for details.\n",
            v.get("feature_count").and_then(|c| c.as_u64()).unwrap_or(0),
            v.get("version").and_then(|s| s.as_str()).unwrap_or("?"),
        );
        for (group, feats) in groups {
            println!("{}:", group);
            if let Some(arr) = feats.as_array() {
                for f in arr {
                    println!(
                        "  {:<16} {}",
                        f.get("feature").and_then(|s| s.as_str()).unwrap_or(""),
                        f.get("synopsis").and_then(|s| s.as_str()).unwrap_or(""),
                    );
                }
            }
            println!();
        }
        return;
    }

    // Disambiguation.
    if let Some(matches) = v.get("matches").and_then(|m| m.as_array()) {
        println!(
            "No exact match for '{}'. Did you mean:",
            v.get("query").and_then(|s| s.as_str()).unwrap_or(""),
        );
        for m in matches {
            println!(
                "  {:<16} {}",
                m.get("feature").and_then(|s| s.as_str()).unwrap_or(""),
                m.get("synopsis").and_then(|s| s.as_str()).unwrap_or(""),
            );
        }
        return;
    }

    // Not found.
    if let Some(nf) = v.get("not_found").and_then(|s| s.as_str()) {
        println!("No asd feature matched '{}'.", nf);
        if let Some(hint) = v.get("hint").and_then(|s| s.as_str()) {
            println!("{}", hint);
        }
        return;
    }

    // Single feature.
    let get = |k: &str| v.get(k).and_then(|s| s.as_str()).unwrap_or("");
    if let Some(from) = v.get("proxied_from").and_then(|s| s.as_str()) {
        println!("(via {from})");
    }
    println!("{} — {}", get("feature"), get("synopsis"));
    println!("  syntax: {}", get("syntax"));
    if let Some(params) = v.get("params").and_then(|p| p.as_array())
        && !params.is_empty()
    {
        println!("  params:");
        for p in params {
            let req = if p.get("required").and_then(|b| b.as_bool()).unwrap_or(false) {
                "(required)"
            } else {
                "(optional)"
            };
            println!(
                "    {:<14} {} {}",
                p.get("name").and_then(|s| s.as_str()).unwrap_or(""),
                req,
                p.get("desc").and_then(|s| s.as_str()).unwrap_or(""),
            );
        }
    }
    let list = |label: &str, key: &str| {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array())
            && !arr.is_empty()
        {
            println!("  {}:", label);
            for item in arr {
                if let Some(s) = item.as_str() {
                    println!("    - {}", s);
                }
            }
        }
    };
    list("examples", "examples");
    list("gotchas", "gotchas");
    list("related", "related");
    println!(
        "  (~{} tokens, v{})",
        v.get("help_tokens").and_then(|t| t.as_u64()).unwrap_or(0),
        get("version"),
    );
}
