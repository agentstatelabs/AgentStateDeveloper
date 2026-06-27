//! agentstatedeveloper-core — Core traits, schema, and ASG-backed
//! implementations for AgentStateDeveloper.
//!
//! This crate is language-agnostic. Language adapters live in sibling
//! crates (e.g., `agentstatedeveloper-python`).

pub mod adapter;
pub mod audit;
pub mod brief;
pub mod calibration;
pub mod candidates;
pub mod context;
pub mod cross_service;
pub mod ser_helpers;
pub mod conclusions_export;
pub mod thinking;
pub mod doc_adapters;
pub mod effects;
pub mod engine;
pub mod error;
pub mod feedback;
pub mod index;
pub mod index_pipeline;
pub mod ledger;
pub mod paths;
pub mod policy;
pub mod prepare_change;
pub mod recipes;
pub mod registry;
pub mod repair;
pub mod schema;
pub mod scratch;
pub mod search_fts;
pub mod sidecar;
pub mod sidecar_config;
pub mod symbol;
pub mod transitive;
pub mod trust;
pub mod workflow;

pub use adapter::{
    CallEdge, DynamicDispatchHint, LanguageAdapter, ParsedSymbol, UnresolvedCall,
    WorkspaceSymbols,
};
pub use audit::{
    AuditEvent, AuditSink, HASH_PREFIX, NullSink, emit_audit, event_types, read_jsonl,
};
pub use candidates::{
    FeedbackImpact, FeedbackMetrics, FeedbackState, RecoverySuggestion, UncertaintyReason,
    UncertaintyReport, apply_feedback_adjustments, apply_file_scope_feedback, build_feedback_state,
    build_feedback_state_from_entries, compute_uncertainty, confidence_reason, confidence_scores,
    detect_ambiguous_tokens, detect_confidence_warnings, detect_possible_misses,
    explain_feedback_impacts, explain_match, find_candidates, glob_match, in_memory_score,
    kind_str, load_exclude_sets, load_scope_aliases, matches_any_path_glob, parse_query,
    query_tokens, resolve_exclude_set, resolve_scope,
    result_bucket, suggest_better_queries, suggest_scoped_queries,
};
pub use cross_service::{
    CrossServiceEdge, DetectedEndpoint, Direction, EndpointRef, ServiceEndpoint, ServiceManifest,
    Transport, contract_hash, http_contract, match_edges, normalize_repo_id, pubsub_contract,
    resolve_repo_id,
};
pub use doc_adapters::{adapt_document, is_doc_file};
pub use effects::{AsgEffectStore, EffectStore};
pub use engine::Engine;
pub use error::{AsdError, Result};
pub use feedback::{
    AsgFeedbackStore, DEFAULT_FEEDBACK_HALF_LIFE_DAYS, FeedbackStore, decay_factor,
    decay_for_entry,
};
pub use index::{AsgIndexStore, IndexStore};
pub use index_pipeline::{CollectResult, IndexSummary, collect_source_files, run_index};
pub use ledger::{
    ApprovalOutcome, AsgLedgerStore, LedgerStore, RatifyOps, ReviewOutcome, detect_orphaned_entries,
};
pub use policy::{
    Decision, FilePolicyGate, PermissivePolicyGate, PolicyFile, PolicyGate, PolicyRule, Situation,
    actions,
};
pub use repair::{
    IssueSeverity, RepairIssue, RepairReport, drop_orphaned_edge_refs, repair_asg, scan_asg,
    scan_sidecar,
};
pub use schema::{
    ASD_PATH_PREFIX, ASD_SCHEMA_VERSION, Author, AuthorKind, ConclusionClass, Effect,
    EffectCategory, EffectDecl, Evidence, FeedbackEntry, FeedbackVerdict, LedgerEntry, LedgerKind,
    Mismatch, Position, Rebind, RoleTag, RuntimeEvidence, ScratchEntry, ScratchStatus, Symbol,
    SymbolKind, TransitiveEffect, Verification, VerificationSource, VerificationStatus,
};
pub use scratch::{AsgScratchStore, CleanFilter, ScratchFilter, ScratchStore};
pub use search_fts::{
    AGENT_DEFAULT_BUDGET, AnnotatedOwner, CoveringTest, FileRecency, FtsFilters, FtsHit,
    OwnerSignalSource, OwnershipSignal, ResolvedSymbol, SearchFtsDb, SymbolMeta, SymbolTier,
    classify_file_role, classify_layer, classify_layer_sym, derive_cold_hints,
    discover_symbol_ownership,
    estimate_tokens, extract_summary, fetch_all_test_file_paths, find_covering_tests,
    find_indexed_test_files, gather_recency, git_dirty_files, hybrid_boost, intent_focus,
    intent_layer_order, is_stopword, load_layer_overrides, parse_intent, propose_test_path,
    propose_test_stub,
    symbol_tier, test_files_for_source, trim_for_agent,
};
pub use search_fts::{DocHit, DocKind, SearchDoc, SearchDocsDb};
pub use search_fts::{
    SOFT_STALE_THRESHOLD_SECS, StaleSeverity, StaleWarning, compute_index_consistency,
    effect_detail_reason, format_age, stale_warning, stale_warning_classified,
};
pub use ser_helpers::{drop_empty_recursive, drop_empty_top_level};
pub use context::assemble_symbol_context;
pub use prepare_change::{
    CLIFF_RATIO_THRESHOLD, CandidateAggregates, FILE_SCORE_FLOOR_RATIO, FileScoreTuple,
    aggregate_candidate_data, cliff_cutoff_index, dirty_files_for_change, explain_conflict_risk,
    file_score_floor, finalize_file_scores, propagate_caller_invariants,
};
pub use sidecar::{
    HydrateSummary, SidecarState, SyncSummary, hydrate_from_dir, mark_fresh_reset, prune_sidecar,
    sidecar_lifecycle_state, sync_to_dir,
};
pub use symbol::{canonical_symbol_id, symbol_fingerprint};
pub use transitive::propagate_transitive;
pub use trust::{DataQuality, TrustScore, TrustSignals, compute_trust_score};
pub use workflow::{
    EvidenceQuality, WorkflowSummary, append_workflow_session, detect_workflow,
    score_evidence_quality,
};
