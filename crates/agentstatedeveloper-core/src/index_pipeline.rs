//! Shared indexing pipeline used by both the CLI `asd index` command and the
//! MCP `reindex` tool.
//!
//! # Performance design
//!
//! ## Old approach (O(N²) storage)
//! Using `spec_set_json` per symbol caused structural sharing to work against
//! us: every write rebuilt the growing `by-qname` Map node (1 entry, 2
//! entries, … N entries). 13 000 symbols × 3 paths = 39 000 growing copies
//! → 59 GB DB for a 1 341-file project.
//!
//! ## New approach (O(N) storage)
//! Each pass assembles the **complete** subtree JSON in memory, then writes
//! it with a **single** `spec_set_json` call per prefix. `json_to_tree`
//! creates the Map node exactly once with all N entries.
//!
//!   1. Pass 1 — symbols + effect declarations
//!      • `/asd/v1/index/by-qname`  (merged with existing)
//!      • `/asd/v1/effects`          (merged with existing)
//!      • `/asd/v1/code`             (merged with existing)
//!   2. Pass 2 — callee / caller edge lists
//!      • `/asd/v1/index/callees`
//!      • `/asd/v1/index/callers`
//!   3. Transitive — updated EffectDecl.transitive fields
//!      • `/asd/v1/effects`          (merged with Pass-1 state)
//!
//! Total object count is O(N) regardless of repo size.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentstategraph::CommitOptions;
use agentstategraph_core::{IntentCategory, TAG_GIT_REVISION};
use chrono::Utc;
use serde_json::Value;

use crate::adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
use crate::audit::{AuditEvent, AuditSink, event_types};
use crate::doc_adapters::{adapt_document, is_doc_file};
use crate::error::{AsdError, Result};
use crate::paths;
use crate::schema::{EffectDecl, Position, Symbol, TransitiveEffect};
use crate::search_fts::{SearchDocsDb, SearchFtsDb};
use crate::symbol::{canonical_symbol_id, symbol_fingerprint};

use agentstategraph::Repository;

/// Summary returned by [`run_index`].
#[derive(Debug, Clone, Default)]
pub struct IndexSummary {
    pub files: usize,
    pub skipped: usize,
    pub symbols: usize,
    pub effects: usize,
    pub edges: usize,
    pub intra_module_edges: usize,
    pub cross_module_edges: usize,
    pub transitive_updates: usize,
    pub orphaned_tagged: usize,
    /// Ledger entries whose orphan tags were cleared because their symbol is
    /// back in the index.
    pub orphaned_untagged: usize,
    /// Number of symbols that received a :line suffix to resolve a same-file
    /// qname collision.  0 means the index is collision-free.
    pub disambiguated: usize,
    /// Top cross-file qname collisions: (qname, first_file, second_file).
    /// Only populated when collisions occur; capped at 10 for display.
    pub top_collisions: Vec<(String, String, String)>,
    /// Number of document files processed by document adapters.
    pub doc_files: usize,
    /// Total document chunks indexed into asd_search_docs.
    pub docs_indexed: usize,
    /// Plan L t-005: dynamic-dispatch sites detected by adapters
    /// (`getattr(obj, x)(…)`, `__getattr__`, etc.). These are call
    /// patterns the static walker can't resolve into edges; surfaced
    /// so agents/humans know the missing edges are by design.
    pub dynamic_dispatch_sites: usize,
    /// Top dynamic-dispatch hits, capped at 5 for display.
    pub dynamic_dispatch_samples: Vec<crate::adapter::DynamicDispatchHint>,
    /// Plan L t-006: call sites the static resolver couldn't bind to
    /// a workspace qname. Includes stdlib (minus a known allowlist),
    /// third-party, and dynamic — treat as a "static-resolution gap"
    /// signal, not a bug count.
    /// t-002: cross-service endpoints (HTTP routes/clients, pub-sub) detected
    /// and written to the endpoint registry this run.
    pub service_endpoints: usize,
    /// t-002 slice 4: intra-process data-flow edges (arg→param) written this run.
    pub dataflow_edges: usize,
    pub dropped_call_edges: usize,
    /// Top unresolved calls, capped at 5 for display.
    pub sample_unresolved: Vec<crate::adapter::UnresolvedCall>,
    /// Plan T: whether ALL SQLite cache-sync steps (FTS rebuild, symbol
    /// cache, call-edge cache) succeeded this run. `false` means at least
    /// one step failed (see `cache_sync_warning`) and reads will fall back
    /// to the slow git path until the open-time self-heal or the next
    /// successful `asd index` repairs the cache. Always `false` when the
    /// pipeline ran without a SQLite `db_path` (no caches to sync).
    pub caches_synced: bool,
    /// Human-readable reason(s) when `caches_synced` is false and a sync
    /// step actually failed. `None` when everything succeeded or when no
    /// `db_path` was supplied.
    pub cache_sync_warning: Option<String>,
    /// Index entries for symbols this run found gone and removed (their
    /// effects and code entries with them). See [`crate::stale`].
    pub stale_pruned: usize,
    /// Stale entries whose symbol had only moved — same file, kind, base
    /// name and body under a new id — and were folded into it.
    pub stale_rebound: usize,
    /// Ledger entries those moves carried to the new symbol.
    pub ledger_entries_rebound: usize,
    /// Stale entries kept because ledger entries or runtime evidence still
    /// hang off them.
    pub stale_kept: usize,
    /// Effects records for symbols nothing in the index refers to any more,
    /// dropped by a run over the whole project. Records holding runtime or
    /// trace evidence stay.
    pub orphaned_effects_pruned: usize,
    /// `/asd/v1/code` entries no indexed symbol points at — mostly the old
    /// body fingerprint left behind by an edit — in files this run covered.
    pub code_entries_pruned: usize,
}

/// Build the `symbol_id → (ledger_text, ledger_flags)` map used to
/// denormalize ledger summaries into FTS rows. One `get_tree` call, no
/// per-symbol git reads. Shared by the index pipeline and
/// `Engine::warm_caches` (which rebuilds an empty FTS table from git).
pub(crate) fn build_ledger_fts_data(
    repo: &Repository,
    ref_name: &str,
) -> HashMap<String, (String, String)> {
    use crate::schema::{LedgerEntry, LedgerKind};
    let ledger_prefix = format!("{}/ledger", crate::paths::ASD_ROOT);
    match repo.get_tree(ref_name, &ledger_prefix) {
        Ok(serde_json::Value::Object(by_symbol)) => {
            let mut map = HashMap::with_capacity(by_symbol.len());
            for (sym_id, per_symbol) in by_symbol {
                if let serde_json::Value::Object(entries_map) = per_symbol {
                    let mut texts: Vec<String> = Vec::new();
                    let mut flags: std::collections::BTreeSet<&'static str> =
                        std::collections::BTreeSet::new();
                    for (_entry_id, v) in entries_map {
                        if let Ok(entry) = serde_json::from_value::<LedgerEntry>(v) {
                            if !entry.summary.is_empty() {
                                texts.push(entry.summary.to_lowercase());
                            }
                            match entry.kind {
                                LedgerKind::Ownership => {
                                    flags.insert("ownership");
                                }
                                LedgerKind::Invariant => {
                                    flags.insert("invariant");
                                }
                                LedgerKind::Hazard => {
                                    flags.insert("hazard");
                                }
                                LedgerKind::Decision => {
                                    flags.insert("decision");
                                }
                                _ => {}
                            }
                        }
                    }
                    if !texts.is_empty() || !flags.is_empty() {
                        map.insert(
                            sym_id,
                            (
                                texts.join(" "),
                                flags.into_iter().collect::<Vec<_>>().join(","),
                            ),
                        );
                    }
                }
            }
            map
        }
        _ => HashMap::new(),
    }
}

