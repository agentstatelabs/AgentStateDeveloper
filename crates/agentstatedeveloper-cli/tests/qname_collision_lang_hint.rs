//! Plan J t-009: `get_symbol_by_qname_lang(_, qname, Some(lang))`
//! prefers the language-matched symbol when the primary qname
//! index holds a different-language collision winner.
//!
//! Background: `put_symbol` writes the qname index at
//! `/asd/v1/index/by-qname/{qname}`, keyed ONLY by qname. When
//! two adapters produce the same qname (e.g. `auth.User` in both
//! Python and Swift), the second write wins at that secondary
//! index. The per-language code tree at
//! `/asd/v1/code/{lang}/{file}/...` still has both. t-009 adds an
//! additive lookup that walks the per-language tree when the
//! primary entry doesn't match the hint, so callers with a
//! language hint resolve to the right symbol even after a write
//! collision.

use std::path::Path;

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Position, Symbol, SymbolKind,
};

fn mk_sym(sym_id: &str, qname: &str, file: &str, language: &str) -> Symbol {
    Symbol {
        symbol_id: sym_id.into(),
        symbol_fp: format!("fp-{sym_id}"),
        qname: qname.into(),
        language: language.into(),
        kind: SymbolKind::Class,
        file: file.into(),
        start: Position { line: 1, col: 0 },
        end: Position { line: 5, col: 0 },
        signature: Some(format!("class {}", qname.rsplit('.').next().unwrap_or(qname))),
        doc: Some(format!("Class {qname} (lang={language})")),
    }
}

fn seed_collision(db: &Path) {
    let engine = Engine::open_sqlite(db).expect("open");
    let idx = AsgIndexStore::from_engine(&engine);
    // Same qname, two languages. Write order: python first, then
    // swift — so the qname index ends up holding the Swift one
    // (last-write-wins). The Python symbol is still recoverable
    // via the per-language code tree.
    let py = mk_sym("sym_py_user", "auth.User", "src/auth/models.py", "python");
    let sw = mk_sym("sym_sw_user", "auth.User", "App/Auth/User.swift", "swift");
    idx.put_symbol(&engine.ref_name, &py, "t").unwrap();
    idx.put_symbol(&engine.ref_name, &sw, "t").unwrap();
}

#[test]
fn no_hint_returns_qname_index_winner() {
    // Backward compat: without a hint, lang-aware lookup behaves
    // exactly like the bare get_symbol_by_qname. Lock the
    // last-write-wins semantics so callers can rely on a
    // deterministic answer when no hint is available.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_collision(&db);

    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);

    let sym = idx
        .get_symbol_by_qname_lang(&engine.ref_name, "auth.User", None)
        .unwrap()
        .expect("primary qname index hit");
    assert_eq!(sym.language, "swift", "last-write-wins: swift overwrote python");
    assert_eq!(sym.symbol_id, "sym_sw_user");
}

#[test]
fn matching_hint_short_circuits_to_primary() {
    // Fast path: hint matches the primary entry — no per-language
    // tree walk needed.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_collision(&db);

    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);

    let sym = idx
        .get_symbol_by_qname_lang(&engine.ref_name, "auth.User", Some("swift"))
        .unwrap()
        .expect("swift hit");
    assert_eq!(sym.language, "swift");
    assert_eq!(sym.symbol_id, "sym_sw_user");
}

#[test]
fn mismatched_hint_recovers_from_per_language_tree() {
    // The actual t-009 fix: primary index has Swift's entry
    // (last-write-wins), but the caller asked for Python. The
    // per-language tree walk recovers sym_py_user.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_collision(&db);

    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);

    let sym = idx
        .get_symbol_by_qname_lang(&engine.ref_name, "auth.User", Some("python"))
        .unwrap()
        .expect("python recovered via per-language tree");
    assert_eq!(
        sym.language, "python",
        "lang_hint must recover the python symbol from the code tree"
    );
    assert_eq!(sym.symbol_id, "sym_py_user");
    assert_eq!(sym.file, "src/auth/models.py");
}

#[test]
fn hint_for_unknown_language_falls_back_to_primary() {
    // Hint specifies a language we don't have indexed (kotlin).
    // Better to return the primary qname-index hit than None —
    // caller can inspect Symbol.language and decide. Returning
    // None on hint miss would silently break every call site
    // that passes the session's default language even when only
    // one language is indexed.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_collision(&db);

    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);

    let sym = idx
        .get_symbol_by_qname_lang(&engine.ref_name, "auth.User", Some("kotlin"))
        .unwrap()
        .expect("falls back to primary even with mismatched hint");
    assert_eq!(sym.language, "swift", "fell back to primary (swift)");
}

#[test]
fn empty_hint_string_treated_as_no_hint() {
    // Defensive: callers may pass Some("") when they have no
    // session language. Should behave identically to None, not
    // trigger a per-language walk for an empty path.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_collision(&db);

    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);

    let sym = idx
        .get_symbol_by_qname_lang(&engine.ref_name, "auth.User", Some(""))
        .unwrap()
        .expect("empty hint behaves like no hint");
    assert_eq!(sym.language, "swift");
}

#[test]
fn missing_qname_returns_none_regardless_of_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed_collision(&db);

    let engine = Engine::open_sqlite(&db).unwrap();
    let idx = AsgIndexStore::from_engine(&engine);

    let sym = idx
        .get_symbol_by_qname_lang(&engine.ref_name, "auth.DoesNotExist", Some("swift"))
        .unwrap();
    assert!(sym.is_none(), "missing qname → None");
}
