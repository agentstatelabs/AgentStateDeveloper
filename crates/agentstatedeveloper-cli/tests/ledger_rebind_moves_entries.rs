//! `asd ledger rebind` moves an orphaned symbol's ledger history onto a live
//! symbol, in one commit, with the entry index and the ledger cache
//! following. It used to look `--from` up by qname — which an orphaned
//! symbol no longer has in the index — so it could not rebind an orphan at
//! all; the MCP tool, which takes the id, left the index and cache on the old
//! id and the `orphaned` tags in place, at two commits per entry.
//!
//! Tagging entries orphaned at the end of `asd index` is one commit too.

use std::path::Path;
use std::process::{Command, Output};

use agentstatedeveloper_core::{
    AsgLedgerStore, Author, AuthorKind, Engine, IndexSummary, LedgerEntry, LedgerKind, LedgerStore,
    run_index, scan_asg,
};
use serde_json::Value;

fn index(engine: &Engine, dir: &Path, db: &Path) -> IndexSummary {
    run_index(
        &engine.repo,
        &engine.ref_name,
        dir,
        "test",
        &agentstatedeveloper_adapters::default_adapters(),
        None,
        None,
        None,
        Some(db),
    )
    .unwrap()
}

fn asd(dir: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_asd"))
        .args(args)
        .arg("--db")
        .arg(db)
        .current_dir(dir)
        .env("ASD_REGISTRY", dir.join("registry.toml"))
        .output()
        .unwrap()
}

fn note(engine: &Engine, symbol_id: &str, summary: &str) -> LedgerEntry {
    let entry = LedgerEntry::new(
        symbol_id,
        LedgerKind::Decision,
        summary,
        Author {
            kind: AuthorKind::Human,
            id: "dev".into(),
        },
    );
    AsgLedgerStore::from_engine(engine)
        .append_entry(&engine.ref_name, &entry, "dev")
        .unwrap();
    entry
}