/// Result of collecting source files under a path.
pub struct CollectResult {
    pub recognized: Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    pub skipped: Vec<PathBuf>,
}

/// Run the full index pipeline over `path`.
///
/// All writes are batched into three commits (symbols, edges, transitive)
/// regardless of repo size, with O(N) total object cost.
///
/// `progress` is called before each file: `(file, index, total)`.
/// `on_phase` is called when post-processing phases begin, with a short
/// human-readable description (e.g. `"building call graph…"`).
/// Pass `None` for either to suppress that output.
pub fn run_index(
    repo: &Repository,
    ref_name: &str,
    path: &Path,
    agent_id: &str,
    adapters: &[Arc<dyn LanguageAdapter>],
    audit: Option<&dyn AuditSink>,
    progress: Option<&dyn Fn(&Path, usize, usize)>,
    on_phase: Option<&dyn Fn(&str)>,
    db_path: Option<&Path>,
) -> Result<IndexSummary> {
    let collected = collect_source_files(path, adapters)?;
    let files = collected.recognized;
    let skipped_files = collected.skipped;
    let index_root = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };

    // t-002: this repo's stable id for cross-service endpoints — an explicit
    // ASD_REPO_ID override, else the normalized git origin URL, else dir name.
    let repo_id = crate::cross_service::resolve_repo_id(
        std::env::var("ASD_REPO_ID").ok().as_deref(),
        &index_root,
    );

    // -----------------------------------------------------------------------
    // Pass 1: parse symbols + effects, assemble complete subtrees in memory,
    // write as a single spec per prefix → O(N) objects, 1 commit.
    // -----------------------------------------------------------------------
    let total = files.len();

    // Existing symbols, so this run's calls resolve to symbols indexed by
    // earlier runs. Read for resolution only — see `run_qnames` below.
    let mut by_qname: serde_json::Map<String, Value> = repo
        .get_tree(ref_name, "/asd/v1/index/by-qname")
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    // What THIS run produced, written at flush time on top of the subtrees as
    // the flush speculation forked them. Writing back a copy read here
    // instead reverted whatever changed while this run parsed: an
    // `effect_declare`, a runtime trace, another index.
    let mut run_qnames: Vec<(String, Value)> = Vec::new();
    // (symbol_id, inferred effects, file)
    let mut run_effects: Vec<(String, Vec<crate::schema::Effect>, String)> = Vec::new();
    // (language, "clean_file/symbol_fp", Symbol) — the `/asd/v1/code` tree.
    let mut run_code: Vec<(String, String, Value)> = Vec::new();
    // Every file this run parsed, as its symbols record it: what this run
    // may declare stale entries in (see `crate::stale`).
    let mut parsed_files: HashSet<String> = HashSet::new();

    let mut symbol_count = 0usize;
    let mut disambiguated_count = 0usize;
    let mut all_edges: Vec<CallEdge> = Vec::new();
    // Track first-seen file for each qname to report cross-file collisions.
    let mut qname_first_file: HashMap<String, String> = HashMap::new();
    let mut collision_log: Vec<(String, String, String)> = Vec::new();

    // Pre-populate qname_to_sym_id from previously-indexed symbols so that
    // cross-package call edges (caller in this run → callee from a prior run)
    // are preserved.  The parsing loop below will overwrite entries for any
    // symbol that is re-indexed in the current run.
    let mut qname_to_sym_id: HashMap<String, String> = by_qname
        .iter()
        .filter_map(|(qname, sym_val)| {
            sym_val
                .get("symbol_id")
                .and_then(|v| v.as_str())
                .map(|id| (qname.clone(), id.to_string()))
        })
        .collect();

    struct FileCtx {
        file_str: String,
        source: String,
        parsed: Vec<ParsedSymbol>,
        adapter: Arc<dyn LanguageAdapter>,
    }
    let mut file_ctxs: Vec<FileCtx> = Vec::with_capacity(files.len());
    let mut indexed_symbols: Vec<Symbol> = Vec::new();
    // Plan L t-005: aggregate dynamic-dispatch hints across all files.
    let mut all_dynamic_dispatch: Vec<crate::adapter::DynamicDispatchHint> = Vec::new();

    for (idx, (file, adapter)) in files.iter().enumerate() {
        if let Some(cb) = progress {
            cb(file, idx + 1, total);
        }
        let source = std::fs::read_to_string(file)
            .map_err(|e| AsdError::Other(format!("read {}: {}", file.display(), e)))?;
        let rel = file.strip_prefix(&index_root).unwrap_or(file);
        let file_str = rel.to_string_lossy().replace('\\', "/");
        parsed_files.insert(file_str.clone());

        let mut parsed = adapter.parse_symbols(&file_str, &source)?;
        disambiguated_count += disambiguate_qnames(&mut parsed);

        // Plan L t-005: detect dynamic-dispatch sites the call-graph
        // walker can't resolve. Default impl returns empty, so this
        // is a no-op for adapters without a story.
        all_dynamic_dispatch.extend(adapter.scan_dynamic_dispatch(&file_str, &source));

        for p in &parsed {
            let symbol_id = canonical_symbol_id(&p.qname, p.kind, &file_str);
            let symbol_fp = symbol_fingerprint(&p.body);
            let sym = Symbol {
                symbol_id: symbol_id.clone(),
                symbol_fp: symbol_fp.clone(),
                qname: p.qname.clone(),
                language: adapter.language().to_string(),
                kind: p.kind,
                file: file_str.clone(),
                start: Position {
                    line: p.start_line,
                    col: p.start_col,
                },
                end: Position {
                    line: p.end_line,
                    col: p.end_col,
                },
                signature: p.signature.clone(),
                doc: p.doc.clone(),
            };

            let sym_val = serde_json::to_value(&sym).map_err(|e| AsdError::Other(e.to_string()))?;

            // Accumulate into in-memory maps — no repo writes yet.
            // Detect collisions: same qname parsed from two different files.
            if let Some(prev_file) = qname_first_file.get(&p.qname) {
                if prev_file != &file_str {
                    collision_log.push((p.qname.clone(), prev_file.clone(), file_str.clone()));
                }
            } else {
                qname_first_file.insert(p.qname.clone(), file_str.clone());
            }
            by_qname.insert(p.qname.clone(), sym_val.clone());
            run_qnames.push((p.qname.clone(), sym_val.clone()));
            let mut inferred = adapter.infer_effects(&source, p);
            // Stamp provenance so a re-index can tell what it inferred from
            // what a person declared (`effect_declare`), and refresh only the
            // former.
            for effect in &mut inferred {
                effect
                    .adapter
                    .get_or_insert_with(|| adapter.language().to_string());
            }
            run_effects.push((symbol_id.clone(), inferred, file_str.clone()));

            let code_key = format!("{}/{}", paths::clean(&file_str), symbol_fp);
            run_code.push((sym.language.clone(), code_key, sym_val));

            qname_to_sym_id.insert(p.qname.clone(), symbol_id.clone());
            symbol_count += 1;
            indexed_symbols.push(sym);
        }

        file_ctxs.push(FileCtx {
            file_str,
            source,
            parsed,
            adapter: Arc::clone(adapter),
        });
    }

    if disambiguated_count > 0 || !collision_log.is_empty() {
        if disambiguated_count > 0 {
            eprintln!(
                "  note: disambiguated {} same-file qname collision(s) with :line suffix",
                disambiguated_count,
            );
        }
        for (qname, f1, f2) in collision_log.iter().take(5) {
            eprintln!("    cross-file collision: {qname:?}  {f1}  ↔  {f2}");
        }
        if collision_log.len() > 5 {
            eprintln!(
                "    … and {} more cross-file collisions",
                collision_log.len() - 5
            );
        }
    }

    if let Some(f) = on_phase {
        f(&format!(
            "  {} files parsed — committing symbols + effects…",
            symbol_count
        ));
    }

    // -----------------------------------------------------------------------
    // Build workspace-wide qname context for cross-module call resolution.
    //
    // Seed from the FULL by-qname map — which at this point contains both
    // previously-indexed symbols (seeded from the repo at the start of Pass 1)
    // AND the symbols parsed in this run.  This allows cross-package edges to
    // resolve: e.g., when indexing AcmeFlow, calls to DriftCompiler.compile
    // resolve because SequencerCore was indexed in a prior run and its symbols
    // are already in by_qname.
    //
    // Must happen BEFORE the Pass 1 commit because spec_set_json consumes
    // by_qname (moves it into a Value::Object).
    // -----------------------------------------------------------------------
    let mut workspace = WorkspaceSymbols::default();
    for (qname, sym_val) in &by_qname {
        workspace.qnames.insert(qname.clone());
        // Extract kind from the serialized Symbol JSON (e.g. "method", "class").
        if let Some(kind_str) = sym_val.get("kind").and_then(|v| v.as_str()) {
            if let Ok(kind) = serde_json::from_value::<crate::schema::SymbolKind>(
                serde_json::Value::String(kind_str.to_string()),
            ) {
                workspace.kinds.insert(qname.clone(), kind);
            }
        }
    }
    // Build suffix index after all qnames are inserted so adapters can do
    // O(1) suffix-based lookup (e.g., "DriftCompiler.compile" →
    // "Sources.Models.DriftCompiler.compile").
    workspace.build_suffix_index();

    // Populate the workspace property map from ALL files so that instance
    // property calls (e.g., `pool.resolve()` where `pool: DriftSynthPool` is
    // declared in a different file) can be resolved across file boundaries.
    // Each adapter contributes its language-specific property extraction.
    for ctx in &file_ctxs {
        let props = ctx.adapter.extract_property_types(&ctx.parsed);
        workspace.properties.extend(props);
    }

    // Flush Pass 1: 3 spec_set_json calls (complete subtrees) → O(N) objects.
    //
    // Each subtree is the one the speculation forked from plus this run's
    // keys, so the speculation changes exactly what this run produced, and
    // committing it — a three-way merge onto the head — keeps every write
    // that landed meanwhile.
    // Only a run over the store's whole project may read a missing file as
    // deleted: a partial run (`asd index src`) records paths relative to a
    // different root, so any other entry's file looks missing to it.
    let whole_project = db_path
        .and_then(Path::parent)
        .map(|d| {
            if d.as_os_str().is_empty() {
                Path::new(".")
            } else {
                d
            }
        })
        .and_then(|d| d.canonicalize().ok())
        .zip(index_root.canonicalize().ok())
        .is_some_and(|(store_dir, root)| store_dir == root);

    let (spec1, fork) = crate::subtree::speculate_at_head(repo, ref_name, "asd-index-pass1")?;
    let flushed = (|| -> Result<Pass1> {
        let mut qname_tree = crate::subtree::read_seed(repo, &fork, "/asd/v1/index/by-qname")?;
        let mut effects_tree = crate::subtree::read_seed(repo, &fork, "/asd/v1/effects")?;
        let mut code_tree = crate::subtree::read_seed(repo, &fork, "/asd/v1/code")?;

        let is_gone = |file: &str| {
            whole_project && !parsed_files.contains(file) && !index_root.join(file).exists()
        };
        let mut pass1 = Pass1::default();
        settle_stale(
            repo,
            spec1,
            &fork,
            StaleScope {
                produced_qnames: &run_qnames.iter().map(|(q, _)| q.as_str()).collect(),
                produced: &indexed_symbols,
                parsed_files: &parsed_files,
                is_gone: &is_gone,
            },
            &mut qname_tree,
            &mut effects_tree,
            &mut pass1,
        )?;

        qname_tree.extend(run_qnames);
        pass1.symbols = qname_tree.len();

        let now = Utc::now();
        for (symbol_id, inferred, file) in run_effects {
            let existing = effects_tree
                .get(&symbol_id)
                .and_then(|v| serde_json::from_value::<EffectDecl>(v.clone()).ok());
            let merged =
                crate::effects::merge_reindexed_effects(existing, &symbol_id, inferred, &file, now);
            effects_tree.insert(
                symbol_id,
                serde_json::to_value(&merged).map_err(|e| AsdError::Other(e.to_string()))?,
            );
        }

        for (lang, key, sym_val) in run_code {
            if let Value::Object(by_key) = code_tree
                .entry(lang)
                .or_insert_with(|| Value::Object(Default::default()))
            {
                by_key.insert(key, sym_val);
            }
        }

        prune_orphans(
            &qname_tree,
            &indexed_symbols,
            whole_project,
            &|file: &str| parsed_files.contains(file) || is_gone(file),
            &mut effects_tree,
            &mut code_tree,
            &mut pass1,
        );
        pass1.effects = effects_tree.len();

        repo.spec_set_json(spec1, "/asd/v1/index/by-qname", &Value::Object(qname_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
        repo.spec_set_json(spec1, "/asd/v1/effects", &Value::Object(effects_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
        if !code_tree.is_empty() || pass1.code_entries_pruned > 0 {
            repo.spec_set_json(spec1, "/asd/v1/code", &Value::Object(code_tree))
                .map_err(|e| AsdError::Other(e.to_string()))?;
        }
        Ok(pass1)
    })();
    let pass1 = match flushed {
        Ok(p) => p,
        Err(e) => {
            let _ = repo.discard_speculation(spec1);
            return Err(e);
        }
    };
    let unique_symbol_count = pass1.symbols;
    let unique_effect_count = pass1.effects;
    // A re-index is a routine checkpoint, so it deliberately does NOT carry
    // `TAG_PIN_STATE`: pinning here would retain a full state tree on every
    // run and leave the store with nothing reclaimable. What it records
    // instead is the revision it indexed, which is enough to rebuild this
    // state with `git checkout <sha> && asd index`.
    let opts1 = CommitOptions::new(
        agent_id,
        IntentCategory::Checkpoint,
        format!(
            "asd index: {} symbols across {} files ({} stale pruned, {} moved, {} orphaned effects and {} code entries dropped)",
            unique_symbol_count,
            files.len(),
            pass1.removed_ids.len() - pass1.rebound,
            pass1.rebound,
            pass1.orphaned_effects.len(),
            pass1.code_entries_pruned
        ),
    )
    .with_tags(
        git_head_sha(&index_root)
            .map(|sha| vec![format!("{TAG_GIT_REVISION}{sha}")])
            .unwrap_or_default(),
    );
    repo.commit_speculation(spec1, opts1)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    // -----------------------------------------------------------------------
    // Pass 2: extract call edges, resolve, write callees+callers as two
    // complete subtree writes → O(N) objects, 1 commit.
    // -----------------------------------------------------------------------
    // Rebuild all_symbol_ids from the winning qname→sym_id mapping so that
    // transitive propagation only processes symbol_ids that are actually
    // present in by_effects (avoids wasted DFS over orphaned IDs from qname
    // collisions where the loser's symbol_id was pushed but never "won" the
    // by_qname slot).
    let all_symbol_ids: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        qname_to_sym_id
            .values()
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect()
    };

    if let Some(f) = on_phase {
        f("  building call graph…");
    }
    // Plan L t-006: aggregate unresolved-call hints across all files.
    let mut all_unresolved: Vec<crate::adapter::UnresolvedCall> = Vec::new();
    // t-002: cross-service endpoints, enriched with repo + symbol identity.
    let mut all_endpoints: Vec<crate::cross_service::ServiceEndpoint> = Vec::new();
    // t-002 slice 4: data-flow edges. Resolve callee param names from signatures.
    let mut all_dataflow: Vec<crate::dataflow::DataFlowEdge> = Vec::new();
    let qname_params: HashMap<String, Vec<String>> = file_ctxs
        .iter()
        .flat_map(|ctx| ctx.parsed.iter())
        .filter_map(|p| {
            p.signature
                .as_ref()
                .map(|sig| (p.qname.clone(), crate::dataflow::parse_params(sig)))
        })
        .collect();
    for ctx in &file_ctxs {
        let edges =
            ctx.adapter
                .extract_call_edges(&ctx.file_str, &ctx.source, &ctx.parsed, &workspace);
        all_edges.extend(edges);
        // Per-file unresolved-call report. Default trait impl returns
        // empty for adapters that don't implement static resolution.
        all_unresolved.extend(ctx.adapter.report_unresolved_calls(
            &ctx.file_str,
            &ctx.source,
            &ctx.parsed,
            &workspace,
        ));
        // Cross-service endpoint detection. The adapter names each endpoint's
        // owner by qname; resolve it to a symbol_id and stamp this repo's id.
        for det in ctx
            .adapter
            .infer_service_endpoints(&ctx.file_str, &ctx.source, &ctx.parsed)
        {
            if let Some(sym_id) = qname_to_sym_id.get(&det.owner_qname) {
                all_endpoints.push(det.into_endpoint(&repo_id, sym_id));
            }
        }
        // Data-flow (arg→param). Resolve symbol identity + the callee's param
        // name from its signature; unresolved sites are dropped.
        for det in ctx
            .adapter
            .extract_dataflow(&ctx.file_str, &ctx.source, &ctx.parsed, &workspace)
        {
            if let Some(edge) = crate::dataflow::resolve_edge(
                &det,
                |q| qname_to_sym_id.get(q).cloned(),
                |q| qname_params.get(q).cloned(),
            ) {
                all_dataflow.push(edge);
            }
        }
    }

    // Plan Q t-004b: project-level endpoint prefix resolution. Give each
    // language one pass over all its files + the full endpoint set, so
    // cross-file router-mount prefix chains resolve to the full runtime path.
    {
        let mut by_lang: HashMap<&str, (Arc<dyn LanguageAdapter>, Vec<(String, String)>)> =
            HashMap::new();
        for ctx in &file_ctxs {
            let entry = by_lang
                .entry(ctx.adapter.language())
                .or_insert_with(|| (Arc::clone(&ctx.adapter), Vec::new()));
            entry.1.push((ctx.file_str.clone(), ctx.source.clone()));
        }
        for (_lang, (adapter, lang_files)) in by_lang {
            adapter.resolve_endpoint_prefixes(&lang_files, &mut all_endpoints);
        }
    }

    let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut callers_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_edge_count = 0usize;
    let mut cross_module_edges = 0usize;

    for edge in &all_edges {
        let Some(caller_sym) = qname_to_sym_id.get(&edge.caller_qname) else {
            continue;
        };
        let Some(callee_sym) = qname_to_sym_id.get(&edge.callee_qname) else {
            continue;
        };
        let cs = callees_of.entry(caller_sym.clone()).or_default();
        if !cs.contains(callee_sym) {
            cs.push(callee_sym.clone());
        }
        let rs = callers_of.entry(callee_sym.clone()).or_default();
        if !rs.contains(caller_sym) {
            rs.push(caller_sym.clone());
        }
        resolved_edge_count += 1;
        if !same_module(&edge.caller_qname, &edge.callee_qname) {
            cross_module_edges += 1;
        }
    }
    let intra_module_edges = resolved_edge_count.saturating_sub(cross_module_edges);

    for v in callees_of.values_mut() {
        v.sort();
    }
    for v in callers_of.values_mut() {
        v.sort();
    }

    // Assemble complete callees / callers subtrees in memory.
    let callees_tree: serde_json::Map<String, Value> = callees_of
        .iter()
        .map(|(sym_id, callees)| (sym_id.clone(), serde_json::json!({ "callees": callees })))
        .collect();
    let callers_tree: serde_json::Map<String, Value> = callers_of
        .iter()
        .map(|(sym_id, callers)| (sym_id.clone(), serde_json::json!({ "callers": callers })))
        .collect();

    let spec2 = repo
        .speculate(ref_name, Some("asd-index-pass2-edges".into()))
        .map_err(|e| AsdError::Other(e.to_string()))?;
    if !callees_tree.is_empty() {
        repo.spec_set_json(spec2, "/asd/v1/index/callees", &Value::Object(callees_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    if !callers_tree.is_empty() {
        repo.spec_set_json(spec2, "/asd/v1/index/callers", &Value::Object(callers_tree))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }

    // t-002: endpoint registry, nested contract_hash → repo_id → symbol_id →
    // ServiceEndpoint, so endpoints sharing a contract (from any repo, once
    // manifests are imported) collapse under one prefix for matching.
    let mut endpoints_tree: serde_json::Map<String, Value> = serde_json::Map::new();
    for ep in &all_endpoints {
        let ch = crate::cross_service::contract_hash(&ep.contract);
        let by_repo = endpoints_tree
            .entry(ch)
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .expect("contract bucket is an object");
        let by_sym = by_repo
            .entry(ep.repo_id.clone())
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .expect("repo bucket is an object");
        by_sym.insert(
            ep.symbol_id.clone(),
            serde_json::to_value(ep).unwrap_or(Value::Null),
        );
    }
    if !endpoints_tree.is_empty() {
        repo.spec_set_json(
            spec2,
            "/asd/v1/index/endpoints",
            &Value::Object(endpoints_tree),
        )
        .map_err(|e| AsdError::Other(e.to_string()))?;
    }

    // t-002 slice 4: data-flow registry, keyed by source symbol_id →
    // [DataFlowEdge].
    let mut dataflow_tree: serde_json::Map<String, Value> = serde_json::Map::new();
    for edge in &all_dataflow {
        dataflow_tree
            .entry(edge.from_symbol_id.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("dataflow bucket is an array")
            .push(serde_json::to_value(edge).unwrap_or(Value::Null));
    }
    if !dataflow_tree.is_empty() {
        repo.spec_set_json(
            spec2,
            "/asd/v1/index/dataflow",
            &Value::Object(dataflow_tree),
        )
        .map_err(|e| AsdError::Other(e.to_string()))?;
    }

    let opts2 = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("asd index: {} call edges", resolved_edge_count),
    );
    repo.commit_speculation(spec2, opts2)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    // -----------------------------------------------------------------------
    // Transitive effect propagation — fully in-memory, then one bulk write.
    // -----------------------------------------------------------------------
    if let Some(f) = on_phase {
        f(&format!(
            "  propagating transitive effects ({} edges)…",
            resolved_edge_count
        ));
    }
    let transitive_updates =
        propagate_transitive_batched(repo, ref_name, &all_symbol_ids, &callees_of, agent_id)?;

    let orphan_tags = crate::ledger::tag_orphaned_entries(repo, ref_name, agent_id)?;
    let orphaned_tagged = orphan_tags.tagged.len();
    let orphaned_untagged = orphan_tags.untagged.len();

    if let Some(sink) = audit {
        let event = AuditEvent::new(event_types::INDEX_RUN, agent_id, "agent", "allow")
            .with_payload(serde_json::json!({
                "path": path.to_string_lossy(),
                "files": files.len(),
                "symbols": unique_symbol_count,
                "effects": unique_effect_count,
                "edges": resolved_edge_count,
                "transitive_updates": transitive_updates,
                "orphaned_tagged": orphaned_tagged,
                "orphaned_untagged": orphaned_untagged,
            }));
        let _ = sink.emit(&event);
    }

    // FTS5 atomic rebuild — replace the entire index from the current snapshot.
    // Full rebuild avoids stale rows from deleted or renamed files; the indexer
    // already owns the complete world, so incremental tracking adds no benefit.
    // Errors are non-fatal; search falls back to in-memory until next index.
    //
    // Deduplicate by qname before rebuilding so FTS row count matches the ASG
    // by-qname tree count (by_qname silently overwrites duplicates; FTS must
    // mirror that behaviour so `asd status` and `asd list stats` agree).
    let mut caches_synced = false;
    let mut cache_sync_warnings: Vec<String> = Vec::new();
    if let Some(db) = db_path {
        if let Some(f) = on_phase {
            f("  rebuilding FTS search index…");
        }

        // M59: build ledger_data map (symbol_id → (ledger_text, ledger_flags))
        // from the ledger tree so FTS rows carry denormalized summaries.
        // One get_tree call, no per-symbol git reads needed later.
        let ledger_data: HashMap<String, (String, String)> = build_ledger_fts_data(repo, ref_name);

        let fts_ok = match SearchFtsDb::open(db) {
            Ok(fts) => {
                // Stale symbols left the store this run: their effects rows
                // leave the cache, and ledger entries that moved follow
                // their symbol there too (the cache answers `list_entries`),
                // as do orphan tags added or cleared.
                let gone: Vec<String> = pass1
                    .removed_ids
                    .iter()
                    .chain(&pass1.orphaned_effects)
                    .cloned()
                    .collect();
                if let Err(e) = fts.delete_effects_for(&gone, ref_name) {
                    cache_sync_warnings.push(format!("effects cache prune failed: {e}"));
                }
                for entry in &pass1.rebound_entries {
                    if let Err(e) = fts.upsert_ledger_entry(entry, ref_name) {
                        cache_sync_warnings.push(format!("ledger cache rebind failed: {e}"));
                        break;
                    }
                }
                for entry in orphan_tags.tagged.iter().chain(&orphan_tags.untagged) {
                    if let Err(e) = fts.upsert_ledger_entry(entry, ref_name) {
                        cache_sync_warnings.push(format!("ledger cache orphan tag failed: {e}"));
                        break;
                    }
                }
                // Keep last-seen symbol per qname, matching by_qname semantics.
                let mut seen: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for (i, sym) in indexed_symbols.iter().enumerate() {
                    seen.insert(sym.qname.as_str(), i);
                }
                let mut deduped: Vec<&Symbol> =
                    seen.values().map(|&i| &indexed_symbols[i]).collect();
                deduped.sort_by(|a, b| a.qname.cmp(&b.qname));
                if let Err(e) = fts.rebuild_refs(&deduped, &ledger_data) {
                    eprintln!("asd: FTS rebuild warning: {e}");
                    cache_sync_warnings.push(format!("FTS rebuild failed: {e}"));
                    false
                } else {
                    true
                }
            }
            Err(e) => {
                eprintln!("asd: FTS index unavailable (non-fatal): {e}");
                cache_sync_warnings.push(format!("FTS index unavailable: {e}"));
                false
            }
        };

        // Record the FTS rebuild outcome so `asd status` can distinguish
        // "symbols fresh / FTS stale" from a fully-fresh index. Best-effort:
        // a second open may also fail if the DB is still locked, in which case
        // stale_warning() falls back to the previous behaviour (old timestamp).
        if let Ok(meta) = SearchFtsDb::open(db) {
            let _ = meta.mark_symbols_indexed(fts_ok);
        }

        // Populate the symbol and call-edge SQLite caches so subsequent
        // `callers`, `callees`, `context-for`, and `investigate` calls can
        // skip the full git tree walk entirely.  Non-fatal: a cache miss just
        // falls back to the authoritative git path.
        match SearchFtsDb::open(db) {
            Ok(cache) => {
                if let Some(f) = on_phase {
                    f("  caching symbols and edges…");
                }
                let sym_refs: Vec<&Symbol> = indexed_symbols.iter().collect();
                let mut sym_ok = true;
                let mut edges_ok = true;
                if let Err(e) = cache.sync_symbols(&sym_refs, ref_name) {
                    eprintln!("asd: symbol cache sync warning: {e}");
                    cache_sync_warnings.push(format!("symbol cache sync failed: {e}"));
                    sym_ok = false;
                }
                if let Err(e) = cache.sync_call_edges(&callees_of, &callers_of, ref_name) {
                    eprintln!("asd: edge cache sync warning: {e}");
                    cache_sync_warnings.push(format!("edge cache sync failed: {e}"));
                    edges_ok = false;
                }
                caches_synced = fts_ok && sym_ok && edges_ok;
            }
            Err(e) => {
                eprintln!("asd: symbol/edge cache unavailable (non-fatal): {e}");
                cache_sync_warnings.push(format!("symbol/edge cache unavailable: {e}"));
            }
        }
    }

    // Document search index — walk the index root for doc-adapter files and
    // rebuild asd_search_docs in one atomic pass (full replace, like symbol FTS).
    let mut doc_files_count = 0usize;
    let mut docs_indexed_count = 0usize;
    if let Some(db) = db_path {
        if let Some(f) = on_phase {
            f("  rebuilding document search index…");
        }
        let mut all_docs = Vec::new();
        collect_doc_files_recursive(&index_root, &mut all_docs, &mut doc_files_count);
        docs_indexed_count = all_docs.len();
        match SearchDocsDb::open(db) {
            Ok(docs_db) => {
                if let Err(e) = docs_db.rebuild(&all_docs) {
                    eprintln!("asd: document index rebuild warning: {e}");
                }
            }
            Err(e) => {
                eprintln!("asd: document index unavailable (non-fatal): {e}");
            }
        }
    }

    let top_collisions = collision_log.into_iter().take(10).collect();
    Ok(IndexSummary {
        files: files.len(),
        skipped: skipped_files.len(),
        symbols: unique_symbol_count,
        effects: unique_effect_count,
        edges: resolved_edge_count,
        intra_module_edges,
        cross_module_edges,
        transitive_updates,
        orphaned_tagged,
        orphaned_untagged,
        disambiguated: disambiguated_count,
        top_collisions,
        doc_files: doc_files_count,
        docs_indexed: docs_indexed_count,
        dynamic_dispatch_sites: all_dynamic_dispatch.len(),
        dynamic_dispatch_samples: {
            let mut v = all_dynamic_dispatch;
            v.truncate(5);
            v
        },
        service_endpoints: all_endpoints.len(),
        dataflow_edges: all_dataflow.len(),
        dropped_call_edges: all_unresolved.len(),
        sample_unresolved: {
            let mut v = all_unresolved;
            v.truncate(5);
            v
        },
        caches_synced,
        cache_sync_warning: if cache_sync_warnings.is_empty() {
            None
        } else {
            Some(cache_sync_warnings.join("; "))
        },
        stale_pruned: pass1.removed_ids.len() - pass1.rebound,
        stale_rebound: pass1.rebound,
        ledger_entries_rebound: pass1.rebound_entries.len(),
        stale_kept: pass1.kept,
        orphaned_effects_pruned: pass1.orphaned_effects.len(),
        code_entries_pruned: pass1.code_entries_pruned,
    })
}

/// Walk a directory recursively, collect doc chunks from all recognised document files.
/// Skips hidden dirs, .git, target/, node_modules/, and binary-looking files.
fn collect_doc_files_recursive(
    root: &Path,
    out: &mut Vec<crate::search_fts::SearchDoc>,
    file_count: &mut usize,
) {
    let skip_dirs = [
        "target",
        "node_modules",
        ".git",
        ".build",
        "DerivedData",
        "dist",
        ".cache",
    ];
    let dir = match std::fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return,
    };
    for entry in dir.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if skip_dirs.contains(&name.as_str()) {
                continue;
            }
            collect_doc_files_recursive(&path, out, file_count);
        } else if is_doc_file(&path) {
            *file_count += 1;
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Some(docs) = adapt_document(&path, &content) {
                    out.extend(docs);
                }
            }
        }
    }
}

/// Append `:line` to the qname of every symbol that collides within a single
/// file's parse output.  Only symbols that actually collide are touched —
/// unique qnames are left unchanged so existing ledger/call-graph data is
/// not invalidated.  Returns the number of symbols that were renamed.
fn disambiguate_qnames(parsed: &mut Vec<ParsedSymbol>) -> usize {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in parsed.iter() {
        *counts.entry(p.qname.clone()).or_insert(0) += 1;
    }
    let mut renamed = 0usize;
    for p in parsed.iter_mut() {
        if counts.get(&p.qname).copied().unwrap_or(0) > 1 {
            p.qname = format!("{}:{}", p.qname, p.start_line);
            renamed += 1;
        }
    }
    renamed
}

/// What pass 1 wrote, for the summary and the cache sync.
#[derive(Default)]
struct Pass1 {
    symbols: usize,
    effects: usize,
    /// Ids of stale symbols removed from the index — pruned or moved.
    removed_ids: Vec<String>,
    /// How many of `removed_ids` moved rather than went away.
    rebound: usize,
    /// Ledger entries carried to a moved symbol's new id.
    rebound_entries: Vec<crate::schema::LedgerEntry>,
    /// Stale entries kept for the knowledge still attached to them.
    kept: usize,
    /// Ids whose effects record went with no symbol left to describe.
    orphaned_effects: Vec<String>,
    code_entries_pruned: usize,
}

/// What one run covered, for deciding what of the stored index it can call
/// stale.
struct StaleScope<'a> {
    produced_qnames: &'a HashSet<&'a str>,
    produced: &'a [Symbol],
    parsed_files: &'a HashSet<String>,
    is_gone: &'a dyn Fn(&str) -> bool,
}

