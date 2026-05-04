use crate::schema::SymbolKind;

/// Canonical stable id for a symbol. Survives content edits and path
/// moves; changes only on rename (which triggers a rebind record).
pub fn canonical_symbol_id(qname: &str, kind: SymbolKind, initial_file: &str) -> String {
    let kind_str = format!("{:?}", kind);
    let seed = format!("{}|{}|{}", qname, kind_str, initial_file);
    let hash = blake3::hash(seed.as_bytes());
    format!("sym_{}", &hash.to_hex()[..16])
}

/// Content-addressed fingerprint of a symbol's body. Changes on every edit.
pub fn symbol_fingerprint(body: &str) -> String {
    let hash = blake3::hash(body.as_bytes());
    format!("fp_{}", &hash.to_hex()[..16])
}

