//! One ledger entry filed under more than one symbol.
//!
//! An entry id belongs under exactly one symbol. Before v1.4.7 `asd hydrate`
//! re-filed entries from `.asd/v1/` under the symbol the sidecar last saw
//! them on, even when the store had since moved them (a line shift, a
//! rebind): SessionDrift-ios ended up with 272 entries stored twice, once
//! under the live symbol and once under a dead id, and the ledger cache —
//! one row per entry — pointed at the dead copy for 144 of them, hiding
//! those entries from their live symbol. Counts never showed it, because
//! they count entry ids.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::schema::LedgerEntry;

/// Every symbol each entry id is filed under, read off a ledger tree
/// (`/asd/v1/ledger`: symbol id → entry id → entry).
pub(crate) fn locations(ledger: &serde_json::Map<String, Value>) -> BTreeMap<String, Vec<String>> {
    let mut at: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (symbol_id, entries) in ledger {
        if let Value::Object(entries) = entries {
            for entry_id in entries.keys() {
                at.entry(entry_id.clone())
                    .or_default()
                    .push(symbol_id.clone());
            }
        }
    }
    at
}

/// How strongly one stored copy of an entry claims to be the real one: a
/// copy under a symbol the index still holds beats one under a dead id,
/// then the latest revision wins (as in conclusions import). Callers break
/// remaining ties their own way.
pub(crate) fn claim(
    symbol_id: &str,
    entry: &LedgerEntry,
    live: &HashSet<String>,
) -> (bool, DateTime<Utc>) {
    (
        live.contains(symbol_id),
        crate::conclusions_export::revised_at(entry),
    )
}