fn head(engine: &Engine) -> String {
    engine.repo.log(&engine.ref_name, 1).unwrap()[0].id.to_hex()
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

/// Entries stored under `symbol_id`, read from the store itself.
fn stored_under(engine: &Engine, symbol_id: &str) -> Vec<LedgerEntry> {
    match engine
        .repo
        .get_json(&engine.ref_name, &format!("/asd/v1/ledger/{symbol_id}"))
    {
        Ok(Value::Object(map)) => map
            .into_values()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect(),
        _ => Vec::new(),
    }
}

/// The symbol the ledger cache files `entry_id` under, and the tags of its
/// cached copy — read directly, since a read through the store repairs a
/// stale row on the way.
fn cached(db: &Path, entry_id: &str) -> (String, Vec<String>) {
    let c = rusqlite::Connection::open(db).unwrap();
    let (symbol_id, body): (String, String) = c
        .query_row(
            "SELECT symbol_id, body FROM asd_ledger_cache WHERE entry_id = ?1",
            [entry_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let entry: LedgerEntry = serde_json::from_str(&body).unwrap();
    (symbol_id, entry.tags)
}

fn is_orphan_tag(t: &String) -> bool {
    t == "orphaned" || t.starts_with("orphaned-at:")
}

/// The commit the orphan-tagging pass makes ("tag N orphaned …" and/or
/// "clear orphan tags from N …").
fn is_tagging_commit(description: &str) -> bool {
    description.starts_with("tag ") || description.starts_with("clear orphan tags")
}

fn set_by_qname(engine: &Engine, tree: &Value, why: &str) {
    engine
        .repo
        .set_json(
            &engine.ref_name,
            "/asd/v1/index/by-qname",
            tree,
            agentstategraph::CommitOptions::new(
                "t",
                agentstategraph_core::IntentCategory::Refine,
                why.to_string(),
            ),
        )
        .unwrap();
}

/// Leave `qname`'s entries tagged orphaned while the symbol is in the index
/// — the state stores indexed before v1.4.6 could be left in, and that an
/// index run now clears. The symbol drops out of the index long enough to be
/// tagged, then its entry is put back as it was.
fn tag_as_orphan_and_restore(engine: &Engine, qname: &str) {
    // `orphaned-at:` has one-second resolution: tag strictly after
    // `created_at`, or the entry's own creation time decides any race.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let by_qname = engine
        .repo
        .get_tree(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap();
    let mut without = by_qname.clone();
    without.as_object_mut().unwrap().remove(qname);
    set_by_qname(engine, &without, &format!("drop {qname}"));
    agentstatedeveloper_core::detect_orphaned_entries(&engine.repo, &engine.ref_name, "t").unwrap();
    set_by_qname(engine, &by_qname, &format!("restore {qname}"));
}

/// A project with live symbols `m.a` and `m.b`, and ledger entries filed
/// under two ids nothing indexes — tagged orphaned by the second index run.
struct Fixture {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    db: std::path::PathBuf,
    gone1: Vec<LedgerEntry>,
    gone2: LedgerEntry,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef b():\n    return 2\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, &dir, &db);
    let gone1 = vec![
        note(&engine, "sym_gone1", "first call on the old symbol"),
        note(&engine, "sym_gone1", "second call on the old symbol"),
    ];
    let gone2 = note(&engine, "sym_gone2", "the other old symbol");
    index(&engine, &dir, &db);
    Fixture {
        _tmp: tmp,
        dir,
        db,
        gone1,
        gone2,
    }
}

#[test]
fn tagging_orphans_is_one_commit_and_reaches_the_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let db = dir.join(".asd-state.db");
    std::fs::write(dir.join("m.py"), "def a():\n    return 1\n").unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, dir, &db);
    let entries: Vec<LedgerEntry> = (0..5)
        .map(|n| note(&engine, &format!("sym_gone{}", n % 2), &format!("note {n}")))
        .collect();
    let before = head(&engine);

    let summary = index(&engine, dir, &db);

    assert_eq!(summary.orphaned_tagged, 5);
    let mut tag_commits = 0;
    for commit in engine.repo.log(&engine.ref_name, 50).unwrap() {
        if commit.id.to_hex() == before {
            break;
        }
        if commit.intent.description.starts_with("tag ") {
            tag_commits += 1;
        }
    }
    assert_eq!(tag_commits, 1, "one commit per tagged entry");
    for e in &entries {
        let (_, tags) = cached(&db, &e.entry_id);
        assert!(
            tags.iter().any(is_orphan_tag),
            "the cached copy of {} was not tagged",
            e.entry_id
        );
    }
    // Already-tagged entries are left alone: no commit at all.
    let settled = head(&engine);
    assert_eq!(index(&engine, dir, &db).orphaned_tagged, 0);
    let after: Vec<String> = engine
        .repo
        .log(&engine.ref_name, 10)
        .unwrap()
        .iter()
        .take_while(|c| c.id.to_hex() != settled)
        .map(|c| c.intent.description.clone())
        .collect();
    assert!(
        !after.iter().any(|d| d.starts_with("tag ")),
        "re-tagged: {after:?}"
    );
}

#[test]
fn rebind_from_an_orphaned_id_moves_its_entries() {
    let f = fixture();
    let before = {
        let engine = Engine::open_sqlite(&f.db).unwrap();
        let (_, tags) = cached(&f.db, &f.gone1[0].entry_id);
        assert!(tags.iter().any(is_orphan_tag), "precondition: tagged");
        head(&engine)
    };

    let out = asd(
        &f.dir,
        &f.db,
        &["ledger", "rebind", "--from", "sym_gone1", "--to", "m.a"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["entries_moved"], 2);

    let engine = Engine::open_sqlite(&f.db).unwrap();
    let a = id_of(&engine, "m.a");
    assert_eq!(report["to_symbol_id"], a.as_str());
    let log = engine.repo.log(&engine.ref_name, 2).unwrap();
    assert_eq!(log[1].id.to_hex(), before, "more than one commit");

    // The caches first: a read through the store repairs a stale row.
    for e in &f.gone1 {
        let (symbol_id, tags) = cached(&f.db, &e.entry_id);
        assert_eq!(
            symbol_id, a,
            "the cache still files {} under the old id",
            e.entry_id
        );
        assert!(
            !tags.iter().any(is_orphan_tag),
            "cached copy still orphaned"
        );
    }
    assert!(
        stored_under(&engine, "sym_gone1").is_empty(),
        "entries left under the old id"
    );
    let moved = stored_under(&engine, &a);
    assert_eq!(moved.len(), 2);
    for e in &moved {
        assert_eq!(e.symbol_id, a);
        assert!(!e.tags.iter().any(is_orphan_tag), "{:?}", e.tags);
        assert!(e.tags.iter().any(|t| t.starts_with("rebound-at:")));
        assert_eq!(
            engine
                .repo
                .get_json(
                    &engine.ref_name,
                    &format!("/asd/v1/ledger-idx/{}", e.entry_id)
                )
                .unwrap(),
            Value::String(a.clone())
        );
    }
    let record = engine
        .repo
        .get_json(&engine.ref_name, "/asd/v1/rebinds/sym_gone1")
        .unwrap();
    assert_eq!(record["to_qname"], "m.a");
    let orphaned: Vec<String> = scan_asg(&engine.repo, &engine.ref_name)
        .unwrap()
        .into_iter()
        .filter(|i| i.kind == "orphaned_ledger")
        .map(|i| i.path)
        .collect();
    assert_eq!(orphaned, vec!["/asd/v1/ledger/sym_gone2".to_string()]);
}

#[test]
fn a_rebind_map_is_checked_whole_then_applied_in_one_commit() {
    let f = fixture();
    let map = f.dir.join("rebinds.json");
    let before = head(&Engine::open_sqlite(&f.db).unwrap());

    std::fs::write(&map, r#"{"sym_gone1": "m.a", "sym_gone2": "m.nope"}"#).unwrap();
    let out = asd(
        &f.dir,
        &f.db,
        &["ledger", "rebind", "--map", map.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("m.nope"));
    {
        let engine = Engine::open_sqlite(&f.db).unwrap();
        assert_eq!(head(&engine), before, "a failed map wrote something");
        assert_eq!(stored_under(&engine, "sym_gone1").len(), 2);
    }

    std::fs::write(&map, r#"{"sym_gone1": "m.a", "sym_gone2": "m.b"}"#).unwrap();
    let out = asd(
        &f.dir,
        &f.db,
        &["ledger", "rebind", "--map", map.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["entries_moved"], 3);
    assert_eq!(report["rebinds"].as_array().unwrap().len(), 2);

    let engine = Engine::open_sqlite(&f.db).unwrap();
    let log = engine.repo.log(&engine.ref_name, 2).unwrap();
    assert_eq!(log[1].id.to_hex(), before, "more than one commit");
    assert_eq!(stored_under(&engine, &id_of(&engine, "m.a")).len(), 2);
    let b = stored_under(&engine, &id_of(&engine, "m.b"));
    assert_eq!(
        b.iter().map(|e| e.entry_id.as_str()).collect::<Vec<_>>(),
        vec![f.gone2.entry_id.as_str()]
    );
    assert!(
        !scan_asg(&engine.repo, &engine.ref_name)
            .unwrap()
            .iter()
            .any(|i| i.kind == "orphaned_ledger")
    );
}

#[test]
fn rebind_from_an_id_with_nothing_filed_fails_without_writing() {
    let f = fixture();
    let before = head(&Engine::open_sqlite(&f.db).unwrap());
    let out = asd(
        &f.dir,
        &f.db,
        &["ledger", "rebind", "--from", "sym_typo", "--to", "m.a"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sym_typo"));
    assert_eq!(head(&Engine::open_sqlite(&f.db).unwrap()), before);
}

/// The post-merge and post-checkout hooks run `conclusions import`, which
/// keeps whichever copy of an entry was revised last, judged by its `*-at:`
/// tags. An entry tagged orphaned whose symbol later came back is exported
/// with `orphaned-at:`; once a rebind drops that tag, the exported copy must
/// not count as newer and move the entry back.
#[test]
fn importing_conclusions_exported_before_a_rebind_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef c():\n    return 3\n",
    )
    .unwrap();
    let entry_id = {
        let engine = Engine::open_sqlite(&db).unwrap();
        index(&engine, &dir, &db);
        let c = id_of(&engine, "m.c");
        let entry = note(&engine, &c, "c is load-bearing");
        tag_as_orphan_and_restore(&engine, "m.c");
        entry.entry_id
    };
    let out_dir = dir.join("exported");
    let export = asd(
        &dir,
        &db,
        &["conclusions", "export", "--out", out_dir.to_str().unwrap()],
    );
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let exported = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap_or_default())
        .collect::<String>();
    assert!(
        exported.contains(&entry_id) && exported.contains("orphaned-at:"),
        "precondition: the export carries the orphan-tagged copy"
    );

    let rebind = asd(
        &dir,
        &db,
        &["ledger", "rebind", "--from", "m.c", "--to", "m.a"],
    );
    assert!(
        rebind.status.success(),
        "{}",
        String::from_utf8_lossy(&rebind.stderr)
    );
    let import = asd(
        &dir,
        &db,
        &[
            "conclusions",
            "import",
            "--in-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    assert!(
        import.status.success(),
        "{}",
        String::from_utf8_lossy(&import.stderr)
    );

    let engine = Engine::open_sqlite(&db).unwrap();
    assert!(
        stored_under(&engine, &id_of(&engine, "m.c")).is_empty(),
        "the import put the entry back under m.c"
    );
    let a = stored_under(&engine, &id_of(&engine, "m.a"));
    assert_eq!(
        a.iter().map(|e| e.entry_id.as_str()).collect::<Vec<_>>(),
        vec![entry_id.as_str()]
    );
    assert!(!a[0].tags.iter().any(is_orphan_tag));
}

/// A symbol whose entries were tagged orphaned and that is back in the index
/// — SessionDrift-ios's branch-only symbols on a checkout of that branch —
/// loses the tags on the next index run, in its one tagging commit.
#[test]
fn an_index_run_clears_orphan_tags_when_the_symbol_is_back() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef c():\n    return 3\n",
    )
    .unwrap();
    let engine = Engine::open_sqlite(&db).unwrap();
    index(&engine, &dir, &db);
    let c = id_of(&engine, "m.c");
    let entry = note(&engine, &c, "c is load-bearing");
    let gone = note(&engine, "sym_gone", "on a symbol that stays gone");
    tag_as_orphan_and_restore(&engine, "m.c");
    // A store left like this by an earlier version has the tags cached too.
    let tagged = stored_under(&engine, &c).remove(0);
    agentstatedeveloper_core::SearchFtsDb::open(&db)
        .unwrap()
        .upsert_ledger_entry(&tagged, &engine.ref_name)
        .unwrap();
    assert!(
        cached(&db, &entry.entry_id).1.iter().any(is_orphan_tag),
        "precondition: tagged while live, in the store and the cache"
    );
    let before = head(&engine);

    let summary = index(&engine, &dir, &db);

    assert_eq!((summary.orphaned_untagged, summary.orphaned_tagged), (1, 0));
    let (symbol_id, tags) = cached(&db, &entry.entry_id);
    assert_eq!(symbol_id, c);
    assert!(!tags.iter().any(is_orphan_tag), "cached copy still tagged");
    let stored = &stored_under(&engine, &c)[0];
    assert!(!stored.tags.iter().any(is_orphan_tag), "{:?}", stored.tags);
    assert!(stored.tags.iter().any(|t| t.starts_with("reattached-at:")));
    // The entry whose symbol is still gone keeps its tags.
    assert!(
        stored_under(&engine, "sym_gone")[0]
            .tags
            .iter()
            .any(is_orphan_tag)
    );
    let _ = gone;
    let tag_commits = engine
        .repo
        .log(&engine.ref_name, 50)
        .unwrap()
        .into_iter()
        .take_while(|c| c.id.to_hex() != before)
        .filter(|c| is_tagging_commit(&c.intent.description))
        .count();
    assert_eq!(tag_commits, 1);

    // Settled: nothing left to change, so no commit.
    let settled = head(&engine);
    let again = index(&engine, &dir, &db);
    assert_eq!((again.orphaned_untagged, again.orphaned_tagged), (0, 0));
    assert!(
        !engine
            .repo
            .log(&engine.ref_name, 10)
            .unwrap()
            .into_iter()
            .take_while(|c| c.id.to_hex() != settled)
            .any(|c| is_tagging_commit(&c.intent.description)),
        "a settled index touched orphan tags"
    );
}

/// A copy exported while the entry was still tagged must not tag it again on
/// the next `conclusions import` (the post-merge and post-checkout hooks).
#[test]
fn importing_a_copy_exported_while_tagged_keeps_the_tags_cleared() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join(".asd-state.db");
    std::fs::write(
        dir.join("m.py"),
        "def a():\n    return 1\n\n\ndef c():\n    return 3\n",
    )
    .unwrap();
    let c = {
        let engine = Engine::open_sqlite(&db).unwrap();
        index(&engine, &dir, &db);
        let c = id_of(&engine, "m.c");
        note(&engine, &c, "c is load-bearing");
        tag_as_orphan_and_restore(&engine, "m.c");
        c
    };
    let out_dir = dir.join("exported");
    let export = asd(
        &dir,
        &db,
        &["conclusions", "export", "--out", out_dir.to_str().unwrap()],
    );
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let exported = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap_or_default())
        .collect::<String>();
    assert!(
        exported.contains("orphaned-at:"),
        "precondition: the export carries the tagged copy"
    );

    {
        let engine = Engine::open_sqlite(&db).unwrap();
        assert_eq!(index(&engine, &dir, &db).orphaned_untagged, 1);
    }
    let import = asd(
        &dir,
        &db,
        &[
            "conclusions",
            "import",
            "--in-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    assert!(
        import.status.success(),
        "{}",
        String::from_utf8_lossy(&import.stderr)
    );

    let engine = Engine::open_sqlite(&db).unwrap();
    let stored = stored_under(&engine, &c);
    assert_eq!(stored.len(), 1);
    assert!(
        !stored[0].tags.iter().any(is_orphan_tag),
        "the import tagged it again: {:?}",
        stored[0].tags
    );
}
