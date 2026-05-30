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
/// only when at least one decision tree emits them — Plan E t-003 dropped
/// the unused `Move` variant; a future `migrate-stale-tests` recipe
/// (Plan F t-002) will re-add it with the body field needed to specify
/// the destination path.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
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

fn action_sort_key(kind: ActionKind) -> u8 {
    match kind {
        ActionKind::Delete => 0,
        ActionKind::Gate => 1,
        ActionKind::Run => 2,
        ActionKind::KeepAsCovered => 3,
        ActionKind::Review => 4,
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
}
