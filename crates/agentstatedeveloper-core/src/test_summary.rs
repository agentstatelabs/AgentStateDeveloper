//! Compact, failures-only summaries of test-runner output (Plan
//! competitive-harvest t-005). Reads a runner's verbose output and keeps only
//! what an agent needs to act: the pass/fail counts and each failure with a
//! little context — typically a ~90% token cut versus the raw log.
//!
//! Runners: Cargo and pytest are parsed precisely (the two relevant to an ASD
//! codebase); everything else falls back to a generic failure-line scan.

/// One failed test plus a few lines of captured detail.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub name: String,
    pub detail: Vec<String>,
}

/// Compact result of a test run.
#[derive(Debug, Clone, PartialEq)]
pub struct TestSummary {
    pub runner: String,
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<Failure>,
}

/// Parse test-runner output into a compact summary, auto-detecting the runner.
pub fn summarize(text: &str) -> TestSummary {
    match detect_runner(text) {
        "cargo" => parse_cargo(text),
        "pytest" => parse_pytest(text),
        other => parse_generic(text, other),
    }
}

fn detect_runner(text: &str) -> &'static str {
    if text.contains("test result:") && text.contains("running ") {
        "cargo"
    } else if text.contains("=== ")
        && (text.contains(" passed") || text.contains(" failed"))
        && (text.contains("pytest") || text.contains("PASSED") || text.contains("FAILED "))
    {
        "pytest"
    } else {
        "generic"
    }
}

// --- Cargo -----------------------------------------------------------------

fn parse_cargo(text: &str) -> TestSummary {
    let mut passed = 0;
    let mut failed = 0;
    let mut failed_names: Vec<String> = Vec::new();
    // `---- name stdout ----` … blocks give detail per failure.
    let mut details: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        // `test result: ok. 12 passed; 0 failed; ...` — sum across binaries.
        if let Some(rest) = t.strip_prefix("test result:") {
            passed += count_before(rest, "passed");
            failed += count_before(rest, "failed");
        }
        // `test some::name ... FAILED`
        if let Some(name) = t.strip_prefix("test ") {
            if let Some(name) = name.strip_suffix(" ... FAILED") {
                let name = name.trim();
                if !name.is_empty() && !failed_names.iter().any(|n| n == name) {
                    failed_names.push(name.to_string());
                }
            }
        }
        // `---- name stdout ----` detail block until the next `----`/blank.
        if t.starts_with("---- ") && t.ends_with(" ----") {
            let name = t
                .trim_start_matches("---- ")
                .trim_end_matches(" ----")
                .trim_end_matches(" stdout")
                .trim()
                .to_string();
            let mut detail = Vec::new();
            i += 1;
            while i < lines.len() {
                let d = lines[i].trim();
                if d.is_empty() || d.starts_with("----") {
                    break;
                }
                if detail.len() < 6 {
                    detail.push(lines[i].trim_end().to_string());
                }
                i += 1;
            }
            details.entry(name).or_insert(detail);
            continue;
        }
        i += 1;
    }

    let failures = failed_names
        .into_iter()
        .map(|name| {
            let detail = details.get(&name).cloned().unwrap_or_default();
            Failure { name, detail }
        })
        .collect();

    TestSummary { runner: "cargo".into(), passed, failed, failures }
}

// --- pytest ----------------------------------------------------------------

fn parse_pytest(text: &str) -> TestSummary {
    let mut passed = 0;
    let mut failed = 0;
    let mut failures: Vec<Failure> = Vec::new();

    for raw in text.lines() {
        let t = raw.trim();
        // Short summary line: `FAILED path::test - AssertionError: ...`
        if let Some(rest) = t.strip_prefix("FAILED ") {
            let mut parts = rest.splitn(2, " - ");
            let name = parts.next().unwrap_or("").trim().to_string();
            let detail = parts.next().map(|d| vec![d.trim().to_string()]).unwrap_or_default();
            if !name.is_empty() && !failures.iter().any(|f| f.name == name) {
                failures.push(Failure { name, detail });
            }
        }
        // The `=== 2 failed, 10 passed in 1.2s ===` summary line.
        if t.starts_with("===") && (t.contains(" passed") || t.contains(" failed")) {
            passed = passed.max(count_before(t, "passed"));
            failed = failed.max(count_before(t, "failed"));
        }
    }
    if failed == 0 {
        failed = failures.len();
    }
    TestSummary { runner: "pytest".into(), passed, failed, failures }
}

