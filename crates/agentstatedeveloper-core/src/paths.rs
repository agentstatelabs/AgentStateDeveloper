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
