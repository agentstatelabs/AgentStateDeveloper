//! `asd-mcp` — MCP server stub for AgentStateDeveloper.
//!
//! M1 placeholder: prints the planned MCP tool surface to stderr and exits 0.
//! The real stdio/HTTP MCP server lands in M2.

use anyhow::Result;

/// (name, one-line purpose) — mirrors DESIGN.md § "MCP tool surface".
const READ_TOOLS: &[(&str, &str)] = &[
    ("code_query", "find symbols by name/kind/file/tag/effect"),
    (
        "code_read",
        "fetch symbol source with declared effects + top ledger entries inline (primary \"read a function\" path)",
    ),
    ("callers_of", "inbound call edges for a symbol"),
    ("callees_of", "outbound call edges"),
    (
        "effects_of",
        "declared + transitive effects, with verification status",
    ),
    ("ledger_get", "entries for a symbol (non-superseded by default)"),
    (
        "ledger_find",
        "search ledger by kind, tag, author, date, free-text",
    ),
    ("traces_of", "execution evidence for a symbol"),
];

const WRITE_TOOLS: &[(&str, &str)] = &[
    (
        "ledger_append",
        "new entry (symbol_id, kind, summary required)",
    ),
    (
        "ledger_supersede",
        "write new entry that supersedes one or more existing",
    ),
    (
        "effect_declare",
        "set/replace declared effects for a symbol; triggers re-verify",
    ),
];

const ADMIN_TOOLS: &[(&str, &str)] = &[
    (
        "verify_effects",
        "run checker against declared effects; returns mismatches",
    ),
    (
        "reindex",
        "force re-parse of file or symbol (normally automatic)",
    ),
    (
        "health",
        "indexer status, last-sync per file, orphaned-entry count",
    ),
];

fn print_section(title: &str, tools: &[(&str, &str)]) {
    eprintln!("{}:", title);
    for (name, purpose) in tools {
        eprintln!("  {} — {}", name, purpose);
    }
    eprintln!();
}

fn main() -> Result<()> {
    eprintln!("asd-mcp — AgentStateDeveloper MCP server (M1 stub)");
    eprintln!("planned MCP tool surface:");
    eprintln!();

    print_section("Read", READ_TOOLS);
    print_section("Write", WRITE_TOOLS);
    print_section("Admin", ADMIN_TOOLS);

    eprintln!("Note: M1 stub — real MCP server lands in M2.");
    Ok(())
}