/// Remove the entries this run shows to be stale (see [`crate::stale`]) from
/// the subtrees pass 1 is about to write.
///
/// - A symbol that moved hands its ledger entries — and its effects record,
///   when the new id has none — to the new id, then leaves the index. It
///   moved when its body reappears under a new id in the same file (same
///   kind and base name), or when another file now produces its qname
///   ([`crate::stale::find_displaced`]).
/// - A symbol that is gone leaves the index with its effects record, unless
///   ledger entries or runtime evidence still hang off it: those stay, as
///   before, so nothing anyone recorded is dropped and its ledger keeps
///   exporting. Its code entries go in [`prune_orphans`].
///
/// The ledger and ledger-idx subtrees are written once, in the same
/// speculation, and only when an entry moved.
#[allow(clippy::too_many_arguments)]
fn settle_stale(
    repo: &Repository,
    spec: agentstategraph::SpecHandle,
    fork: &str,
    scope: StaleScope<'_>,
    qname_tree: &mut serde_json::Map<String, Value>,
    effects_tree: &mut serde_json::Map<String, Value>,
    pass1: &mut Pass1,
) -> Result<()> {
    let mut stale = crate::stale::find_stale(
        qname_tree,
        scope.produced_qnames,
        scope.parsed_files,
        scope.is_gone,
    );
    let mut moves = crate::stale::match_moves(&stale, scope.produced);
    for (st, new_id) in crate::stale::find_displaced(
        qname_tree,
        scope.produced,
        scope.parsed_files,
        scope.is_gone,
    ) {
        moves.insert(st.symbol.symbol_id.clone(), new_id);
        stale.push(st);
    }
    if stale.is_empty() {
        return Ok(());
    }
    let mut ledger = crate::subtree::read_seed(repo, fork, &paths::ledger_root())?;
    let mut ledger_index: Option<serde_json::Map<String, Value>> = None;

    for st in &stale {
        let old_id = &st.symbol.symbol_id;
        if let Some(new_id) = moves.get(old_id) {
            if let Some(Value::Object(entries)) = ledger.remove(old_id) {
                if ledger_index.is_none() {
                    ledger_index = Some(crate::subtree::read_seed(
                        repo,
                        fork,
                        &paths::ledger_index_root(),
                    )?);
                }
                let index = ledger_index.as_mut().expect("just read");
                let Value::Object(dest) = ledger
                    .entry(new_id.clone())
                    .or_insert_with(|| Value::Object(Default::default()))
                else {
                    return Err(AsdError::Other(format!(
                        "ledger node for {new_id} is not a map"
                    )));
                };
                for (entry_id, mut value) in entries {
                    if let Value::Object(fields) = &mut value {
                        fields.insert("symbol_id".into(), Value::String(new_id.clone()));
                    }
                    if let Ok(entry) = serde_json::from_value(value.clone()) {
                        pass1.rebound_entries.push(entry);
                    }
                    index.insert(entry_id.clone(), Value::String(new_id.clone()));
                    dest.insert(entry_id, value);
                }
            }
            if let Some(mut decl) = effects_tree.remove(old_id)
                && !effects_tree.contains_key(new_id)
            {
                if let Value::Object(fields) = &mut decl {
                    fields.insert("symbol_id".into(), Value::String(new_id.clone()));
                }
                effects_tree.insert(new_id.clone(), decl);
            }
            pass1.rebound += 1;
        } else {
            let has_ledger = ledger
                .get(old_id)
                .and_then(Value::as_object)
                .is_some_and(|entries| !entries.is_empty());
            let has_evidence = effects_tree
                .get(old_id)
                .is_some_and(crate::effects::value_carries_evidence);
            if has_ledger || has_evidence {
                pass1.kept += 1;
                continue;
            }
            effects_tree.remove(old_id);
        }
        pass1.removed_ids.push(old_id.clone());
        qname_tree.remove(&st.key);
    }

    if let Some(index) = ledger_index {
        repo.spec_set_json(spec, &paths::ledger_root(), &Value::Object(ledger))
            .map_err(|e| AsdError::Other(e.to_string()))?;
        repo.spec_set_json(spec, &paths::ledger_index_root(), &Value::Object(index))
            .map_err(|e| AsdError::Other(e.to_string()))?;
    }
    Ok(())
}

