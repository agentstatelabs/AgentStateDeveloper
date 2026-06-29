//! ASG path convention for ASD data.
//!
//! All ASD state lives under `/asd/v1/` inside an ASG repository.

pub const ASD_ROOT: &str = "/asd/v1";

pub fn clean(p: &str) -> String {
    p.trim_start_matches('/').replace("//", "/")
}

pub fn code_path(lang: &str, file: &str, symbol_fp: &str) -> String {
    format!("{}/code/{}/{}/{}", ASD_ROOT, lang, clean(file), symbol_fp)
}

pub fn qname_index_path(qname: &str) -> String {
    format!("{}/index/by-qname/{}", ASD_ROOT, qname)
}

pub fn callers_path(symbol_id: &str) -> String {
    format!("{}/index/callers/{}", ASD_ROOT, symbol_id)
}

pub fn callees_path(symbol_id: &str) -> String {
    format!("{}/index/callees/{}", ASD_ROOT, symbol_id)
}

pub fn effects_reverse_index_path(effect: &str, symbol_id: &str) -> String {
    format!("{}/index/effects-rev/{}/{}", ASD_ROOT, effect, symbol_id)
}

// ---------------------------------------------------------------------------
// Cross-service edge paths (t-002)
//
// Service endpoints (HTTP routes/clients, pub-sub emit/listen) are indexed by a
// hash of their normalized *contract key* so endpoints from any repo that share
// a contract collapse under one prefix — the cross-service analog of the
// effects-rev reverse index. The contract hash (not the raw key) is used as a
// path segment to keep keys path-safe (raw keys contain '/', ':', spaces).
// ---------------------------------------------------------------------------

/// All endpoints (local + imported) for one contract live under this prefix.
pub fn endpoint_contract_prefix(contract_hash: &str) -> String {
    format!("{}/index/endpoints/{}", ASD_ROOT, contract_hash)
}

/// A single endpoint record, namespaced by repo_id so same-contract endpoints
/// from different repos never collide.
pub fn endpoint_path(contract_hash: &str, repo_id: &str, symbol_id: &str) -> String {
    format!(
        "{}/index/endpoints/{}/{}/{}",
        ASD_ROOT, contract_hash, repo_id, symbol_id
    )
}

/// This repo's exported service manifest (the unit other repos import to match
/// contracts cross-repo).
pub fn service_manifest_path() -> String {
    format!("{}/meta/service-manifest", ASD_ROOT)
}

/// Per-edge runtime-confidence sidecar (t-013), keyed by from→to symbol id.
/// Kept separate from the hot-path callees/callers lists.
pub fn edge_evidence_path(from_symbol_id: &str, to_symbol_id: &str) -> String {
    format!(
        "{}/index/edge-evidence/{}/{}",
        ASD_ROOT, from_symbol_id, to_symbol_id
    )
}

pub fn ledger_entry_path(symbol_id: &str, entry_id: &str) -> String {
    format!("{}/ledger/{}/{}", ASD_ROOT, symbol_id, entry_id)
}

pub fn ledger_symbol_path(symbol_id: &str) -> String {
    format!("{}/ledger/{}", ASD_ROOT, symbol_id)
}

pub fn effects_path(symbol_id: &str) -> String {
    format!("{}/effects/{}", ASD_ROOT, symbol_id)
}

pub fn traces_path(symbol_id: &str, trace_id: &str) -> String {
    format!("{}/traces/{}/{}", ASD_ROOT, symbol_id, trace_id)
}

pub fn file_meta_path(file: &str) -> String {
    format!("{}/meta/files/{}", ASD_ROOT, clean(file))
}

pub fn schema_version_path() -> String {
    format!("{}/meta/schema-version", ASD_ROOT)
}

pub fn rebind_path(from_symbol_id: &str) -> String {
    format!("{}/rebinds/{}", ASD_ROOT, from_symbol_id)
}

/// Reverse index: maps entry_id → symbol_id for O(1) find_entry in ratify.
/// Kept under a separate prefix (ledger-idx/) to avoid polluting tree walks
/// over the main ledger/ subtree.
pub fn ledger_entry_index_path(entry_id: &str) -> String {
    format!("{}/ledger-idx/{}", ASD_ROOT, entry_id)
}

// ---------------------------------------------------------------------------
// Scratchpad paths
// ---------------------------------------------------------------------------

/// Root prefix for all scratch entries. Flat layout (not symbol-nested)
/// because scratch has multiple scoping dimensions (symbol, workflow, session).
pub fn scratch_root() -> &'static str {
    "/asd/v1/scratch"
}

/// Path for a single scratch entry.
pub fn scratch_entry_path(scratch_id: &str) -> String {
    format!("/asd/v1/scratch/{}", scratch_id)
}

// ---------------------------------------------------------------------------
// Feedback paths
// ---------------------------------------------------------------------------

pub fn feedback_entry_path(symbol_id: &str, entry_id: &str) -> String {
    format!("{}/feedback/{}/{}", ASD_ROOT, symbol_id, entry_id)
}

pub fn feedback_symbol_path(symbol_id: &str) -> String {
    format!("{}/feedback/{}", ASD_ROOT, symbol_id)
}