// --- Generic ---------------------------------------------------------------

fn parse_generic(text: &str, runner: &str) -> TestSummary {
    let mut failures: Vec<Failure> = Vec::new();
    for raw in text.lines() {
        let t = raw.trim();
        let looks_failed = t.contains("FAIL")
            || t.contains("panicked")
            || t.starts_with("Error")
            || t.contains("✗")
            || t.contains("✘");
        if looks_failed && !t.is_empty() && failures.len() < 200 {
            failures.push(Failure { name: t.to_string(), detail: Vec::new() });
        }
    }
    let failed = failures.len();
    TestSummary { runner: runner.into(), passed: 0, failed, failures }
}

// --- helpers ---------------------------------------------------------------

/// Extract the integer immediately preceding `word` in `s`, e.g.
/// `count_before("ok. 12 passed; 0 failed", "passed")` → 12. Returns 0 if absent.
fn count_before(s: &str, word: &str) -> usize {
    let Some(pos) = s.find(word) else {
        return 0;
    };
    s[..pos]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .next_back()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_before_extracts_preceding_int() {
        assert_eq!(count_before("ok. 12 passed; 3 failed", "passed"), 12);
        assert_eq!(count_before("ok. 12 passed; 3 failed", "failed"), 3);
        assert_eq!(count_before("no number here", "passed"), 0);
    }

    #[test]
    fn cargo_failures_and_counts() {
        let out = "\
running 3 tests
test alpha ... ok
test beta::works ... FAILED
test gamma ... ok

failures:

---- beta::works stdout ----
thread 'beta::works' panicked at src/lib.rs:42:9:
assertion `left == right` failed
  left: 1
  right: 2

failures:
    beta::works

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";
        let s = summarize(out);
        assert_eq!(s.runner, "cargo");
        assert_eq!((s.passed, s.failed), (2, 1));
        assert_eq!(s.failures.len(), 1);
        assert_eq!(s.failures[0].name, "beta::works");
        assert!(s.failures[0].detail.iter().any(|d| d.contains("panicked")));
    }

    #[test]
    fn cargo_sums_across_binaries() {
        let out = "\
running 1 test
test a ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
running 2 tests
test b ... ok
test c ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";
        let s = summarize(out);
        assert_eq!((s.passed, s.failed), (3, 0));
        assert!(s.failures.is_empty());
    }

    #[test]
    fn pytest_short_summary() {
        let out = "\
=========================== short test summary info ============================
FAILED tests/test_api.py::test_charge - assert 500 == 200
FAILED tests/test_api.py::test_refund - KeyError: 'amount'
========================= 2 failed, 10 passed in 1.23s =========================
";
        let s = summarize(out);
        assert_eq!(s.runner, "pytest");
        assert_eq!((s.passed, s.failed), (10, 2));
        assert_eq!(s.failures.len(), 2);
        assert_eq!(s.failures[0].name, "tests/test_api.py::test_charge");
        assert_eq!(s.failures[0].detail, vec!["assert 500 == 200".to_string()]);
    }

    #[test]
    fn generic_fallback_collects_failure_lines() {
        let out = "ok pkg/a\n--- FAIL: TestThing (0.00s)\n    thing_test.go:10: boom\nFAIL\n";
        let s = summarize(out);
        assert_eq!(s.runner, "generic");
        assert!(s.failed >= 1);
        assert!(s.failures.iter().any(|f| f.name.contains("FAIL")));
    }
}
