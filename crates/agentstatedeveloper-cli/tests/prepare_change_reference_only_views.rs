//! Regression coverage for prepare-change reference-only recipes.
//!
//! The classifier can correctly mark view/surface files as reference-only while
//! the final recipe accidentally drops them. Keep this test focused on the JSON
//! contract agents consume: demoted view files must appear in
//! `safe_change_recipe.reference_only`, not only in `classification_debug`.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentstatedeveloper_core::{
    AsgIndexStore, Engine, IndexStore, Position, SearchFtsDb, Symbol, SymbolKind,
};

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

fn mk_sym(id: &str, qname: &str, file: &str, line: u32) -> Symbol {
    Symbol {
        symbol_id: id.into(),
        symbol_fp: format!("fp-{id}"),
        qname: qname.into(),
        language: "swift".into(),
        kind: SymbolKind::Function,
        file: file.into(),
        start: Position { line, col: 0 },
        end: Position {
            line: line + 4,
            col: 0,
        },
        signature: Some(format!(
            "func {}()",
            qname.rsplit('.').next().unwrap_or(qname)
        )),
        doc: Some(format!("Symbol {qname}")),
    }
}

fn seed(db: &Path) {
    let engine = Engine::open_sqlite(db).expect("open sqlite");
    let idx = AsgIndexStore::from_engine(&engine);
    let symbols = vec![
        mk_sym(
            "active_drift_clip",
            "App.AcmeFlow.AcmeFlowViewModel.activeDriftClipForPlayhead",
            "App/AcmeFlow/AcmeFlowApp.swift",
            40,
        ),
        mk_sym(
            "sheet_music_playhead",
            "App.AcmeFlow.Views.SheetMusicView.SheetMusicView.localSchedulerPlayheadClip",
            "App/AcmeFlow/Views/SheetMusicView.swift",
            90,
        ),
        mk_sym(
            "waveform_canvas",
            "App.AcmeFlow.Views.WaveformCanvas.WaveformCanvas",
            "App/AcmeFlow/Views/WaveformCanvas.swift",
            5,
        ),
    ];
    for sym in &symbols {
        idx.put_symbol(&engine.ref_name, sym, "t").unwrap();
    }
    SearchFtsDb::open(db)
        .unwrap()
        .rebuild(&symbols)
        .expect("rebuild fts");
}

fn run_prepare_change(db: &Path, description: &str) -> serde_json::Value {
    let out = Command::new(asd_bin())
        .arg("--db")
        .arg(db)
        .arg("prepare-change")
        .arg(description)
        .arg("--agent")
        .arg("--debug-classification")
        .output()
        .expect("spawn asd");
    assert!(
        out.status.success(),
        "prepare-change failed:\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "non-JSON stdout: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn recipe_reference_files(v: &serde_json::Value) -> Vec<String> {
    v["safe_change_recipe"]["reference_only"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["file"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn anchor_missing_view_files_are_in_reference_only_recipe() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);

    let v = run_prepare_change(&db, "drift playhead clip local scheduler");
    let files = recipe_reference_files(&v);

    assert!(
        files.iter().any(|f| f.contains("SheetMusicView.swift")),
        "SheetMusicView was classified as reference-only but missing from recipe: {v:#?}"
    );
}

#[test]
fn rendering_surfaces_are_in_reference_only_recipe() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    seed(&db);

    let v = run_prepare_change(&db, "drift canvas waveform");
    let files = recipe_reference_files(&v);

    assert!(
        files.iter().any(|f| f.contains("WaveformCanvas.swift")),
        "WaveformCanvas was classified as reference-only but missing from recipe: {v:#?}"
    );
    let rationale = v["safe_change_recipe"]["reference_only"]
        .as_array()
        .and_then(|items| {
            items.iter().find_map(|item| {
                item["file"]
                    .as_str()
                    .filter(|file| file.contains("WaveformCanvas.swift"))
                    .and_then(|_| item["rationale"].as_str())
            })
        })
        .unwrap_or("");
    assert!(
        rationale.contains("rendering surface"),
        "WaveformCanvas reference entry must explain the surface demotion; got {rationale:?}"
    );
}
