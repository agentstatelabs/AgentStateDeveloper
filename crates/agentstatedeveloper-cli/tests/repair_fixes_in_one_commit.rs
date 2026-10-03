//! `asd repair --fix` lands every fix in one commit. It used to commit once
//! per orphaned effect and per rewritten edge list, each commit storing a
//! fresh copy of the enclosing map: on ThreadWeaver-ios, with 54,685
//! orphaned effects, roughly 100 GB.

use agentstatedeveloper_core::{Engine, paths, repair_asg, run_index};
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;
use serde_json::{Value, json};

fn put_json(engine: &Engine, path: &str, value: &Value) {
    engine
        .repo
        .set_json(
            &engine.ref_name,
            path,
            value,
            CommitOptions::new("t", IntentCategory::Refine, format!("seed {path}")),
        )
        .unwrap();
}

fn id_of(engine: &Engine, qname: &str) -> String {
    engine
        .repo
        .get_json(&engine.ref_name, &format!("/asd/v1/index/by-qname/{qname}"))
        .unwrap()["symbol_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn head(engine: &Engine) -> String {
    engine.repo.log(&engine.ref_name, 1).unwrap()[0].id.to_hex()
}

fn list(engine: &Engine, path: &str, field: &str) -> Vec<String> {
    engine.repo.get_json(&engine.ref_name, path).unwrap()[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn repair_fix_drops_every_orphan_in_one_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return b()\n\n\ndef b():\n    return 1\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    run_index(
        &engine.repo,
        &engine.ref_name,
        dir,
        "test",
        &agentstatedeveloper_adapters::default_adapters(),
        None,
        None,
        None,
        Some(&db),
    )
    .unwrap();
    let (a, b) = (id_of(&engine, "m.a"), id_of(&engine, "m.b"));

    // Forty orphaned effects records and one holding runtime evidence,
    // seeded in one write; then a dead id in each edge direction.
    let mut effects = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/effects")
        .unwrap();
    let template = effects[&a].clone();
    let map = effects.as_object_mut().unwrap();
    for n in 0..40 {
        let id = format!("sym_orphan_{n}");
        let mut decl = template.clone();
        decl["symbol_id"] = json!(id);
        map.insert(id, decl);
    }
    let mut traced = template.clone();
    traced["symbol_id"] = json!("sym_traced");
    traced["runtime"] = json!({
        "confirmations": 3,
        "contradictions": 0,
        "prior": 0.5,
        "last_observed_at": "2026-10-01T00:00:00Z"
    });
    map.insert("sym_traced".into(), traced);
    put_json(&engine, "/asd/v1/effects", &effects);
    put_json(
        &engine,
        &paths::callees_path(&a),
        &json!({ "callees": [b.clone(), "sym_dead_callee"] }),
    );
    put_json(
        &engine,
        &paths::callers_path(&b),
        &json!({ "callers": [a.clone(), "sym_dead_caller"] }),
    );

    let before = head(&engine);
    let report = repair_asg(&engine.repo, &engine.ref_name, "test", false).unwrap();

    let log = engine.repo.log(&engine.ref_name, 2).unwrap();
    assert_eq!(
        log[1].id.to_hex(),
        before,
        "repair made more than one commit: {:?}",
        log.iter()
            .map(|c| &c.intent.description)
            .collect::<Vec<_>>()
    );
    assert_eq!(report.fixes_applied, 42);
    let effects = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/effects")
        .unwrap();
    let mut left: Vec<&String> = effects.as_object().unwrap().keys().collect();
    left.sort();
    let mut expected = vec![&a, &b];
    let traced = "sym_traced".to_string();
    expected.push(&traced);
    expected.sort();
    assert_eq!(left, expected);
    assert_eq!(
        list(&engine, &paths::callees_path(&a), "callees"),
        vec![b.clone()]
    );
    assert_eq!(
        list(&engine, &paths::callers_path(&b), "callers"),
        vec![a.clone()]
    );
    // The evidence-bearing record is still reported, but not as fixable.
    let kept: Vec<_> = report
        .issues
        .iter()
        .map(|i| (i.kind.as_str(), i.auto_fixable))
        .collect();
    assert_eq!(kept, vec![("orphaned_effect", false)]);

    // Nothing left to fix: no commit at all.
    let settled = head(&engine);
    let again = repair_asg(&engine.repo, &engine.ref_name, "test", false).unwrap();
    assert_eq!(again.fixes_applied, 0);
    assert_eq!(head(&engine), settled);
}
