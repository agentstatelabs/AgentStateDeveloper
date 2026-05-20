//! CLI re-export of the brief helpers that now live in core
//! (`agentstatedeveloper_core::brief`). Kept as a single-line module so
//! existing `crate::commands::brief::*` call sites in read.rs / graph.rs
//! resolve unchanged. Plan D t-007.

pub use agentstatedeveloper_core::brief::{
    brief_call_list, brief_from_env, brief_read, brief_search_results, brief_symbol, query_id,
};
