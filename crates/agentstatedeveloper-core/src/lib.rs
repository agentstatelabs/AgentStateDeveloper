//! agentstatedeveloper-core — Core traits, schema, and ASG-backed
//! implementations for AgentStateDeveloper.
//!
//! This crate is language-agnostic. Language adapters live in sibling
//! crates (e.g., `agentstatedeveloper-python`).

pub mod adapter;
pub mod audit;
pub mod candidates;
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
pub mod repair;
pub mod schema;
pub mod scratch;
pub mod search_fts;
pub mod sidecar;
pub mod symbol;
pub mod transitive;

pub use adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
pub use index_pipeline::{CollectResult, IndexSummary, collect_source_files, run_index};
pub use audit::{AuditEvent, AuditSink, HASH_PREFIX, NullSink, emit_audit, event_types, read_jsonl};
pub use effects::{AsgEffectStore, EffectStore};
pub use engine::Engine;
pub use error::{AsdError, Result};
pub use index::{AsgIndexStore, IndexStore};
pub use ledger::{ApprovalOutcome, AsgLedgerStore, LedgerStore, RatifyOps, ReviewOutcome, detect_orphaned_entries};
pub use policy::{
    Decision, FilePolicyGate, PermissivePolicyGate, PolicyFile, PolicyGate, PolicyRule, Situation,
    actions,
};
pub use schema::{
    ASD_PATH_PREFIX, ASD_SCHEMA_VERSION, Author, AuthorKind, Effect, EffectCategory, EffectDecl,
    Evidence, FeedbackEntry, FeedbackVerdict, LedgerEntry, LedgerKind, Mismatch, Position, Rebind,
    ScratchEntry, ScratchStatus, Symbol, SymbolKind, TransitiveEffect, Verification,
    VerificationSource, VerificationStatus,
};
pub use scratch::{AsgScratchStore, CleanFilter, ScratchFilter, ScratchStore};
pub use feedback::{AsgFeedbackStore, FeedbackStore};
pub use candidates::{
    apply_feedback_adjustments, apply_file_scope_feedback, confidence_reason, confidence_scores,
    detect_ambiguous_tokens, detect_confidence_warnings, detect_possible_misses, explain_match,
    explain_feedback_impacts, FeedbackImpact, find_candidates,
    glob_match, in_memory_score, kind_str, load_scope_aliases, matches_any_path_glob,
    parse_query, query_tokens, resolve_scope, result_bucket, suggest_better_queries,
    suggest_scoped_queries,
};
pub use repair::{IssueSeverity, RepairIssue, RepairReport, drop_orphaned_edge_refs, repair_asg, scan_asg};
pub use search_fts::{AGENT_DEFAULT_BUDGET, AnnotatedOwner, CoveringTest, FileRecency, FtsFilters, FtsHit, OwnerSignalSource, OwnershipSignal, ResolvedSymbol, SearchFtsDb, SymbolMeta, SymbolTier, classify_layer, classify_layer_sym, derive_cold_hints, discover_symbol_ownership, estimate_tokens, extract_summary, find_covering_tests, find_indexed_test_files, gather_recency, git_dirty_files, hybrid_boost, intent_focus, intent_layer_order, is_stopword, load_layer_overrides, parse_intent, propose_test_path, symbol_tier, trim_for_agent};
pub use search_fts::{effect_detail_reason, format_age, stale_warning};
pub use search_fts::{DocHit, DocKind, SearchDoc, SearchDocsDb};
pub use doc_adapters::{adapt_document, is_doc_file};
pub use sidecar::{hydrate_from_dir, mark_fresh_reset, prune_sidecar, sidecar_lifecycle_state, sync_to_dir, HydrateSummary, SidecarState, SyncSummary};
pub use symbol::{canonical_symbol_id, symbol_fingerprint};
pub use transitive::propagate_transitive;
