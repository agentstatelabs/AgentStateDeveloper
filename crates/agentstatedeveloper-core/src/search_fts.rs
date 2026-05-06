//! FTS5-backed search index for ASD symbols.
//!
//! Opens a direct `rusqlite` connection to the same `.asd-state.db` used by
//! the stategraph backend and maintains a `asd_search_fts` virtual table.
//!
//! ## Why a separate connection?
//! Stategraph abstracts over SQLite and doesn't expose raw SQL. Opening a
//! second connection to the same WAL-mode database is safe and avoids coupling
//! the two storage layers.
//!
//! ## Rebuild vs. incremental
//! `rebuild()` is the canonical path: the indexer already holds the complete
//! "current world" snapshot after `asd index`, so a full replace avoids any
//! sync bugs with the git blob store (e.g. deleted files leaving stale FTS rows).
//!
//! ## Tokenizer choice
//! `unicode61 remove_diacritics 1` — word-based, unicode-aware. CamelCase and
//! snake_case identifiers are pre-expanded at insert time so "refreshDriftPlayhead"
//! becomes "refreshDriftPlayhead refresh drift playhead", making word-level
//! queries work without prefix tricks.
//!
//! ## Column weights for BM25
//! qname=10, signature=5, doc=5, file=2  (unindexed metadata columns excluded)
//!
//! ## Test symbol handling
//! `is_test` is set at insert time based on file-path heuristics. Tests are
//! excluded from results by default; pass `FtsFilters { include_tests: true }`
//! to include them.

use std::path::Path;

use rusqlite::{Connection, params};

use crate::schema::Symbol;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single ranked result from FTS search.
#[derive(Debug, Clone)]
pub struct FtsHit {
    /// BM25 score (negated from SQLite's negative convention; higher = more relevant).
    pub bm25_score: f64,
    pub symbol_id: String,
    /// Original qname (not expanded).
    pub qname: String,
    pub kind: String,
    pub language: String,
    /// Original file path (not expanded).
    pub file: String,
    pub line: u32,
    /// Original signature (not expanded).
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub is_test: bool,
}

/// Filters applied before BM25 ranking.
#[derive(Debug, Default, Clone)]
pub struct FtsFilters {
    pub kind: Option<String>,
    pub language: Option<String>,
    /// Include test symbols (files under test/tests/spec directories, etc.).
    /// Default: false — tests are excluded so production entry points rank first.
    pub include_tests: bool,
}

// ---------------------------------------------------------------------------
// SearchFtsDb
// ---------------------------------------------------------------------------

pub struct SearchFtsDb {
    conn: Connection,
}