/// Drop what no indexed symbol refers to any more, once this run's symbols
/// are merged in:
///
/// - effects records whose id is neither in the index nor produced by this
///   run (a symbol losing a cross-file qname collision still keeps its
///   record), unless they hold runtime or trace evidence. Only a run over the
///   whole project can tell, since a partial run records its symbols under
///   different paths and so different ids.
/// - code entries in files `in_scope` covers that no indexed or produced
///   symbol points at — chiefly the old fingerprint an edited body leaves.
fn prune_orphans(
    qname_tree: &serde_json::Map<String, Value>,
    produced: &[Symbol],
    whole_project: bool,
    in_scope: &dyn Fn(&str) -> bool,
    effects_tree: &mut serde_json::Map<String, Value>,
    code_tree: &mut serde_json::Map<String, Value>,
    pass1: &mut Pass1,
) {
    let indexed: Vec<Symbol> = qname_tree
        .values()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    let live = || indexed.iter().chain(produced);

    if whole_project {
        let ids: HashSet<&str> = live().map(|s| s.symbol_id.as_str()).collect();
        effects_tree.retain(|id, value| {
            let keep = ids.contains(id.as_str()) || crate::effects::value_carries_evidence(value);
            if !keep {
                pass1.orphaned_effects.push(id.clone());
            }
            keep
        });
    }

    let code_keys: HashSet<(&str, String)> = live()
        .map(|s| {
            (
                s.language.as_str(),
                format!("{}/{}", paths::clean(&s.file), s.symbol_fp),
            )
        })
        .collect();
    for (lang, by_key) in code_tree.iter_mut() {
        let Value::Object(by_key) = by_key else {
            continue;
        };
        by_key.retain(|key, value| {
            let file = value.get("file").and_then(Value::as_str);
            let keep = file.is_none_or(|f| !in_scope(f))
                || code_keys.contains(&(lang.as_str(), key.clone()));
            if !keep {
                pass1.code_entries_pruned += 1;
            }
            keep
        });
    }
    code_tree.retain(|_, by_key| by_key.as_object().is_none_or(|m| !m.is_empty()));
}

