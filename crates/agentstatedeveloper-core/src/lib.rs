//! agentstatedeveloper-core — Core traits, schema, and ASG-backed
//! implementations for AgentStateDeveloper.
//!
//! This crate is language-agnostic. Language adapters live in sibling
//! crates (e.g., `agentstatedeveloper-python`).

pub mod adapter;
pub mod effects;
pub mod engine;
pub mod error;
pub mod index;
pub mod ledger;
pub mod paths;
pub mod policy;
pub mod schema;
pub mod sidecar;
pub mod symbol;
pub mod transitive;

pub use adapter::{CallEdge, LanguageAdapter, ParsedSymbol, WorkspaceSymbols};
pub use effects::{AsgEffectStore, EffectStore};
pub use engine::Engine;
pub use error::{AsdError, Result};
pub use index::{AsgIndexStore, IndexStore};
pub use ledger::{ApprovalOutcome, AsgLedgerStore, LedgerStore, ReviewOutcome};
pub use policy::{
    Decision, FilePolicyGate, PermissivePolicyGate, PolicyFile, PolicyGate, PolicyRule, Situation,
    actions,
};
pub use schema::{
    ASD_PATH_PREFIX, ASD_SCHEMA_VERSION, Author, AuthorKind, Effect, EffectCategory, EffectDecl,
    Evidence, LedgerEntry, LedgerKind, Mismatch, Position, Symbol, SymbolKind, TransitiveEffect,
    Verification, VerificationSource, VerificationStatus,
};
pub use sidecar::{hydrate_from_dir, sync_to_dir, HydrateSummary, SyncSummary};
pub use symbol::{REBIND_SIMILARITY_THRESHOLD, canonical_symbol_id, symbol_fingerprint};
pub use transitive::propagate_transitive;
