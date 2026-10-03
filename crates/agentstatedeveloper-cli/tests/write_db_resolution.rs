//! Commands that pair a sidecar directory with a store must never pair it
//! with someone else's store.
//!
//! Without `--db`, read commands fall back to a walk-up parent and then the
//! registry's active repo. `hydrate` used to share that fallback: run in a
//! directory holding only a copied `.asd/` sidecar, it loaded one project's
//! 12,003 symbols and 12,085 ledger entries into the registry's active repo —
//! a different project — and re-warmed that store's caches. `sync` had the
//! mirror-image fault: it exported a fallback store into whatever directory
//! it ran from.
//!
//! Every test here sandboxes `HOME` and `ASD_REGISTRY` and registers a decoy
//! "active repo"; none may move the decoy's head.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentstatedeveloper_core::{
    AsgIndexStore, AsgLedgerStore, Author, AuthorKind, Engine, IndexStore, LedgerEntry, LedgerKind,
    LedgerStore, Position, Symbol, SymbolKind, ledger_integrity::asg_ledger_entry_ids, sync_to_dir,
};
use tempfile::TempDir;

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

/// A sandbox with a registry whose active repo is a decoy store holding one
/// symbol and one ledger entry.
struct Sandbox {
    root: TempDir,
    decoy_db: PathBuf,
    registry: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let decoy = root.path().join("decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        let decoy_db = decoy.join(".asd-state.db");
        seed(&decoy_db, "sym_decoy", 1);

        let registry = root.path().join("repos.toml");
        std::fs::write(
            &registry,
            format!(
                "[active]\nrepo = \"decoy\"\n\n[repos.decoy]\npath = \"{}\"\n",
                decoy_db.display()
            ),
        )
        .unwrap();
        Self {
            root,
            decoy_db,
            registry,
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let d = self.root.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Every ref and the commit count of the decoy, read through a read-only
    /// connection: opening an `Engine` can itself write (it auto-hydrates a
    /// store without a symbol index when a sidecar sits beside it), which
    /// would make the canary report its own reads.
    fn decoy_state(&self) -> (Vec<(String, String, Vec<u8>)>, i64) {
        let c = rusqlite::Connection::open_with_flags(
            &self.decoy_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = c
            .prepare("SELECT namespace, name, target FROM refs ORDER BY namespace, name")
            .unwrap();
        let refs = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let commits = c
            .query_row("SELECT COUNT(*) FROM commits", [], |r| r.get(0))
            .unwrap();
        (refs, commits)
    }

    /// Run `asd` from `cwd` with no `--db`, so resolution is left to defaults.
    fn asd(&self, cwd: &Path, args: &[&str]) -> Output {
        let out = Command::new(asd_bin())
            .current_dir(cwd)
            .args(args)
            .env("HOME", self.root.path())
            .env("ASD_REGISTRY", &self.registry)
            .env_remove("ASD_DB")
            .output()
            .expect("spawn asd");
        assert!(
            out.status.success(),
            "asd {args:?} failed\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

/// Create an indexed store at `db` — one symbol with `n` ledger entries —
/// like a real repo after `asd index`; returns the entry ids.
fn seed(db: &Path, symbol: &str, n: usize) -> Vec<String> {
    let engine = Engine::open_sqlite(db).unwrap();
    AsgIndexStore::from_engine(&engine)
        .put_symbol(
            &engine.ref_name,
            &Symbol {
                symbol_id: symbol.into(),
                symbol_fp: format!("fp-{symbol}"),
                qname: format!("pkg.{symbol}"),
                language: "python".into(),
                kind: SymbolKind::Function,
                file: "pkg/mod.py".into(),
                start: Position { line: 1, col: 0 },
                end: Position { line: 2, col: 0 },
                signature: None,
                doc: None,
            },
            "tester",
        )
        .unwrap();
    let ledger = AsgLedgerStore::from_engine(&engine);
    (0..n)
        .map(|i| {
            let e = LedgerEntry::new(
                symbol,
                LedgerKind::Decision,
                format!("{symbol} decision {i}"),
                Author {
                    kind: AuthorKind::Human,
                    id: "tester".into(),
                },
            );
            ledger.append_entry(&engine.ref_name, &e, "tester").unwrap();
            e.entry_id
        })
        .collect()
}

/// A directory holding only a sidecar (no store), as after copying `.asd/`
/// or cloning a repo. Returns the entry ids the sidecar carries.
fn sidecar_only_dir(sb: &Sandbox, name: &str) -> (PathBuf, Vec<String>) {
    let src = sb.dir(&format!("{name}-src"));
    let src_db = src.join(".asd-state.db");
    let ids = seed(&src_db, "sym_proj", 3);
    let dir = sb.dir(name);
    let engine = Engine::open_sqlite(&src_db).unwrap();
    sync_to_dir(&engine.repo, &engine.ref_name, &dir).unwrap();
    assert!(!dir.join(".asd-state.db").exists());
    (dir, ids)
}

fn ledger_ids(db: &Path) -> Vec<String> {
    let e = Engine::open_sqlite(db).unwrap();
    asg_ledger_entry_ids(&e.repo, &e.ref_name)
        .into_iter()
        .collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn hydrate_in_a_sidecar_only_dir_fills_a_store_there_not_the_active_repo() {
    let sb = Sandbox::new();
    let before = sb.decoy_state();
    let (dir, ids) = sidecar_only_dir(&sb, "clone");

    let out = sb.asd(&dir, &["hydrate"]);

    assert_eq!(
        sb.decoy_state(),
        before,
        "hydrate wrote into the registry's active repo"
    );
    let local = dir.join(".asd-state.db");
    assert!(
        local.exists(),
        "hydrate did not create the store beside the sidecar"
    );
    assert_eq!(sorted(ledger_ids(&local)), sorted(ids));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        json["db"].as_str().unwrap().ends_with(".asd-state.db"),
        "hydrate output must name the store it filled: {json}"
    );
}

#[test]
fn hydrate_dir_without_db_fills_the_store_beside_that_sidecar() {
    let sb = Sandbox::new();
    let before = sb.decoy_state();
    let (dir, ids) = sidecar_only_dir(&sb, "clone");
    let elsewhere = sb.dir("elsewhere");

    sb.asd(&elsewhere, &["hydrate", "--dir", dir.to_str().unwrap()]);

    assert_eq!(
        sb.decoy_state(),
        before,
        "hydrate wrote into the registry's active repo"
    );
    assert!(
        !elsewhere.join(".asd-state.db").exists(),
        "hydrate --dir filled a store in the directory it ran from"
    );
    assert_eq!(sorted(ledger_ids(&dir.join(".asd-state.db"))), sorted(ids));
}

#[test]
fn sync_from_a_subdirectory_writes_the_sidecar_beside_the_store() {
    let sb = Sandbox::new();
    let project = sb.dir("project");
    seed(&project.join(".asd-state.db"), "sym_proj", 2);
    let sub = sb.dir("project/src/deep");

    sb.asd(&sub, &["sync"]);

    assert!(
        project.join(".asd/v1/ledger").is_dir(),
        "sync did not write the sidecar at the project root"
    );
    assert!(
        !sub.join(".asd").exists(),
        "sync exported the project's store into a subdirectory"
    );
}

#[test]
fn sync_outside_any_project_never_exports_the_active_repo_into_the_cwd() {
    let sb = Sandbox::new();
    let before = sb.decoy_state();
    let stray = sb.dir("stray");

    sb.asd(&stray, &["sync"]);

    assert!(
        !stray.join(".asd").exists(),
        "sync exported the registry's active repo into an unrelated directory"
    );
    assert_eq!(sb.decoy_state(), before);
}

/// `asd index <project>` run from the project's parent directory used
/// `./.asd-state.db`: it built and registered a new store in the parent and
/// left the project's own store untouched (2026-10-02, `Apps/` and
/// SessionDrift-ios).
#[test]
fn index_from_a_parent_directory_uses_the_projects_store() {
    let sb = Sandbox::new();
    let parent = sb.dir("apps");
    let project = parent.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("rates.py"),
        "def fetch_rates():\n    return 1\n",
    )
    .unwrap();
    let project_db = project.join(".asd-state.db");
    seed(&project_db, "sym_existing", 1);

    let out = sb.asd(&parent, &["index", "proj"]);

    assert!(
        !parent.join(".asd-state.db").exists(),
        "index built a stray store in the parent"
    );
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        Path::new(summary["db"].as_str().unwrap()),
        project_db.canonicalize().unwrap(),
        "{summary}"
    );
    let engine = Engine::open_sqlite(&project_db).unwrap();
    let qnames = engine
        .repo
        .get_json(&engine.ref_name, "/asd/v1/index/by-qname")
        .unwrap();
    assert!(
        qnames
            .as_object()
            .unwrap()
            .keys()
            .any(|q| q.ends_with("fetch_rates")),
        "the project's store was not indexed: {qnames}"
    );
}