/// Compute transitive effects entirely in memory, then write the ones that
/// changed in one speculation.
///
/// Takes `callees_of` from the in-memory Pass-2 map to avoid repo reads
/// during the DFS.
fn propagate_transitive_batched(
    repo: &Repository,
    ref_name: &str,
    symbol_ids: &[String],
    callees_of: &HashMap<String, Vec<String>>,
    agent_id: &str,
) -> Result<usize> {
    let updates = transitive_updates(repo, ref_name, symbol_ids, callees_of);
    write_transitive(repo, ref_name, &updates, agent_id)
}

/// Each symbol whose stored `transitive` effects differ from the ones its
/// callees now imply, with the new list.
fn transitive_updates(
    repo: &Repository,
    ref_name: &str,
    symbol_ids: &[String],
    callees_of: &HashMap<String, Vec<String>>,
) -> Vec<(String, Vec<TransitiveEffect>)> {
    // Read the complete effects tree once.
    let effects_tree = repo
        .get_tree(ref_name, "/asd/v1/effects")
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    // Deserialize into a local cache for fast access.
    let effects_cache: HashMap<String, EffectDecl> = effects_tree
        .iter()
        .filter_map(|(k, v)| {
            serde_json::from_value::<EffectDecl>(v.clone())
                .ok()
                .map(|d| (k.clone(), d))
        })
        .collect();

    crate::transitive::transitive_effects(callees_of, &effects_cache, symbol_ids)
        .into_iter()
        .filter(|(sym, new_transitive)| {
            effects_cache.get(sym).is_some_and(|decl| {
                !crate::transitive::transitive_eq(&decl.transitive, new_transitive)
            })
        })
        .collect()
}

