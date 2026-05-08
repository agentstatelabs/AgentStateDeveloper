//! `asd status` — show index health, age, modified files, and sidecar lifecycle.

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Engine, IndexStore, LedgerStore,
    SearchFtsDb, SidecarState, format_age, sidecar_lifecycle_state,
    schema::{LedgerKind, Symbol},
};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Show source files modified since the last index run (requires git).
    #[arg(long)]
    pub show_dirty: bool,

    /// Emit machine-readable JSON instead of the default human text.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cfg: &Config, args: StatusArgs) -> Result<()> {
    let fts = SearchFtsDb::open(&cfg.db_path)?;

    let project_root = cfg.db_path.parent().unwrap_or(std::path::Path::new("."));
    let sidecar_state = sidecar_lifecycle_state(project_root);

    if !fts.has_data() {
        if args.json {
            println!("{}", json!({
                "state": "empty",
                "note": "run 'asd index <dir>' to build",
                "sidecar": sidecar_state_key(&sidecar_state),
            }));
        } else {
            println!("ASD index status");
            println!("  db:       {}", cfg.db_path.display());
            println!("  state:    empty — run 'asd index <dir>' to build");
        }
        return Ok(());
    }

    let count = fts.symbol_count();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (indexed_at, age_hours, fresh) = match fts.last_indexed_at() {
        Some(ts) => {
            let age_h = (now - ts).max(0) / 3600;
            (Some(ts), Some(age_h), age_h == 0)
        }
        None => (None, None, false),
    };

    let dirty_files = if args.show_dirty || args.json {
        collect_dirty_files(cfg)
    } else {
        vec![]
    };

    // Concept-gap detection: symbols with Ownership but no Concept entry.
    let concept_gaps: Vec<serde_json::Value> = if args.json {
        if let Ok(engine) = Engine::open_sqlite(&cfg.db_path) {
            let index_store = AsgIndexStore { repo: &engine.repo };
            let ledger_store = AsgLedgerStore { repo: &engine.repo };
            let tree = engine.repo
                .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
                .unwrap_or(serde_json::Value::Object(Default::default()));
            tree.as_object()
                .map(|m| {
                    m.values()
                        .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                        .filter_map(|sym| {
                            let entries = ledger_store
                                .list_entries(&engine.ref_name, &sym.symbol_id)
                                .unwrap_or_default();
                            let has_ownership = entries.iter().any(|e| e.kind == LedgerKind::Ownership);
                            let has_concept = entries.iter().any(|e| e.kind == LedgerKind::Concept);
                            if has_ownership && !has_concept {
                                Some(json!({"qname": sym.qname, "file": sym.file}))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let sidecar_key = sidecar_state_key(&sidecar_state);
    let sidecar_action = sidecar_action_hint(&sidecar_state);

    if args.json {
        let index_state = if fresh { "fresh" } else if age_hours.unwrap_or(0) >= 1 { "stale" } else { "ok" };
        let out = json!({
            "db": cfg.db_path.display().to_string(),
            "symbols": count,
            "indexed_at_unix": indexed_at,
            "age_hours": age_hours,
            "state": index_state,
            "sidecar": sidecar_key,
            "sidecar_action": sidecar_action,
            "dirty_files": dirty_files,
            "concept_gaps": concept_gaps,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // Human-readable output.
    println!("ASD index status");
    println!("  db:       {}", cfg.db_path.display());
    println!("  symbols:  {count}");

    match indexed_at {
        Some(ts) => {
            println!("  indexed:  {} (unix {})", format_age(ts), ts);
            if age_hours.unwrap_or(0) >= 1 {
                println!("  warning:  index is {}h old — consider re-running 'asd index'", age_hours.unwrap_or(0));
            } else {
                println!("  state:    fresh");
            }
        }
        None => println!("  indexed:  unknown"),
    }

    let sidecar_label = match sidecar_state {
        SidecarState::Missing   => "missing — run 'asd sync' to create",
        SidecarState::Present   => "present — run 'asd hydrate' to load into ASG",
        SidecarState::Hydrated  => "hydrated",
        SidecarState::FreshReset => "fresh-reset (deliberate reset — re-run 'asd index' + 'asd sync')",
    };
    println!("  sidecar:  {sidecar_label}");

    if args.show_dirty {
        let files = dirty_files;
        if files.is_empty() {
            println!("  dirty:    none (all tracked source files match index)");
        } else {
            println!("  dirty:    {} modified source file(s) since last commit:", files.len());
            for f in &files {
                println!("            {}", f);
            }
        }
    }

    Ok(())
}

fn sidecar_state_key(s: &SidecarState) -> &'static str {
    match s {
        SidecarState::Missing    => "missing",
        SidecarState::Present    => "present",
        SidecarState::Hydrated   => "hydrated",
        SidecarState::FreshReset => "fresh-reset",
    }
}

fn sidecar_action_hint(s: &SidecarState) -> &'static str {
    match s {
        SidecarState::Missing    => "run 'asd sync' to create sidecar",
        SidecarState::Present    => "run 'asd hydrate' to load sidecar into ASG",
        SidecarState::Hydrated   => "sidecar is current",
        SidecarState::FreshReset => "re-run 'asd index' then 'asd sync'",
    }
}

fn collect_dirty_files(cfg: &Config) -> Vec<String> {
    let workspace = cfg.db_path.parent().unwrap_or(std::path::Path::new("."));
    let output = std::process::Command::new("git")
        .args(["status", "--short", "--untracked-files=no"])
        .current_dir(workspace)
        .output();

    let Ok(out) = output else { return vec![]; };
    if !out.status.success() { return vec![]; }

    let text = String::from_utf8_lossy(&out.stdout);
    let source_exts = [".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go", ".kt", ".java", ".rb", ".cs"];
    text.lines()
        .filter(|l| source_exts.iter().any(|ext| l.ends_with(ext)))
        .map(|l| l.trim().to_string())
        .collect()
}
