//! Plan C t-004: change-intent recipes.
//!
//! Recipes are structured outputs that replace raw symbol lists for known
//! task families. Where a query like "migrate stale tests" today returns
//! a flat list of candidate symbols that the LLM has to classify itself,
//! a recipe returns a per-file action plan (move / delete / run / gate /
//! keep-as-covered) with a one-line reason for each action.
//!
//! The schema is intentionally minimal so additional recipes (Plan C+)
//! can land without re-litigating the shape.

use serde::Serialize;

use crate::engine::Engine;
use crate::index::{AsgIndexStore, IndexStore};
use crate::ledger::{AsgLedgerStore, LedgerStore};
use crate::schema::{LedgerKind, RoleTag};
use crate::search_fts::symbol_tier;

#[derive(Debug, Clone, Serialize)]
pub struct Recipe {
    pub intent: String,
    pub query: String,
    pub actions: Vec<RecipeAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipeAction {
    pub kind: ActionKind,
    pub file: String,
    pub qname: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Action kinds emitted by recipes. Each value corresponds to a branch
/// in some recipe's `pick_action`-style decision tree. Variants are added
/// only when at least one decision tree actually emits them.
///
/// Plan F t-002: `Move` re-added for the `migrate_stale_tests` recipe,
/// which reads `move_to` out of a Mapping entry's body JSON to specify
/// the destination path. Emitted only by that recipe, not by
/// `classify_test_migration`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Move,
    Delete,
    Run,
    Gate,
    KeepAsCovered,
    Review,
}

/// First concrete recipe: classify the test-tier symbols matching a
/// query by their migration story. Walks test files in scope, reads the
/// ledger for each symbol, and picks an `ActionKind` per the role tags
/// + Mapping entries already in the ledger.
///
/// Decision tree (first match wins, per symbol):
/// 1. `Mapping` entry → `KeepAsCovered`
/// 2. `Constraint`/`Decision` with role=stale-api → `Delete`
/// 3. `Ownership`/`Concept` with role=diagnostic-test → `Gate`
/// 4. `Ownership`/`Concept` with role=fast-test + command → `Run`
/// 5. Default → `Review`
pub fn classify_test_migration(
    engine: &Engine,
    index_store: &AsgIndexStore,
    candidate_qnames: &[String],
    query: &str,
) -> Recipe {
    let ledger_store = AsgLedgerStore::from_engine(engine);
    let mut actions: Vec<RecipeAction> = Vec::new();

    for qname in candidate_qnames {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        if symbol_tier(&sym.file) != 2 {
            continue;
        }
        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();

        actions.push(pick_action(&entries, sym.file.clone(), sym.qname.clone()));
    }

    actions.sort_by_key(|a| action_sort_key(a.kind));

    Recipe {
        intent: "classify-test-migration".to_string(),
        query: query.to_string(),
        actions,
    }
}

fn pick_action(
    entries: &[crate::schema::LedgerEntry],
    file: String,
    qname: String,
) -> RecipeAction {
    for entry in entries {
        if matches!(entry.kind, LedgerKind::Mapping) {
            return RecipeAction {
                kind: ActionKind::KeepAsCovered,
                file,
                qname,
                reason: format!("covered by mapping: {}", entry.summary),
                command: entry.command.clone(),
                role: entry.role.clone(),
            };
        }
    }
    for entry in entries {
        if !matches!(entry.kind, LedgerKind::Constraint | LedgerKind::Decision) {
            continue;
        }
        if entry.role.as_deref().and_then(RoleTag::from_str) == Some(RoleTag::StaleApi) {
            return RecipeAction {
                kind: ActionKind::Delete,
                file,
                qname,
                reason: format!("stale-api per ledger: {}", entry.summary),
                command: entry.command.clone(),
                role: entry.role.clone(),
            };
        }
    }
    for entry in entries {
        if !matches!(entry.kind, LedgerKind::Ownership | LedgerKind::Concept) {
            continue;
        }
        if entry.role.as_deref().and_then(RoleTag::from_str) == Some(RoleTag::DiagnosticTest) {
            return RecipeAction {
                kind: ActionKind::Gate,
                file,
                qname,
                reason: format!("diagnostic-only per classification: {}", entry.summary),
                command: entry.command.clone(),
                role: entry.role.clone(),
            };
        }
    }
    for entry in entries {
        if !matches!(entry.kind, LedgerKind::Ownership | LedgerKind::Concept) {
            continue;
        }
        if entry.role.as_deref().and_then(RoleTag::from_str) == Some(RoleTag::FastTest)
            && entry.command.is_some()
        {
            return RecipeAction {
                kind: ActionKind::Run,
                file,
                qname,
                reason: "fast-test with reproduction command".to_string(),
                command: entry.command.clone(),
                role: entry.role.clone(),
            };
        }
    }
    RecipeAction {
        kind: ActionKind::Review,
        file,
        qname,
        reason: "no role-tagged ledger entry; needs human classification".to_string(),
        command: None,
        role: None,
    }
}

/// Plan F t-002 — `migrate-stale-tests` recipe. Extends
/// `classify_test_migration` with a Move action: when a Mapping entry's
/// body carries `move_to: "<destination/path>"`, the test file is
/// emitted as Move with that path in `command`. Otherwise falls back to
/// the same decision tree as classify_test_migration so callers get one
/// unified plan.
///
/// Body schema for Mapping entries this recipe consumes:
/// ```json
/// {"from_qname": "pkg.legacy_test", "to_qname": "pkg.modern_test",
///  "move_to": "tests/modern/foo_test.py"}
/// ```
/// Plain `to_qname` (no `move_to`) keeps the t-004 KeepAsCovered behavior.
pub fn migrate_stale_tests(
    engine: &Engine,
    index_store: &AsgIndexStore,
    candidate_qnames: &[String],
    query: &str,
) -> Recipe {
    let ledger_store = AsgLedgerStore::from_engine(engine);
    let mut actions: Vec<RecipeAction> = Vec::new();

    for qname in candidate_qnames {
        let sym = match index_store.get_symbol_by_qname(&engine.ref_name, qname) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        if symbol_tier(&sym.file) != 2 {
            continue;
        }
        let entries = ledger_store
            .list_entries(&engine.ref_name, &sym.symbol_id)
            .unwrap_or_default();

        // First match wins. Move takes precedence over KeepAsCovered
        // when a Mapping entry has both — the move_to is the operational
        // instruction; the to_qname is just provenance.
        if let Some(move_to) = entries.iter().find_map(|e| {
            if !matches!(e.kind, LedgerKind::Mapping) {
                return None;
            }
            mapping_move_to(e.body.as_deref())
        }) {
            actions.push(RecipeAction {
                kind: ActionKind::Move,
                file: sym.file.clone(),
                qname: sym.qname.clone(),
                reason: format!("mapping says move to {move_to}"),
                command: Some(move_to),
                role: None,
            });
            continue;
        }
        // Otherwise reuse the classify_test_migration decision tree —
        // single source of truth for the remaining branches.
        actions.push(pick_action(&entries, sym.file.clone(), sym.qname.clone()));
    }

    actions.sort_by_key(|a| action_sort_key(a.kind));

    Recipe {
        intent: "migrate-stale-tests".to_string(),
        query: query.to_string(),
        actions,
    }
}

/// Parse `move_to` out of a Mapping entry's body JSON. Returns None on
/// missing body, non-JSON, or missing move_to field — caller falls back
/// to the KeepAsCovered branch.
fn mapping_move_to(body: Option<&str>) -> Option<String> {
    let raw = body?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.get("move_to")?.as_str().map(|s| s.to_string())
}

fn action_sort_key(kind: ActionKind) -> u8 {
    match kind {
        ActionKind::Delete => 0,
        ActionKind::Move => 1,
        ActionKind::Gate => 2,
        ActionKind::Run => 3,
        ActionKind::KeepAsCovered => 4,
        ActionKind::Review => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Author, AuthorKind, LedgerEntry, Position, Symbol, SymbolKind};

    fn engine_with_symbol(qname: &str, file: &str) -> (Engine, String) {
        let engine = Engine::open_in_memory().unwrap();
        let index = AsgIndexStore::from_engine(&engine);
        let sym = Symbol {
            symbol_id: format!("sym_{}", qname.replace('.', "_")),
            symbol_fp: "fp".into(),
            qname: qname.to_string(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: file.to_string(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };
        index.put_symbol(&engine.ref_name, &sym, "test").unwrap();
        (engine, sym.symbol_id)
    }

    fn append(
        engine: &Engine,
        sym_id: &str,
        kind: LedgerKind,
        role: Option<&str>,
        command: Option<&str>,
    ) {
        let ledger = AsgLedgerStore::from_engine(engine);
        let mut entry = LedgerEntry::new(
            sym_id,
            kind,
            "test entry",
            Author {
                kind: AuthorKind::Agent,
                id: "test".into(),
            },
        );
        entry.role = role.map(str::to_string);
        entry.command = command.map(str::to_string);
        ledger
            .append_entry(&engine.ref_name, &entry, "test")
            .unwrap();
    }

    #[test]
    fn stale_api_constraint_yields_delete() {
        let (engine, sid) = engine_with_symbol("pkg.tests.legacy_test", "tests/legacy_test.py");
        append(
            &engine,
            &sid,
            LedgerKind::Constraint,
            Some("stale-api"),
            None,
        );
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = classify_test_migration(
            &engine,
            &index,
            &["pkg.tests.legacy_test".to_string()],
            "migrate stale tests",
        );
        assert_eq!(recipe.actions.len(), 1);
        assert_eq!(recipe.actions[0].kind, ActionKind::Delete);
    }

    #[test]
    fn mapping_yields_keep_as_covered() {
        let (engine, sid) = engine_with_symbol("pkg.tests.covered_test", "tests/covered_test.py");
        append(&engine, &sid, LedgerKind::Mapping, None, None);
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = classify_test_migration(
            &engine,
            &index,
            &["pkg.tests.covered_test".to_string()],
            "migrate tests",
        );
        assert_eq!(recipe.actions[0].kind, ActionKind::KeepAsCovered);
    }

    #[test]
    fn diagnostic_classification_yields_gate() {
        let (engine, sid) =
            engine_with_symbol("pkg.tests.real_file_test", "tests/real_file_test.py");
        append(
            &engine,
            &sid,
            LedgerKind::Ownership,
            Some("diagnostic-test"),
            None,
        );
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = classify_test_migration(
            &engine,
            &index,
            &["pkg.tests.real_file_test".to_string()],
            "migrate tests",
        );
        assert_eq!(recipe.actions[0].kind, ActionKind::Gate);
    }

    #[test]
    fn fast_test_classification_with_command_yields_run() {
        let (engine, sid) = engine_with_symbol("pkg.tests.fast_test", "tests/fast_test.py");
        append(
            &engine,
            &sid,
            LedgerKind::Ownership,
            Some("fast-test"),
            Some("swift test --filter FastTest"),
        );
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = classify_test_migration(
            &engine,
            &index,
            &["pkg.tests.fast_test".to_string()],
            "migrate tests",
        );
        assert_eq!(recipe.actions[0].kind, ActionKind::Run);
        assert_eq!(
            recipe.actions[0].command.as_deref(),
            Some("swift test --filter FastTest")
        );
    }

    #[test]
    fn no_ledger_evidence_falls_to_review() {
        let (engine, _) = engine_with_symbol("pkg.tests.bare_test", "tests/bare_test.py");
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = classify_test_migration(
            &engine,
            &index,
            &["pkg.tests.bare_test".to_string()],
            "migrate tests",
        );
        assert_eq!(recipe.actions[0].kind, ActionKind::Review);
    }

    #[test]
    fn non_test_tier_symbols_are_skipped() {
        let (engine, sid) = engine_with_symbol("pkg.module.production_fn", "src/pkg/module.py");
        append(&engine, &sid, LedgerKind::Mapping, None, None);
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = classify_test_migration(
            &engine,
            &index,
            &["pkg.module.production_fn".to_string()],
            "migrate tests",
        );
        assert_eq!(
            recipe.actions.len(),
            0,
            "production-tier symbols must not appear in a test-migration recipe"
        );
    }

    // -- Plan F t-002: migrate_stale_tests recipe ---------------------------

    fn append_mapping_with_body(engine: &Engine, sym_id: &str, body: &str) {
        let ledger = AsgLedgerStore::from_engine(engine);
        let mut entry = LedgerEntry::new(
            sym_id,
            LedgerKind::Mapping,
            "mapping with body",
            Author { kind: AuthorKind::Agent, id: "test".into() },
        );
        entry.body = Some(body.to_string());
        ledger.append_entry(&engine.ref_name, &entry, "test").unwrap();
    }

    #[test]
    fn mapping_move_to_parses_body_field() {
        assert_eq!(
            super::mapping_move_to(Some(r#"{"move_to":"tests/modern/foo.py"}"#)),
            Some("tests/modern/foo.py".to_string())
        );
    }

    #[test]
    fn mapping_move_to_returns_none_when_missing() {
        assert_eq!(super::mapping_move_to(None), None);
        assert_eq!(super::mapping_move_to(Some("plain text")), None);
        assert_eq!(super::mapping_move_to(Some(r#"{"to_qname":"x"}"#)), None);
    }

    #[test]
    fn migrate_stale_tests_emits_move_when_mapping_has_move_to() {
        let (engine, sid) =
            engine_with_symbol("pkg.tests.legacy_test", "tests/legacy_test.py");
        append_mapping_with_body(
            &engine,
            &sid,
            r#"{"from_qname":"pkg.tests.legacy_test","to_qname":"pkg.tests.modern_test","move_to":"tests/modern/foo_test.py"}"#,
        );
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = super::migrate_stale_tests(
            &engine,
            &index,
            &["pkg.tests.legacy_test".to_string()],
            "migrate stale tests",
        );
        assert_eq!(recipe.intent, "migrate-stale-tests");
        assert_eq!(recipe.actions.len(), 1);
        assert_eq!(recipe.actions[0].kind, ActionKind::Move);
        assert_eq!(
            recipe.actions[0].command.as_deref(),
            Some("tests/modern/foo_test.py")
        );
    }

    #[test]
    fn migrate_stale_tests_falls_back_to_classify_decision_tree() {
        // A Mapping entry without move_to should fall back to
        // KeepAsCovered, matching classify_test_migration.
        let (engine, sid) =
            engine_with_symbol("pkg.tests.covered_test", "tests/covered_test.py");
        append_mapping_with_body(
            &engine,
            &sid,
            r#"{"from_qname":"pkg.tests.covered_test","to_qname":"pkg.tests.new"}"#,
        );
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = super::migrate_stale_tests(
            &engine,
            &index,
            &["pkg.tests.covered_test".to_string()],
            "migrate stale tests",
        );
        assert_eq!(recipe.actions[0].kind, ActionKind::KeepAsCovered);
    }

    #[test]
    fn migrate_stale_tests_routes_stale_api_constraint_to_delete() {
        // Inherits the Delete branch from pick_action.
        let (engine, sid) =
            engine_with_symbol("pkg.tests.legacy", "tests/legacy_test.py");
        append(&engine, &sid, LedgerKind::Constraint, Some("stale-api"), None);
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = super::migrate_stale_tests(
            &engine,
            &index,
            &["pkg.tests.legacy".to_string()],
            "migrate stale tests",
        );
        assert_eq!(recipe.actions[0].kind, ActionKind::Delete);
    }

    #[test]
    fn migrate_stale_tests_skips_non_test_tier() {
        let (engine, sid) =
            engine_with_symbol("pkg.module.production_fn", "src/pkg/module.py");
        append_mapping_with_body(
            &engine,
            &sid,
            r#"{"move_to":"src/new.py"}"#,
        );
        let index = AsgIndexStore::from_engine(&engine);
        let recipe = super::migrate_stale_tests(
            &engine,
            &index,
            &["pkg.module.production_fn".to_string()],
            "migrate",
        );
        assert_eq!(recipe.actions.len(), 0);
    }
}