/// Write each symbol's new `transitive` field — and nothing else. Writing
/// back the whole effects map read by [`transitive_updates`] reverted any
/// effect declared, traced or verified while the pass computed.
///
/// One subtree write, not one per symbol: a nested `spec_set_json` copies
/// every map on the path, so per-symbol writes stored a copy of the whole
/// effects map for each update (10x the database on a cold index). The
/// subtree is the one the speculation forked from, so the speculation still
/// changes only these fields.
fn write_transitive(
    repo: &Repository,
    ref_name: &str,
    updates: &[(String, Vec<TransitiveEffect>)],
    agent_id: &str,
) -> Result<usize> {
    if updates.is_empty() {
        return Ok(0);
    }
    let (spec, fork) = crate::subtree::speculate_at_head(repo, ref_name, "asd-index-transitive")?;
    let staged = (|| -> Result<usize> {
        let mut effects = crate::subtree::read_seed(repo, &fork, "/asd/v1/effects")?;
        let mut updated = 0;
        for (sym_id, transitive) in updates {
            // Gone since the pass read it: nothing left to annotate.
            let Some(Value::Object(decl)) = effects.get_mut(sym_id) else {
                continue;
            };
            let value =
                serde_json::to_value(transitive).map_err(|e| AsdError::Other(e.to_string()))?;
            decl.insert("transitive".to_string(), value);
            updated += 1;
        }
        repo.spec_set_json(spec, "/asd/v1/effects", &Value::Object(effects))
            .map_err(|e| AsdError::Other(e.to_string()))?;
        Ok(updated)
    })();
    let updated = match staged {
        Ok(n) => n,
        Err(e) => {
            let _ = repo.discard_speculation(spec);
            return Err(e);
        }
    };
    let opts = CommitOptions::new(
        agent_id,
        IntentCategory::Refine,
        format!("asd index: transitive effects for {} symbols", updated),
    );
    repo.commit_speculation(spec, opts)
        .map_err(|e| AsdError::Other(e.to_string()))?;

    Ok(updated)
}

