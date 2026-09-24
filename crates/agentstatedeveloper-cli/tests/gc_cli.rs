//! `asd gc` end to end, through the real binary.
//!
//! The properties that matter to a user running it by hand: the default only
//! previews and deletes nothing; `--sweep` deletes what the preview said it
//! would; the live state survives; and the flags are wired through.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::Engine;
use agentstategraph::CommitOptions;
use agentstategraph_core::IntentCategory;

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

/// A store with history to reclaim: one value overwritten several times.
fn seed(db: &Path) {
    let engine = Engine::open_sqlite(db).expect("open store");
    for n in 0..6 {
        engine
            .repo
            .set_json(
                &engine.ref_name,
                "/test/value",
                &serde_json::json!({ "n": n }),
                CommitOptions::new("alice", IntentCategory::Refine, format!("edit {n}")),
            )
            .expect("commit");
    }
}

/// Run `asd --db <db> gc <args>`, returning (success, stdout JSON, stderr).
fn gc(db: &Path, args: &[&str]) -> (bool, serde_json::Value, String) {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db)
        .arg("gc")
        .args(args)
        .output()
        .expect("run asd");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| serde_json::json!({ "_raw": stdout.to_string() }));
    (
        out.status.success(),
        json,
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn gc_previews_by_default_and_sweeps_only_when_asked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = tmp.path().join(".asd-state.db");
    seed(&db);

    // Default: a preview. It reports reclaimable objects and deletes none.
    let (ok, preview, stderr) = gc(&db, &["--keep-recent", "1"]);
    assert!(ok, "{preview} {stderr}");
    assert_eq!(preview["result"]["dry_run"], true, "{preview}");
    assert_eq!(
        preview["result"]["policy"]["checkpoint_every"], 0,
        "{preview}"
    );
    let total = preview["result"]["would_reclaim"]["total_objects"]
        .as_i64()
        .unwrap();
    let reclaimable = preview["result"]["would_reclaim"]["reclaimable_objects"]
        .as_i64()
        .unwrap();
    assert!(
        reclaimable > 0,
        "overwritten values must be reclaimable: {preview}"
    );
    assert!(stderr.contains("Nothing was deleted"), "{stderr}");

    let (_, again, _) = gc(&db, &["--keep-recent", "1"]);
    assert_eq!(
        again["result"]["would_reclaim"]["total_objects"], total,
        "a preview must not delete anything"
    );

    // --sweep deletes exactly what the preview predicted; --vacuum and
    // --unpin-legacy are wired through (a store written by v1.2.2+ has no
    // legacy pins, so the count is zero).
    let (ok, swept, stderr) = gc(
        &db,
        &[
            "--keep-recent",
            "1",
            "--sweep",
            "--vacuum",
            "--unpin-legacy",
        ],
    );
    assert!(ok, "{swept} {stderr}");
    assert_eq!(swept["result"]["mutated"], true, "{swept}");
    assert_eq!(
        swept["result"]["objects_deleted"].as_i64().unwrap(),
        reclaimable,
        "the sweep must delete what the preview said it would: {swept}"
    );
    assert!(swept["result"].get("vacuum").is_some(), "{swept}");
    assert_eq!(swept["unpinned_legacy_milestones"], 0, "{swept}");

    // And the live state survived.
    let engine = Engine::open_sqlite(&db).expect("reopen");
    assert_eq!(
        engine
            .repo
            .get_json(&engine.ref_name, "/test/value")
            .unwrap(),
        serde_json::json!({ "n": 5 })
    );
    let head = engine.repo.head(&engine.ref_name).unwrap();
    assert_eq!(
        engine.repo.first_missing_object(&head).unwrap(),
        None,
        "the sweep must leave the live state fully readable"
    );
}
