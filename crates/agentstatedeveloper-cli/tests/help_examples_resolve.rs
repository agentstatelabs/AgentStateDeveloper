//! Plan J t-018: command examples in user-facing output must
//! actually resolve.
//!
//! Two consecutive field-test catches on ExampleProj in one session
//! triggered this:
//!   - `asd think bootstrap` told users to run `asd reindex` —
//!     no such CLI subcommand until 1.0.62 added the alias.
//!   - `commands/think.rs:283` referenced a CWD-relative path to
//!     docs/initial-read-prompt.md that failed from any non-source
//!     checkout (fixed 1.0.61 via include_str!).
//!
//! Pattern: command examples baked into help text, JSON output,
//! and bootstrap checklists silently drift from reality. This
//! test extracts every `` `asd <subcommand> ...` `` pattern from
//! a handful of user-facing surfaces and asserts each subcommand
//! resolves via `asd <subcommand> --help` (cheap, side-effect-free).
//!
//! Scope:
//!   - CLI surfaces: --help (long_about), bootstrap commands
//!   - Future: extend to DESIGN.md + CHANGELOG.md backtick blocks
//!     (filed as a stretch goal in t-018's DESIGN entry)
//!
//! Allow-list for placeholders: any extracted token containing
//! `<` (e.g. `<dir>`, `<NAME>`, `<QN>`) or `[` (clap usage
//! `[OPTIONS]`) is skipped — those are syntax templates, not
//! literal subcommands.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

fn asd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_asd"))
}

/// Extract `asd <subcommand>` references from text that contains
/// backtick-quoted command examples. Returns the unique set of
/// subcommand tokens (the word right after `asd `).
///
/// Examples this should extract:
///   `asd think prompt`             → "think"
///   `asd index .` (or `asd reindex .`) → "index", "reindex"
///   `asd search "x" --agent`       → "search"
///
/// Examples this should SKIP:
///   `asd <subcommand>`             → contains `<` → placeholder
///   `asd --help`                   → starts with `-` → flag, not subcommand
///   `asd [OPTIONS]`                → contains `[` → clap template
fn extract_asd_subcommands(text: &str) -> HashSet<String> {
    let mut found: HashSet<String> = HashSet::new();
    // Walk every backtick-quoted span.
    for span in text.split('`').enumerate().filter_map(|(i, s)| {
        // Even indices are OUTSIDE backticks; odd indices are inside.
        if i % 2 == 1 { Some(s) } else { None }
    }) {
        let trimmed = span.trim();
        // Must start with `asd ` followed by a word.
        let rest = match trimmed.strip_prefix("asd ") {
            Some(r) => r.trim_start(),
            None => continue,
        };
        // First token after `asd ` is the subcommand candidate.
        let token = match rest.split_whitespace().next() {
            Some(t) => t,
            None => continue,
        };
        // Skip placeholders, flags, and clap usage templates.
        if token.starts_with('-')
            || token.starts_with('<')
            || token.starts_with('[')
            || token.contains('<')
            || token.contains('[')
        {
            continue;
        }
        // Strip a trailing colon if present (markdown formatting:
        // "run `asd init`:").
        let cleaned = token.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
        if !cleaned.is_empty() {
            found.insert(cleaned.to_string());
        }
    }
    found
}

