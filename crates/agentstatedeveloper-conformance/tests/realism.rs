//! Tier-2 realism: run the full adapter pipeline over REAL source trees and
//! assert only coarse invariants — nothing panics, aggregate counts clear a
//! floor. This is the layer that catches what synthetic fixtures can't: the
//! incidental mess of code we didn't write.
//!
//! These tests are `#[ignore]`d so they stay out of the fast every-commit
//! path. Run them in nightly CI (or locally) with:
//!
//! ```text
//! cargo test -p agentstatedeveloper-conformance -- --ignored --nocapture
//! ```
//!
//! - `realism_asd_self` always runs (ASD's own source tree — a large, real
//!   Rust workspace; zero network).
//! - `realism_external_corpus` runs over a polyglot corpus pointed to by the
//!   `ASD_REALISM_CORPUS` env var; it SKIPS (not fails) when unset. Populate
//!   the corpus with `scripts/fetch-realism-corpus.sh`.

use std::path::PathBuf;

use agentstatedeveloper_conformance::{run_pipeline_over_tree, TreeStats};

/// Workspace root, resolved from the compile-time crate dir — never CWD
/// (CLAUDE.md: CWD-relative paths break from non-source checkouts).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn report(label: &str, s: &TreeStats) {
    eprintln!("\n=== realism: {label} ===");
    eprintln!("  files_parsed       {}", s.files_parsed);
    eprintln!("  symbols            {}", s.symbols);
    eprintln!("  files_with_effects {}", s.files_with_effects);
    eprintln!("  call_edges         {}", s.call_edges);
    eprintln!("  inbound_endpoints  {}", s.inbound_endpoints);
    eprintln!("  outbound_endpoints {}", s.outbound_endpoints);
    eprintln!("  languages          {:?}", s.by_language);
    if !s.panicked_files.is_empty() {
        eprintln!("  !! PANICKED FILES ({}):", s.panicked_files.len());
        for f in &s.panicked_files {
            eprintln!("     {f}");
        }
    }
}

/// The pipeline must survive ASD's own source tree (real, large, messy Rust)
/// without panicking, and produce non-trivial structure. Floors are set well
/// below observed values so normal growth/churn never trips them — the test
/// guards against collapse-to-zero and panics, not exact counts.
#[test]
#[ignore = "tier-2 realism (slow); run with --ignored"]
fn realism_asd_self() {
    let s = run_pipeline_over_tree(&workspace_root());
    report("ASD self (Rust)", &s);

    assert!(
        s.panicked_files.is_empty(),
        "pipeline panicked on {} real file(s):\n{:#?}",
        s.panicked_files.len(),
        s.panicked_files
    );
    assert!(s.files_parsed >= 100, "expected ≥100 files, got {}", s.files_parsed);
    assert!(s.symbols >= 1000, "expected ≥1000 symbols, got {}", s.symbols);
    assert!(s.call_edges >= 100, "expected ≥100 call edges, got {}", s.call_edges);
    assert!(
        s.files_with_effects >= 5,
        "expected ≥5 files with effects, got {}",
        s.files_with_effects
    );
}

/// Polyglot realism over a corpus of real third-party repos. Skips cleanly
/// when the corpus isn't present so it never blocks a local run.
#[test]
#[ignore = "tier-2 realism (slow, needs ASD_REALISM_CORPUS); run with --ignored"]
fn realism_external_corpus() {
    let Ok(dir) = std::env::var("ASD_REALISM_CORPUS") else {
        eprintln!(
            "realism_external_corpus: ASD_REALISM_CORPUS unset — skipping.\n\
             Populate it with scripts/fetch-realism-corpus.sh, then re-run."
        );
        return;
    };
    let root = PathBuf::from(&dir);
    assert!(root.is_dir(), "ASD_REALISM_CORPUS={dir} is not a directory");

    let s = run_pipeline_over_tree(&root);
    report(&format!("external corpus ({dir})"), &s);

    // The only hard invariant we can assert without knowing the corpus
    // contents: no panics, and SOME real code was actually exercised.
    assert!(
        s.panicked_files.is_empty(),
        "pipeline panicked on {} real file(s):\n{:#?}",
        s.panicked_files.len(),
        s.panicked_files
    );
    assert!(s.files_parsed > 0, "no recognized source files under {dir}");
    assert!(s.symbols > 0, "parsed {} files but found no symbols under {dir}", s.files_parsed);
}
