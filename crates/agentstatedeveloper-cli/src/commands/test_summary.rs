//! `asd test-summary` — read test-runner output on stdin and emit a compact,
//! failures-only summary. Pairs with ASD's test-gap detection: run your tests,
//! pipe the log in, and hand the agent only the failures + counts.
//!
//! Example: `cargo test 2>&1 | asd test-summary`

use std::io::Read;

use anyhow::Result;
use clap::Args;
use serde_json::json;

use agentstatedeveloper_core::test_summary::summarize;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct TestSummaryArgs {
    /// Machine-readable JSON.
    #[arg(long)]
    pub agent: bool,
}

pub fn run(_cfg: &Config, args: TestSummaryArgs) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let s = summarize(&input);

    if args.agent {
        let out = json!({
            "runner": s.runner,
            "passed": s.passed,
            "failed": s.failed,
            "failures": s.failures.iter().map(|f| json!({
                "name": f.name,
                "detail": f.detail,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if s.failed == 0 {
        println!("✓ {} passed, 0 failed ({})", s.passed, s.runner);
        return Ok(());
    }
    println!("✗ {} failed, {} passed ({})", s.failed, s.passed, s.runner);
    for f in &s.failures {
        println!("  {}", f.name);
        for d in &f.detail {
            println!("      {}", d);
        }
    }
    Ok(())
}