/// Run `asd <subcommand> --help` and assert exit 0.
/// Returns the subprocess output for diagnostics on failure.
fn assert_subcommand_resolves(sub: &str) -> Result<(), String> {
    let out = Command::new(asd_bin())
        .args([sub, "--help"])
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`asd {sub} --help` exited non-zero\nstderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn run_cmd_capture_stdout(args: &[&str]) -> String {
    let out = Command::new(asd_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn `asd {}`: {e}", args.join(" ")));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn top_level_help_long_about_examples_resolve() {
    // asd --help renders the long_about block which lists the
    // bootstrap sequences + daily loop. Every `asd <sub>`
    // reference there must resolve.
    let text = run_cmd_capture_stdout(&["--help"]);
    let subs = extract_asd_subcommands(&text);
    assert!(
        !subs.is_empty(),
        "extractor should find at least one `asd <sub>` in --help; got empty set\n--- help text ---\n{text}"
    );
    let mut broken: Vec<String> = Vec::new();
    for sub in &subs {
        if let Err(msg) = assert_subcommand_resolves(sub) {
            broken.push(format!("  - {sub}: {msg}"));
        }
    }
    assert!(
        broken.is_empty(),
        "`asd --help` references {} broken subcommand(s):\n{}\n(all extracted: {subs:?})",
        broken.len(),
        broken.join("\n")
    );
}

#[test]
fn think_bootstrap_human_output_examples_resolve() {
    // `asd think bootstrap` prints the starter checklist in plain
    // text. This is the surface that shipped `asd reindex`
    // before 1.0.62 added the alias — the literal regression
    // that triggered t-018.
    //
    // Needs --db pointing at an empty tempdir-backed sqlite so
    // the gather_thinking pass doesn't error on a missing repo.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let text = run_cmd_capture_stdout(&[
        "--db",
        db.to_str().unwrap(),
        "think",
        "bootstrap",
    ]);
    let subs = extract_asd_subcommands(&text);
    // Some output (e.g. "Read the prompt") references commands;
    // empty set is acceptable for a fresh repo. The strict check
    // is "anything we DID extract must resolve."
    let mut broken: Vec<String> = Vec::new();
    for sub in &subs {
        if let Err(msg) = assert_subcommand_resolves(sub) {
            broken.push(format!("  - {sub}: {msg}"));
        }
    }
    assert!(
        broken.is_empty(),
        "`asd think bootstrap` references {} broken subcommand(s):\n{}\n(all extracted: {subs:?})\n--- bootstrap text ---\n{text}",
        broken.len(),
        broken.join("\n")
    );
}

#[test]
fn think_bootstrap_json_output_examples_resolve() {
    // The --json variant has its own checklist string array; make
    // sure that ships with valid commands too (each entry has its
    // own copy of the example text).
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join(".asd-state.db");
    let text = run_cmd_capture_stdout(&[
        "--db",
        db.to_str().unwrap(),
        "think",
        "bootstrap",
        "--json",
    ]);
    let subs = extract_asd_subcommands(&text);
    let mut broken: Vec<String> = Vec::new();
    for sub in &subs {
        if let Err(msg) = assert_subcommand_resolves(sub) {
            broken.push(format!("  - {sub}: {msg}"));
        }
    }
    assert!(
        broken.is_empty(),
        "`asd think bootstrap --json` references {} broken subcommand(s):\n{}\n(all extracted: {subs:?})",
        broken.len(),
        broken.join("\n")
    );
}

#[test]
fn extractor_unit_skips_placeholders_and_flags() {
    // Self-test for the extractor — making sure the allow-list
    // logic doesn't accidentally flag valid templates as broken
    // subcommands.
    let text = "Try `asd index .` or `asd <subcommand> --help` or `asd --help` or `asd [OPTIONS]`.";
    let subs = extract_asd_subcommands(text);
    assert!(subs.contains("index"), "must extract literal 'index'");
    assert!(!subs.contains("<subcommand>"), "must skip placeholder");
    assert!(!subs.iter().any(|s| s.starts_with('-')), "must skip flags");
    assert!(!subs.iter().any(|s| s.starts_with('[')), "must skip clap templates");
}

#[test]
fn extractor_unit_handles_markdown_trailing_punctuation() {
    // Bootstrap output sometimes has `asd init`: (with trailing
    // colon glued to the closing backtick). The extractor must
    // not produce the broken "init:" token.
    let text = "Run `asd init` to start.";
    let subs = extract_asd_subcommands(text);
    assert!(subs.contains("init"), "got: {subs:?}");
}

#[test]
fn extractor_unit_returns_empty_for_no_backticks() {
    // No backticks → empty set; the extractor must not panic or
    // synthesize commands from prose.
    let text = "This text has no backticks and mentions asd index by name.";
    let subs = extract_asd_subcommands(text);
    assert!(subs.is_empty(), "got: {subs:?}");
}
