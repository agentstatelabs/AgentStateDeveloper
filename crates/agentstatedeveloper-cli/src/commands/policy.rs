//! `asd policy …` — introspect the loaded policy file.
//!
//! All subcommands require a policy file path (via `--policy` / `ASD_POLICY`).
//! Without one, the CLI errors out with a clear message — introspection
//! doesn't make sense against the permissive default.

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use serde_json::json;

use agentstatedeveloper_core::{Decision, FilePolicyGate, PolicyFile, PolicyGate, Situation};

use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum PolicyCmd {
    /// List loaded policy rules. Optionally filter by path prefix.
    List(ListArgs),

    /// Show a specific policy rule by `path`.
    Show(ShowArgs),

    /// Evaluate a hypothetical action against the loaded policy file.
    /// Useful for "would this be allowed if I tried?" previews.
    Evaluate(EvaluateArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Only list rules whose path starts with this prefix.
    #[arg(long)]
    pub prefix: Option<String>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Full policy path (as written in the policy file, e.g.
    /// `/policies/code/hazard-requires-human`).
    pub path: String,
}

#[derive(Debug, Args)]
pub struct EvaluateArgs {
    /// Action to evaluate, e.g. `asd.ledger.append.hazard`.
    pub action: String,

    /// Agent id to attribute to the query. Defaults to the CLI's default.
    #[arg(long, default_value = "asd-cli-user")]
    pub agent_id: String,

    /// Optional human-readable situation description (not evaluated).
    #[arg(long)]
    pub description: Option<String>,
}

pub fn run(cfg: &Config, cmd: PolicyCmd) -> Result<()> {
    let path = cfg.policy_path.as_ref().ok_or_else(|| {
        anyhow!("no policy file configured — pass --policy <path> or set ASD_POLICY")
    })?;

    match cmd {
        PolicyCmd::List(args) => list(path, args),
        PolicyCmd::Show(args) => show(path, args),
        PolicyCmd::Evaluate(args) => evaluate(path, args),
    }
}

fn list(path: &std::path::Path, args: ListArgs) -> Result<()> {
    let file =
        PolicyFile::load(path).with_context(|| format!("load policy file {}", path.display()))?;
    let filtered: Vec<_> = file
        .policies
        .iter()
        .filter(|r| args.prefix.as_deref().is_none_or(|p| r.path.starts_with(p)))
        .collect();

    let out = json!({
        "source": path.display().to_string(),
        "strict": file.strict,
        "count": filtered.len(),
        "policies": filtered.iter().map(|r| json!({
            "path": r.path,
            "version": r.version,
            "description": r.description,
            "match_action": r.match_action,
            "deny": r.deny,
            "require_approval": r.require_approval,
            "agent_id": r.agent_id,
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn show(path: &std::path::Path, args: ShowArgs) -> Result<()> {
    let file = PolicyFile::load(path)?;
    let rule = file
        .policies
        .iter()
        .find(|r| r.path == args.path)
        .ok_or_else(|| anyhow!("policy not found: {}", args.path))?;
    println!("{}", serde_json::to_string_pretty(rule)?);
    Ok(())
}

fn evaluate(path: &std::path::Path, args: EvaluateArgs) -> Result<()> {
    let gate = FilePolicyGate::from_file(path)?;
    let situation = Situation {
        description: args
            .description
            .unwrap_or_else(|| format!("dry-run evaluation of {}", args.action)),
        qualifiers: serde_json::Value::Null,
    };
    let decision = gate.evaluate(&situation, &args.action, &args.agent_id)?;

    let out = match &decision {
        Decision::Allow { matched_policy } => json!({
            "status": "allowed",
            "matched_policy": matched_policy,
        }),
        Decision::Deny {
            matched_policy,
            reason,
        } => json!({
            "status": "denied",
            "matched_policy": matched_policy,
            "reason": reason,
        }),
        Decision::RequireApproval {
            matched_policy,
            approvers,
            reason,
        } => json!({
            "status": "awaiting-approval",
            "matched_policy": matched_policy,
            "approvers": approvers,
            "reason": reason,
        }),
        Decision::NoPolicyMatch => json!({
            "status": "no-policy-match",
            "matched_policy": serde_json::Value::Null,
        }),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
