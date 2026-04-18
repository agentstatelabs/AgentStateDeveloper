//! AgentStateDeveloper MCP stdio server — exposes ASD read/write operations
//! as MCP tools for coding agents.
//!
//! Patterns mirror `agentstategraph-mcp::server` (same `rmcp` version).

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;

use agentstatedeveloper_core::{
    ASD_PATH_PREFIX, AsgEffectStore, AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Decision,
    Effect, EffectCategory, EffectDecl, EffectStore, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Situation, actions,
};

/// The AgentStateDeveloper MCP server.
#[derive(Clone)]
pub struct AsdMcpServer {
    engine: Arc<Mutex<Engine>>,
    db_path: PathBuf,
    tool_router: ToolRouter<Self>,
}

// -- Parameter types --------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct CodeQueryParams {
    /// Substring match on qualified name.
    pub name_contains: Option<String>,
    /// Filter by symbol kind: module, function, method, class, variable.
    pub kind: Option<String>,
    /// Filter by language (e.g. "python").
    pub language: Option<String>,
    /// Max results to return (default: 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct CodeReadParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct EffectsOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CallersOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CalleesOfParams {
    /// Fully-qualified symbol name.
    pub qname: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerGetParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// Include entries that have been superseded (default: false).
    #[serde(default)]
    pub include_superseded: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerFindParams {
    /// Filter by ledger kind (decision, assumption, constraint, rationale, hazard, tradeoff).
    pub kind: Option<String>,
    /// Filter by tag (must be present on entry).
    pub tag: Option<String>,
    /// Filter by author id.
    pub author_id: Option<String>,
    /// Max results to return (default: 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct LedgerAppendParams {
    /// Fully-qualified symbol name this entry attaches to.
    pub qname: String,
    /// Ledger entry kind.
    pub kind: String,
    /// One-line summary.
    pub summary: String,
    /// Optional free-form body (markdown ok).
    pub body: Option<String>,
    /// Optional tags.
    pub tags: Option<Vec<String>>,
    /// Author kind: "agent" or "human" (default: "agent").
    #[serde(default = "default_author_kind")]
    pub author_kind: String,
    /// Author id (default: "asd-mcp").
    #[serde(default = "default_author_id")]
    pub author_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct EffectDeclareParams {
    /// Fully-qualified symbol name.
    pub qname: String,
    /// List of declared effects. Each element is a JSON object matching the
    /// `Effect` schema: `{ "effect": "<category>", "qualifiers": ..., "note": ... }`.
    pub declared: Vec<serde_json::Value>,
    /// Author id (default: "asd-mcp"). Surfaced to the policy gate so rules
    /// can scope by agent identity.
    #[serde(default = "default_author_id")]
    pub author_id: String,
}

fn default_limit() -> u32 {
    50
}
fn default_author_kind() -> String {
    "agent".to_string()
}
fn default_author_id() -> String {
    "asd-mcp".to_string()
}

// -- Tool implementations ---------------------------------------------------

#[tool_router]
impl AsdMcpServer {
    pub fn new(engine: Arc<Mutex<Engine>>, db_path: PathBuf) -> Self {
        Self {
            engine,
            db_path,
            tool_router: Self::tool_router(),
        }
    }

    // -- Read tools --

    #[tool(
        description = "Health check: reports MCP server status, ASG db path, and indexed symbol count."
    )]
    async fn health(&self) -> String {
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);
        let symbol_count = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.len(),
            _ => 0,
        };
        let db_path = self
            .db_path
            .canonicalize()
            .unwrap_or_else(|_| self.db_path.clone())
            .to_string_lossy()
            .to_string();
        let payload = serde_json::json!({
            "status": "ok",
            "db_path": db_path,
            "symbol_count": symbol_count,
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Query indexed symbols. Filters (all optional, AND-combined): name_contains, kind, language. Returns up to `limit` symbol summaries."
    )]
    async fn code_query(&self, params: Parameters<CodeQueryParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/index/by-qname", ASD_PATH_PREFIX);

        let qnames: Vec<String> = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            _ => return "[]".to_string(),
        };

        let index = AsgIndexStore { repo: &engine.repo };
        let mut symbols = Vec::new();
        let kind_filter = p.kind.as_deref().map(|k| k.to_lowercase());
        let name_filter = p.name_contains.as_deref();
        let lang_filter = p.language.as_deref();
        let limit = p.limit.max(1) as usize;

        for qname in qnames {
            if let Some(needle) = name_filter
                && !qname.contains(needle)
            {
                continue;
            }
            let sym = match index.get_symbol_by_qname(&ref_name, &qname) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            if let Some(lang) = lang_filter
                && sym.language != lang
            {
                continue;
            }
            if let Some(ref k) = kind_filter {
                let sym_kind = match sym.kind {
                    agentstatedeveloper_core::SymbolKind::Module => "module",
                    agentstatedeveloper_core::SymbolKind::Function => "function",
                    agentstatedeveloper_core::SymbolKind::Method => "method",
                    agentstatedeveloper_core::SymbolKind::Class => "class",
                    agentstatedeveloper_core::SymbolKind::Variable => "variable",
                };
                if sym_kind != k {
                    continue;
                }
            }
            symbols.push(sym);
            if symbols.len() >= limit {
                break;
            }
        }

        symbols.sort_by(|a, b| a.qname.cmp(&b.qname));
        serde_json::to_string(&symbols).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Read a symbol by qname. Returns { symbol, effects, ledger } — full context needed to reason about the code unit."
    )]
    async fn code_read(&self, params: Parameters<CodeReadParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore { repo: &engine.repo };
        let effects = match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let ledger = match ledger_store.list_entries(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let payload = serde_json::json!({
            "symbol": symbol,
            "effects": effects,
            "ledger": ledger,
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Return declared + transitive effects for a symbol (resolved via qname)."
    )]
    async fn effects_of(&self, params: Parameters<EffectsOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore { repo: &engine.repo };
        match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(Some(decl)) => {
                serde_json::to_string(&decl).unwrap_or_else(|_| "null".to_string())
            }
            Ok(None) => "null".to_string(),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(
        description = "List symbols that call the given symbol (inbound call edges, intra-module)."
    )]
    async fn callers_of(&self, params: Parameters<CallersOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore { repo: &engine.repo };
        let target = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ids = match index.get_callers(&ref_name, &target.symbol_id) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        let syms = match resolve_symbols_by_ids(&engine, &ids) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        serde_json::to_string(&syms).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List symbols called by the given symbol (outbound call edges, intra-module)."
    )]
    async fn callees_of(&self, params: Parameters<CalleesOfParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let index = AsgIndexStore { repo: &engine.repo };
        let target = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };
        let ids = match index.get_callees(&ref_name, &target.symbol_id) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        let syms = match resolve_symbols_by_ids(&engine, &ids) {
            Ok(v) => v,
            Err(e) => return err_json(&e.to_string()),
        };
        serde_json::to_string(&syms).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List ledger entries for a symbol, newest first. By default, entries superseded by later entries are omitted; set include_superseded=true to include them."
    )]
    async fn ledger_get(&self, params: Parameters<LedgerGetParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        let entries = match ledger_store.list_entries(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        let filtered: Vec<&LedgerEntry> = if p.include_superseded {
            entries.iter().collect()
        } else {
            // Collect all superseded ids, then exclude entries whose id appears in any supersedes list.
            let mut superseded: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for e in &entries {
                for sid in &e.supersedes {
                    superseded.insert(sid.clone());
                }
            }
            entries
                .iter()
                .filter(|e| !superseded.contains(&e.entry_id))
                .collect()
        };

        serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Search ledger entries across all symbols. Filters (all optional): kind, tag, author_id. O(n) scan — v1 simplicity."
    )]
    async fn ledger_find(&self, params: Parameters<LedgerFindParams>) -> String {
        let p = params.0;
        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();
        let prefix = format!("{}/ledger", ASD_PATH_PREFIX);
        let limit = p.limit.max(1) as usize;

        let kind_filter = match p.kind.as_deref() {
            Some(s) => match parse_ledger_kind(s) {
                Ok(k) => Some(k),
                Err(e) => return err_json(&e),
            },
            None => None,
        };

        let tree = match engine.repo.get_tree(&ref_name, &prefix) {
            Ok(v) => v,
            Err(_) => return "[]".to_string(),
        };

        let mut matches: Vec<LedgerEntry> = Vec::new();
        if let serde_json::Value::Object(by_symbol) = tree {
            for (_sym_id, per_symbol) in by_symbol {
                let entries_map = match per_symbol {
                    serde_json::Value::Object(m) => m,
                    _ => continue,
                };
                for (_entry_id, v) in entries_map {
                    if let Ok(entry) = serde_json::from_value::<LedgerEntry>(v) {
                        if let Some(k) = kind_filter
                            && entry.kind != k
                        {
                            continue;
                        }
                        if let Some(ref t) = p.tag
                            && !entry.tags.iter().any(|x| x == t)
                        {
                            continue;
                        }
                        if let Some(ref a) = p.author_id
                            && &entry.author.id != a
                        {
                            continue;
                        }
                        matches.push(entry);
                    }
                }
            }
        }

        matches.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        matches.truncate(limit);
        serde_json::to_string(&matches).unwrap_or_else(|_| "[]".to_string())
    }

    // -- Write tools --

    #[tool(
        description = "Append a ledger entry to a symbol (resolved via qname). Routes through the configured policy gate — may deny, allow, or flag the entry as awaiting-approval. Returns { entry_id, matched_policy, status }."
    )]
    async fn ledger_append(&self, params: Parameters<LedgerAppendParams>) -> String {
        let p = params.0;
        let kind = match parse_ledger_kind(&p.kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };
        let author_kind = match parse_author_kind(&p.author_kind) {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };

        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        // Evaluate policy before doing any write.
        let action = actions::ledger_append_action(kind.as_str());
        let situation = Situation {
            description: format!("ledger.append for {}", p.qname),
            qualifiers: serde_json::json!({
                "qname": &p.qname,
                "kind": kind.as_str(),
            }),
        };
        let decision = match engine.policy.evaluate(&situation, &action, &p.author_id) {
            Ok(d) => d,
            Err(e) => return err_json(&format!("policy evaluation failed: {}", e)),
        };

        if let Decision::Deny {
            matched_policy,
            reason,
        } = &decision
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }

        let author = Author {
            kind: author_kind,
            id: p.author_id.clone(),
        };
        let mut entry = LedgerEntry::new(symbol.symbol_id.clone(), kind, p.summary, author);
        entry.body = p.body;
        if let Some(tags) = p.tags {
            entry.tags = tags;
        }
        entry.matched_policy = decision.matched_policy();

        // RequireApproval: tag the entry so downstream reviewers see it.
        if let Decision::RequireApproval {
            approvers, reason, ..
        } = &decision
        {
            entry.tags.push("awaiting-approval".to_string());
            for a in approvers {
                entry.tags.push(format!("approver:{}", a));
            }
            if let Some(r) = reason {
                if entry.body.is_none() {
                    entry.body = Some(format!("Approval reason: {}", r));
                }
            }
        }

        let ledger_store = AsgLedgerStore { repo: &engine.repo };
        if let Err(e) = ledger_store.append_entry(&ref_name, &entry, &p.author_id) {
            return err_json(&e.to_string());
        }

        let status = match &decision {
            Decision::Allow { .. } => "allowed",
            Decision::RequireApproval { .. } => "awaiting-approval",
            Decision::Deny { .. } => "denied",
            Decision::NoPolicyMatch => "no-policy-match",
        };

        serde_json::to_string(&serde_json::json!({
            "entry_id": entry.entry_id,
            "symbol_id": symbol.symbol_id,
            "matched_policy": entry.matched_policy,
            "status": status,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Overwrite the `declared` effects list for a symbol. Routes through the configured policy gate. Uses `asd.effect.declare.broadens` as the action when the new list introduces effect categories not already present; otherwise `asd.effect.declare`. Returns the updated EffectDecl plus a `status` string."
    )]
    async fn effect_declare(&self, params: Parameters<EffectDeclareParams>) -> String {
        let p = params.0;

        // Deserialize each declared element into an Effect.
        let mut declared: Vec<Effect> = Vec::with_capacity(p.declared.len());
        for (i, v) in p.declared.into_iter().enumerate() {
            match serde_json::from_value::<Effect>(v) {
                Ok(e) => declared.push(e),
                Err(e) => return err_json(&format!("declared[{}]: {}", i, e)),
            }
        }

        let engine = self.engine.lock().await;
        let ref_name = engine.ref_name.clone();

        let index = AsgIndexStore { repo: &engine.repo };
        let symbol = match index.get_symbol_by_qname(&ref_name, &p.qname) {
            Ok(Some(s)) => s,
            Ok(None) => return err_json(&format!("symbol not found: {}", p.qname)),
            Err(e) => return err_json(&e.to_string()),
        };

        let effects_store = AsgEffectStore { repo: &engine.repo };
        let existing = match effects_store.get_effects(&ref_name, &symbol.symbol_id) {
            Ok(e) => e,
            Err(e) => return err_json(&e.to_string()),
        };

        // Broadening check: if any new effect category is not already present
        // in the existing declared list, this call is broadening.
        let existing_set: std::collections::HashSet<EffectCategory> = existing
            .as_ref()
            .map(|d| d.declared.iter().map(|e| e.effect).collect())
            .unwrap_or_default();
        let new_categories: Vec<String> =
            declared.iter().map(|e| e.effect.as_str().to_string()).collect();
        let broadens = declared.iter().any(|e| !existing_set.contains(&e.effect));
        let action = if broadens {
            actions::EFFECT_DECLARE_BROADENS
        } else {
            actions::EFFECT_DECLARE
        };

        let situation = Situation {
            description: format!("effect.declare for {}", p.qname),
            qualifiers: serde_json::json!({
                "qname": &p.qname,
                "declared": new_categories,
                "broadens": broadens,
            }),
        };
        let decision = match engine.policy.evaluate(&situation, action, &p.author_id) {
            Ok(d) => d,
            Err(e) => return err_json(&format!("policy evaluation failed: {}", e)),
        };

        if let Decision::Deny {
            matched_policy,
            reason,
        } = &decision
        {
            return err_json(&format!(
                "policy denied: {} (matched {})",
                reason, matched_policy
            ));
        }

        let matched_policy = decision.matched_policy();

        let updated = EffectDecl {
            symbol_id: symbol.symbol_id.clone(),
            declared,
            transitive: existing.as_ref().map(|d| d.transitive.clone()).unwrap_or_default(),
            verification: existing.as_ref().and_then(|d| d.verification.clone()),
            confidence: existing.as_ref().and_then(|d| d.confidence),
            matched_policy: matched_policy.clone(),
        };

        if let Err(e) =
            effects_store.put_effects(&ref_name, &symbol.symbol_id, &updated, &p.author_id)
        {
            return err_json(&e.to_string());
        }

        let status = match &decision {
            Decision::Allow { .. } => "allowed",
            Decision::RequireApproval { .. } => "awaiting-approval",
            Decision::Deny { .. } => "denied",
            Decision::NoPolicyMatch => "no-policy-match",
        };

        serde_json::to_string(&serde_json::json!({
            "effect_decl": updated,
            "matched_policy": matched_policy,
            "status": status,
            "action": action,
        }))
        .unwrap_or_else(|_| "{}".to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AsdMcpServer {}

// -- Helpers ----------------------------------------------------------------

fn err_json(msg: &str) -> String {
    serde_json::to_string(&serde_json::json!({ "error": msg }))
        .unwrap_or_else(|_| "{\"error\":\"unknown\"}".to_string())
}

fn parse_ledger_kind(s: &str) -> Result<LedgerKind, String> {
    match s.to_lowercase().as_str() {
        "decision" => Ok(LedgerKind::Decision),
        "assumption" => Ok(LedgerKind::Assumption),
        "constraint" => Ok(LedgerKind::Constraint),
        "rationale" => Ok(LedgerKind::Rationale),
        "hazard" => Ok(LedgerKind::Hazard),
        "tradeoff" => Ok(LedgerKind::Tradeoff),
        other => Err(format!("unknown ledger kind: {}", other)),
    }
}

fn parse_author_kind(s: &str) -> Result<AuthorKind, String> {
    match s.to_lowercase().as_str() {
        "agent" => Ok(AuthorKind::Agent),
        "human" => Ok(AuthorKind::Human),
        other => Err(format!("unknown author kind: {}", other)),
    }
}

/// Resolve symbol_ids to full Symbol records by scanning the qname index.
fn resolve_symbols_by_ids(
    engine: &Engine,
    ids: &[String],
) -> agentstatedeveloper_core::Result<Vec<agentstatedeveloper_core::Symbol>> {
    let ref_name = &engine.ref_name;
    let prefix = format!("{}/index/by-qname", agentstatedeveloper_core::ASD_PATH_PREFIX);
    let tree = match engine.repo.get_tree(ref_name, &prefix) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    let qnames: Vec<String> = match tree {
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        _ => return Ok(Vec::new()),
    };
    let index = AsgIndexStore { repo: &engine.repo };
    let id_set: std::collections::HashSet<&String> = ids.iter().collect();
    let mut out = Vec::new();
    for qn in qnames {
        if let Some(sym) = index.get_symbol_by_qname(ref_name, &qn)? {
            if id_set.contains(&sym.symbol_id) {
                out.push(sym);
            }
        }
    }
    out.sort_by(|a, b| a.qname.cmp(&b.qname));
    Ok(out)
}