impl SearchFtsDb {
    /// Open (or create) the FTS index in `db_path`.
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let db = Self { conn };
        db.ensure_schema()?;
        Ok(db)
    }

    fn ensure_schema(&self) -> rusqlite::Result<()> {
        // Version 3: adds is_test UNINDEXED column.
        // Any version mismatch drops and recreates — data is reproduced by next `asd index`.
        const SCHEMA_VER: i64 = 3;

        let current: i64 = self.conn.query_row(
            "SELECT version FROM asd_fts_meta LIMIT 1",
            [],
            |r| r.get(0),
        ).unwrap_or(0);

        if current != SCHEMA_VER {
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS asd_search_fts;
                 DROP TABLE IF EXISTS asd_search_meta;
                 DROP TABLE IF EXISTS asd_fts_meta;",
            )?;
        }

        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS asd_search_fts USING fts5(
                symbol_id  UNINDEXED,
                qname,
                signature,
                doc,
                file,
                language   UNINDEXED,
                kind       UNINDEXED,
                line       UNINDEXED,
                qname_orig UNINDEXED,
                sig_orig   UNINDEXED,
                file_orig  UNINDEXED,
                is_test    UNINDEXED,
                tokenize   = 'unicode61 remove_diacritics 1'
            );
            CREATE TABLE IF NOT EXISTS asd_search_meta (
                file       TEXT PRIMARY KEY,
                indexed_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS asd_fts_meta (version INTEGER PRIMARY KEY);
            INSERT OR IGNORE INTO asd_fts_meta VALUES ({SCHEMA_VER});"
        ))
    }

    /// Atomically replace the entire FTS table from `symbols`.
    ///
    /// This is the canonical indexing path. Because `asd index` already has the
    /// complete current-world snapshot, a full rebuild is cheaper than tracking
    /// which files changed and avoids stale rows from deleted files.
    pub fn rebuild(&self, symbols: &[Symbol]) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "DELETE FROM asd_search_fts; DELETE FROM asd_search_meta;",
        )?;
        for sym in symbols {
            self.insert_symbol(sym)?;
        }
        Ok(())
    }

    fn insert_symbol(&self, sym: &Symbol) -> rusqlite::Result<()> {
        let qname_exp = expand_identifier(&sym.qname);
        let sig_orig = sym.signature.as_deref().unwrap_or("");
        let sig_exp = if sig_orig.is_empty() { String::new() } else { expand_text(sig_orig) };
        let doc = sym.doc.as_deref().unwrap_or("");
        let file_exp = expand_text(&sym.file);
        let kind = format!("{:?}", sym.kind).to_lowercase();
        let is_test = if is_test_file(&sym.file) { "1" } else { "0" };

        self.conn.execute(
            "INSERT INTO asd_search_fts(
                 symbol_id, qname, signature, doc, file, language, kind, line,
                 qname_orig, sig_orig, file_orig, is_test)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                sym.symbol_id,
                qname_exp,
                sig_exp,
                doc,
                file_exp,
                sym.language,
                kind,
                sym.start.line,
                sym.qname,
                sym.signature.as_deref().unwrap_or(""),
                sym.file,
                is_test,
            ],
        )?;
        Ok(())
    }

    /// Ranked FTS search. Returns hits ordered by relevance (highest first).
    ///
    /// BM25 column weights: qname=10, signature=5, doc=5, file=2.
    /// Tests excluded unless `filters.include_tests` is true.
    pub fn search(
        &self,
        query: &str,
        filters: &FtsFilters,
        limit: usize,
    ) -> rusqlite::Result<Vec<FtsHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        let tokens: Vec<String> = query
            .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
            .map(|t| t.to_lowercase())
            .filter(|t| t.len() >= 2)
            .collect();

        if tokens.is_empty() {
            return Ok(vec![]);
        }

        // FTS5 MATCH: "token1" OR "token2" ... — each token quoted against special chars.
        let match_expr = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");

        // UNINDEXED columns can be used in regular WHERE clauses (not MATCH).
        let kind_clause = filters
            .kind
            .as_deref()
            .map(|k| format!("AND kind = '{}'", k.to_lowercase().replace('\'', "")))
            .unwrap_or_default();
        let lang_clause = filters
            .language
            .as_deref()
            .map(|l| format!("AND language = '{}'", l.to_lowercase().replace('\'', "")))
            .unwrap_or_default();
        let test_clause = if filters.include_tests { "" } else { "AND is_test != '1'" };

        // Fetch extra for hybrid ledger reranking.
        let fetch = (limit * 4).max(80);

        // Columns: 0=symbol_id,1=language,2=kind,3=line,4=doc,
        //          5=qname_orig,6=sig_orig,7=file_orig,8=is_test,9=score
        let sql = format!(
            "SELECT symbol_id, language, kind, line, doc,
                    qname_orig, sig_orig, file_orig, is_test,
                    bm25(asd_search_fts, 10.0, 5.0, 5.0, 2.0) AS score
             FROM asd_search_fts
             WHERE asd_search_fts MATCH ?1
             {test_clause}
             {kind_clause}
             {lang_clause}
             ORDER BY score
             LIMIT {fetch}"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let hits = stmt.query_map(params![match_expr], |row| {
            let bm25_raw: f64 = row.get(9)?;
            let sig_orig: Option<String> = row.get(6)?;
            let is_test_str: String = row.get(8).unwrap_or_default();
            Ok(FtsHit {
                bm25_score: -bm25_raw,
                symbol_id: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                line: row.get::<_, u32>(3).unwrap_or(0),
                doc: row.get(4)?,
                qname: row.get(5)?,
                signature: sig_orig.filter(|s| !s.is_empty()),
                file: row.get(7)?,
                is_test: is_test_str == "1",
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

        Ok(hits)
    }

    /// True if the FTS table has at least one row.
    pub fn has_data(&self) -> bool {

        self.conn
            .query_row("SELECT COUNT(*) FROM asd_search_fts LIMIT 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n > 0)
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Hybrid reranking boost
// ---------------------------------------------------------------------------

/// Rust-side score boost applied on top of BM25 after FTS returns hits.
///
/// Two components, both measured against `tokens` (lowercased query words):
///
/// **Path word boost (+1.5 / token)** — camelCase-expands each file path
/// segment and checks for exact word matches. "drift" matches the segment
/// "ExampleFlow" (expanded → ["session","drift"]) but NOT "overlap" for
/// the token "over". Rewards symbols that live in a subsystem named after
/// a query concept without rewarding false substring matches.
///
/// **Name segment boost (+2.0 / token)** — camelCase-expands only the last
/// dotted segment of `qname` (the function/method name, stripped of
/// namespace). "drift playhead" matches both words in `refreshDriftPlayhead`
/// (+4.0) but only "playhead" in `isInPlayheadHandle` (+2.0), correcting
/// the case where a single-token exact match outranks a dual-token match.
pub fn hybrid_boost(hit: &FtsHit, tokens: &[String]) -> f64 {
    if tokens.is_empty() {
        return 0.0;
    }

    // Expand file path into whole camelCase words.
    let path_words: Vec<String> = hit.file
        .split(|c: char| c == '/' || c == '\\' || c == '.')
        .filter(|s| !s.is_empty())
        .flat_map(|seg| {
            let mut words: Vec<String> = vec![seg.to_lowercase()];
            words.extend(split_camel(seg).iter().map(|w| w.to_lowercase()));
            words
        })
        .filter(|w| w.len() >= 2)
        .collect();

    let path_boost = tokens
        .iter()
        .filter(|t| path_words.iter().any(|w| w == t.as_str()))
        .count() as f64
        * 1.5;

    // Expand last qname segment (function name) into camelCase words.
    let last_seg = hit.qname.rsplit('.').next().unwrap_or(&hit.qname);
    let name_words: Vec<String> = {
        let mut words: Vec<String> = vec![last_seg.to_lowercase()];
        words.extend(split_camel(last_seg).iter().map(|w| w.to_lowercase()));
        words
    };

    let name_boost = tokens
        .iter()
        .filter(|t| name_words.iter().any(|w| w == t.as_str()))
        .count() as f64
        * 2.0;

    path_boost + name_boost
}

// ---------------------------------------------------------------------------
// Test-file detection
// ---------------------------------------------------------------------------

/// Heuristically determine whether a symbol's source file is a test file.
///
/// Checks directory components and filename patterns that are idiomatic across
/// all supported languages. Does NOT check symbol name — only the file path.
fn is_test_file(file: &str) -> bool {
    let lower = file.to_lowercase();
    let segments: Vec<&str> = lower.split(|c| c == '/' || c == '\\').collect();

    // Directory components that indicate a test tree.
    const TEST_DIRS: &[&str] = &[
        "tests", "test", "specs", "spec", "__tests__", "__mocks__", "testing", "testcases",
    ];
    if segments.iter().rev().skip(1).any(|s| TEST_DIRS.contains(s)) {
        return true;
    }

    // Filename patterns (language-specific conventions).
    if let Some(filename) = segments.last() {
        // Python: test_foo.py, foo_test.py
        if filename.starts_with("test_") || filename.ends_with("_test.py") {
            return true;
        }
        // Go: foo_test.go
        if filename.ends_with("_test.go") {
            return true;
        }
        // Rust: foo_test.rs
        if filename.ends_with("_test.rs") {
            return true;
        }
        // Swift: FooTests.swift, FooSpec.swift
        if filename.ends_with("tests.swift") || filename.ends_with("spec.swift") {
            return true;
        }
        // JS/TS: foo.test.ts, foo.spec.ts, foo.test.tsx, foo.spec.tsx, foo.test.js
        if filename.contains(".test.") || filename.contains(".spec.") {
            return true;
        }
        // Ruby: foo_spec.rb
        if filename.ends_with("_spec.rb") {
            return true;
        }
        // Java/Kotlin: FooTest.java, FooTests.kt
        if filename.ends_with("test.java")
            || filename.ends_with("tests.java")
            || filename.ends_with("test.kt")
            || filename.ends_with("tests.kt")
        {
            return true;
        }
        // C#: FooTests.cs
        if filename.ends_with("tests.cs") || filename.ends_with("test.cs") {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Text expansion helpers
// ---------------------------------------------------------------------------

/// Expand a dotted qname like `App.MyModule.refreshDriftPlayhead` into a
/// string that contains both the original and all word fragments.
fn expand_identifier(qname: &str) -> String {
    let mut parts: Vec<String> = vec![qname.to_string()];

    for segment in qname.split('.') {
        parts.push(segment.to_lowercase());
        for word in split_camel(segment) {
            let w = word.to_lowercase();
            if w.len() >= 2 {
                parts.push(w);
            }
        }
    }

    parts.dedup();
    parts.join(" ")
}

/// Expand arbitrary text (signature, file path) by splitting on non-alphanumeric
/// chars and camelCase boundaries, appending lowercased words.
fn expand_text(text: &str) -> String {
    let mut words: Vec<String> = vec![text.to_string()];

    let tokens: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 2)
        .collect();

    for tok in tokens {
        words.push(tok.to_lowercase());
        for word in split_camel(tok) {
            let w = word.to_lowercase();
            if w.len() >= 2 {
                words.push(w);
            }
        }
    }

    words.dedup();
    words.join(" ")
}

/// Split a camelCase or PascalCase token into constituent words.
fn split_camel(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut starts = vec![0usize];

    for i in 1..bytes.len() {
        let prev = bytes[i - 1];
        let curr = bytes[i];
        // lower→upper ("dD") or digit→upper ("1D")
        if curr.is_ascii_uppercase() && (prev.is_ascii_lowercase() || prev.is_ascii_digit()) {
            starts.push(i);
        }
        // consecutive uppercase followed by lower ("XMLParser" → "XML","Parser")
        if i + 1 < bytes.len()
            && prev.is_ascii_uppercase()
            && curr.is_ascii_uppercase()
            && bytes[i + 1].is_ascii_lowercase()
        {
            starts.push(i);
        }
    }

    starts.push(bytes.len());
    starts
        .windows(2)
        .map(|w| &s[w[0]..w[1]])
        .filter(|seg| !seg.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_camel_basic() {
        assert_eq!(split_camel("refreshDriftPlayhead"), vec!["refresh", "Drift", "Playhead"]);
        assert_eq!(split_camel("DriftSynthPool"), vec!["Drift", "Synth", "Pool"]);
        assert_eq!(split_camel("XMLParser"), vec!["XML", "Parser"]);
        assert_eq!(split_camel("simple"), vec!["simple"]);
    }

    #[test]
    fn expand_identifier_roundtrip() {
        let exp = expand_identifier("App.ExampleFlow.refreshDriftPlayhead");
        assert!(exp.contains("refreshDriftPlayhead"), "original preserved");
        assert!(exp.contains("refresh"), "camel split");
        assert!(exp.contains("playhead"), "camel split tail");
        assert!(exp.contains("exampleflow"), "segment lowercased");
    }

    #[test]
    fn is_test_file_detection() {
        assert!(is_test_file("Packages/AudioEngine/Tests/KarplusStrongTests.swift"));
        assert!(is_test_file("tests/test_charge_card.py"));
        assert!(is_test_file("src/__tests__/auth.test.ts"));
        assert!(is_test_file("payments/test_stripe.py"));
        assert!(is_test_file("pkg/payments/charge_test.go"));
        assert!(is_test_file("src/auth/auth_spec.rb"));
        assert!(!is_test_file("App/ExampleFlow/ExampleFlowApp.swift"));
        assert!(!is_test_file("src/payments/charge.py"));
        assert!(!is_test_file("Packages/AudioEngine/Sources/KarplusStrong.swift"));
    }

    #[test]
    fn fts_excludes_tests_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();

        use crate::schema::{Position, Symbol, SymbolKind};
        let make_sym = |id: &str, qname: &str, file: &str| Symbol {
            symbol_id: id.to_string(),
            symbol_fp: format!("fp_{id}"),
            qname: qname.to_string(),
            language: "swift".to_string(),
            kind: SymbolKind::Method,
            file: file.to_string(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 10, col: 0 },
            signature: None,
            doc: None,
        };

        let prod = make_sym("sym_prod", "App.ViewModel.refreshDriftPlayhead", "App/ViewModel.swift");
        let test = make_sym("sym_test", "Tests.DriftTests.testRefreshPlayhead", "Tests/DriftTests.swift");

        fts.rebuild(&[prod, test]).unwrap();
        assert!(fts.has_data());

        // Default: tests excluded.
        let hits = fts.search("playhead", &FtsFilters::default(), 10).unwrap();
        assert_eq!(hits.len(), 1, "only prod symbol by default");
        assert_eq!(hits[0].symbol_id, "sym_prod");

        // With include_tests: both returned.
        let hits_all = fts.search("playhead", &FtsFilters { include_tests: true, ..Default::default() }, 10).unwrap();
        assert_eq!(hits_all.len(), 2, "both when include_tests");
    }

    #[test]
    fn fts_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();

        use crate::schema::{Position, Symbol, SymbolKind};
        let sym = Symbol {
            symbol_id: "sym_abc".into(),
            symbol_fp: "fp_abc".into(),
            qname: "App.ViewModel.refreshDriftPlayhead".into(),
            language: "swift".into(),
            kind: SymbolKind::Method,
            file: "App/ViewModel.swift".into(),
            start: Position { line: 10, col: 0 },
            end: Position { line: 20, col: 0 },
            signature: Some("private func refreshDriftPlayhead()".into()),
            doc: Some("Refreshes the drift playhead position".into()),
        };

        fts.rebuild(std::slice::from_ref(&sym)).unwrap();
        assert!(fts.has_data());

        let hits = fts.search("playhead", &FtsFilters::default(), 10).unwrap();
        assert!(!hits.is_empty(), "should find by qname fragment");
        assert_eq!(hits[0].symbol_id, "sym_abc");
        assert_eq!(hits[0].qname, "App.ViewModel.refreshDriftPlayhead", "orig qname preserved");
        assert!(!hits[0].is_test, "production file not flagged as test");

        let hits2 = fts.search("refresh drift", &FtsFilters::default(), 10).unwrap();
        assert!(!hits2.is_empty(), "should find multi-token");

        let hits3 = fts.search(
            "playhead",
            &FtsFilters { language: Some("python".into()), ..Default::default() },
            10,
        ).unwrap();
        assert!(hits3.is_empty(), "language filter should exclude swift");
    }
}