fn same_module(caller: &str, callee: &str) -> bool {
    let cm = caller.split('.').next().unwrap_or("");
    let ee = callee.split('.').next().unwrap_or("");
    !cm.is_empty() && cm == ee
}

/// Collect source files under `root`, respecting built-in exclusions and an
/// optional `.asdignore` file in `root` (one directory-name pattern per line).
/// The git revision `root` is currently checked out at, if it is a git
/// worktree at all.
///
/// Recorded on the index checkpoint so an unpinned milestone still says which
/// source produced it. The full hash is used deliberately: an abbreviation can
/// stop resolving uniquely as a repository grows, and this value's whole job is
/// to still be resolvable long after the snapshot it replaces was reclaimed.
///
/// Best-effort — a non-git checkout, a repository with no commits, or a missing
/// `git` binary all yield `None`, and the checkpoint is simply written without
/// the tag.
fn git_head_sha(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

pub fn collect_source_files(
    root: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
) -> Result<CollectResult> {
    let mut recognized = Vec::new();
    let mut skipped = Vec::new();
    if root.is_file() {
        if let Some(adapter) = adapter_for_path(root, adapters) {
            recognized.push((root.to_path_buf(), adapter));
        } else {
            skipped.push(root.to_path_buf());
        }
        return Ok(CollectResult {
            recognized,
            skipped,
        });
    }
    let extra_excludes = load_asdignore(root);
    walk(
        root,
        adapters,
        &mut recognized,
        &mut skipped,
        &extra_excludes,
    )?;
    Ok(CollectResult {
        recognized,
        skipped,
    })
}

/// Read `.asdignore` from `root` and return non-empty, non-comment lines.
fn load_asdignore(root: &Path) -> Vec<String> {
    let path = root.join(".asdignore");
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn adapter_for_path(
    p: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
) -> Option<Arc<dyn LanguageAdapter>> {
    let ext = p.extension().and_then(|s| s.to_str())?;
    adapters
        .iter()
        .find(|a| a.file_extensions().contains(&ext))
        .cloned()
}

/// Built-in directory names that are always excluded from indexing.
fn is_builtin_excluded(name: &str) -> bool {
    matches!(
        name,
        // VCS / tooling
        ".git" | ".svn" | ".hg"
        // Python
        | ".venv" | "venv" | "__pycache__" | ".tox" | ".mypy_cache"
        // JS / TS
        | "node_modules" | "dist" | ".next" | ".turbo"
        // Rust
        | "target"
        // Swift / Xcode
        | ".build" | "DerivedData" | "xcuserdata" | ".xcodeproj"
        // Generic build outputs
        | "build" | "out" | ".cache"
        // Claude / AI worktrees
        | ".claude"
        // ASD's own state dir
        | ".asd"
    )
}

fn walk(
    dir: &Path,
    adapters: &[Arc<dyn LanguageAdapter>],
    out: &mut Vec<(PathBuf, Arc<dyn LanguageAdapter>)>,
    skipped: &mut Vec<PathBuf>,
    extra_excludes: &[String],
) -> Result<()> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| AsdError::Other(format!("read_dir {}: {}", dir.display(), e)))?;
    for entry in rd {
        let entry = entry.map_err(|e| AsdError::Other(e.to_string()))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|e| AsdError::Other(e.to_string()))?;
        if ft.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if is_builtin_excluded(name) {
                continue;
            }
            if extra_excludes.iter().any(|pat| name == pat.as_str()) {
                continue;
            }
            walk(&path, adapters, out, skipped, extra_excludes)?;
        } else if ft.is_file() {
            if let Some(adapter) = adapter_for_path(&path, adapters) {
                out.push((path, adapter));
            } else {
                skipped.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{AsgEffectStore, EffectStore};
    use crate::engine::Engine;
    use crate::schema::{Effect, EffectCategory};

    fn decl(symbol_id: &str, declared: Vec<Effect>) -> EffectDecl {
        EffectDecl {
            symbol_id: symbol_id.into(),
            declared,
            transitive: Vec::new(),
            verification: None,
            confidence: None,
            runtime: None,
            matched_policy: None,
        }
    }

    #[test]
    fn the_transitive_write_keeps_a_declaration_made_while_it_computed() {
        let engine = Engine::open_in_memory().unwrap();
        let (repo, ref_name) = (&engine.repo, engine.ref_name.as_str());
        let store = AsgEffectStore::new(repo);
        let net = Effect {
            adapter: Some("python".into()),
            ..Effect::new(EffectCategory::IoNetOut)
        };
        store
            .put_effects(ref_name, "caller", &decl("caller", Vec::new()), "index")
            .unwrap();
        store
            .put_effects(ref_name, "callee", &decl("callee", vec![net]), "index")
            .unwrap();

        let callees_of = HashMap::from([("caller".to_string(), vec!["callee".to_string()])]);
        let ids = vec!["caller".to_string(), "callee".to_string()];
        let updates = transitive_updates(repo, ref_name, &ids, &callees_of);
        assert_eq!(updates.len(), 1, "caller reaches io.net.out via callee");

        // An `effect_declare` lands while the pass computes.
        let by_hand = Effect {
            note: Some("writes the rate cache".into()),
            ..Effect::new(EffectCategory::IoFsWrite)
        };
        store
            .put_effects(ref_name, "caller", &decl("caller", vec![by_hand]), "human")
            .unwrap();

        write_transitive(repo, ref_name, &updates, "index").unwrap();

        let after = store.get_effects(ref_name, "caller").unwrap().unwrap();
        assert_eq!(
            after.declared.first().and_then(|e| e.note.as_deref()),
            Some("writes the rate cache"),
            "the transitive write reverted a declaration: {after:?}"
        );
        assert_eq!(after.transitive.len(), 1);
        assert_eq!(after.transitive[0].via, vec!["callee".to_string()]);
    }
}
