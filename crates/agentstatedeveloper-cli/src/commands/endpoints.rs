//! `asd endpoints` — list cross-service endpoints detected in this repo, show
//! the in-repo matched edges, and export a service manifest.
//!
//! Endpoints (HTTP routes/clients, pub-sub) are written to the registry at
//! `/asd/v1/index/endpoints` during `asd index`. This command reads them back,
//! runs in-repo contract matching ([`match_edges`]), and can emit a
//! [`ServiceManifest`] — the unit a Team-tier tool imports for cross-repo
//! matching. (This OSS command does in-repo matching + export only.)

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};

use agentstatedeveloper_core::{Engine, ServiceEndpoint, ServiceManifest, match_edges};

use crate::config::Config;

#[derive(Debug, Args)]
pub struct EndpointsArgs {
    /// Emit a JSON ServiceManifest (repo_id + endpoints) for cross-repo import.
    #[arg(long)]
    pub export: bool,

    /// Machine-readable JSON: { endpoints, edges }.
    #[arg(long)]
    pub agent: bool,
}

pub fn run(cfg: &Config, args: EndpointsArgs) -> Result<()> {
    let engine = Engine::open_sqlite(&cfg.db_path)?;
    let endpoints = load_endpoints(&engine);

    if args.export {
        let repo_id = endpoints
            .first()
            .map(|e| e.repo_id.clone())
            .unwrap_or_default();
        let manifest = ServiceManifest { repo_id, endpoints };
        println!("{}", serde_json::to_string_pretty(&manifest)?);
        return Ok(());
    }

    let edges = match_edges(&endpoints);

    if args.agent {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "endpoints": endpoints,
                "edges": edges,
            }))?
        );
        return Ok(());
    }

    if endpoints.is_empty() {
        println!(
            "No cross-service endpoints detected. Run `asd index` first; \
             HTTP detection currently covers Python (FastAPI/Flask + requests/httpx)."
        );
        return Ok(());
    }

    println!("{} endpoint(s):", endpoints.len());
    for e in &endpoints {
        println!(
            "  {:?}/{:?}  {}  in {}  (conf {:.2})",
            e.transport, e.direction, e.contract, e.qname, e.confidence
        );
    }
    println!();
    if edges.is_empty() {
        println!(
            "No in-repo matched edges. (A match needs a client call and a server route \
             in THIS repo sharing a contract; cross-repo matching is a Team-tier feature.)"
        );
    } else {
        println!("{} in-repo matched edge(s):", edges.len());
        for ed in &edges {
            println!("  {}  ->  {}   [{}]", ed.from.qname, ed.to.qname, ed.contract);
        }
    }
    Ok(())
}

/// Read the endpoint registry (`contract_hash → repo_id → symbol_id → endpoint`)
/// and flatten it into a sorted list.
fn load_endpoints(engine: &Engine) -> Vec<ServiceEndpoint> {
    let tree = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/endpoints")
        .unwrap_or(Value::Null);

    let mut out = Vec::new();
    if let Some(by_contract) = tree.as_object() {
        for by_repo in by_contract.values() {
            let Some(by_repo) = by_repo.as_object() else {
                continue;
            };
            for by_sym in by_repo.values() {
                let Some(by_sym) = by_sym.as_object() else {
                    continue;
                };
                for ep_val in by_sym.values() {
                    if let Ok(ep) = serde_json::from_value::<ServiceEndpoint>(ep_val.clone()) {
                        out.push(ep);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.contract.cmp(&b.contract).then(a.qname.cmp(&b.qname)));
    out
}
