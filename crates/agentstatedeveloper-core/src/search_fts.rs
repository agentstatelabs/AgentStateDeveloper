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
//! ## Tokenizer choice
//! `unicode61 remove_diacritics 1` — word-based, unicode-aware. CamelCase and
//! snake_case identifiers are pre-expanded at insert time so "refreshDriftPlayhead"
//! becomes "refreshDriftPlayhead refresh drift playhead", making word-level and
//! substring-approximate queries both work.
//!
//! ## Column weights for BM25
//! qname=10, signature=5, doc=5, file=2  (language and kind are UNINDEXED)

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
}

/// Filters applied before BM25 ranking.
#[derive(Debug, Default, Clone)]
pub struct FtsFilters {
    pub kind: Option<String>,
    pub language: Option<String>,
}

// ---------------------------------------------------------------------------
// SearchFtsDb
// ---------------------------------------------------------------------------

pub struct SearchFtsDb {
    conn: Connection,
}

impl SearchFtsDb {
    /// Open (or create) the FTS index in `db_path`.
    ///
    /// `db_path` is the same file as the stategraph database. We open a second
    /// connection in WAL mode so reads/writes don't block each other.
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let db = Self { conn };
        db.ensure_schema()?;
        Ok(db)
    }

    fn ensure_schema(&self) -> rusqlite::Result<()> {
        // Version 2: adds qname_orig/sig_orig/file_orig UNINDEXED display columns.
        // If the version table is absent or stale, drop and recreate everything.
        const SCHEMA_VER: i64 = 2;

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

    /// Remove all FTS entries for a specific source file, then insert fresh
    /// entries for every symbol in `symbols` that belongs to that file.
    ///
    /// This is the incremental update path used by `asd index`.
    pub fn upsert_file(&self, file: &str, symbols: &[Symbol]) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM asd_search_fts WHERE file = ?1",
            params![file],
        )?;

        for sym in symbols.iter().filter(|s| s.file == file) {
            self.insert_symbol(sym)?;
        }

        self.conn.execute(
            "INSERT OR REPLACE INTO asd_search_meta(file, indexed_at) VALUES(?1, unixepoch())",
            params![file],
        )?;
        Ok(())
    }

    /// Wipe and rebuild the entire FTS table from `symbols`.
    /// Used by `asd reindex` and on first index of a fresh db.
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

        self.conn.execute(
            "INSERT INTO asd_search_fts(
                 symbol_id, qname, signature, doc, file, language, kind, line,
                 qname_orig, sig_orig, file_orig)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
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
            ],
        )?;
        Ok(())
    }

    /// Ranked FTS search. Returns hits ordered by relevance (highest first).
    ///
    /// `query` is tokenised the same way as at insert — whitespace/punctuation
    /// splits into tokens, each matched independently. Passing `"playhead clips"`
    /// finds symbols that contain both words across any indexed column.
    ///
    /// BM25 column weights: qname=10, signature=5, doc=5, file=2.
    pub fn search(
        &self,
        query: &str,
        filters: &FtsFilters,
        limit: usize,
    ) -> rusqlite::Result<Vec<FtsHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        // Build the FTS MATCH expression: each token OR-OR'd isn't right;
        // SQLite FTS5 MATCH uses implicit AND by default for space-separated terms.
        // We want any token to contribute (OR semantics) — use column filters.
        // Strategy: run one query per token and UNION, or use the prefix query.
        // Simplest correct approach: `token1 OR token2 OR ...` via explicit OR.
        let tokens: Vec<String> = query
            .split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
            .map(|t| t.to_lowercase())
            .filter(|t| t.len() >= 2)
            .collect();

        if tokens.is_empty() {
            return Ok(vec![]);
        }

        // FTS5 MATCH expression: "token1" OR "token2" ...
        // Each token is quoted to avoid special-char issues.
        let match_expr = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");

        // Pre-filter by kind/language in SQL to reduce result set before BM25 sort.
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

        // Fetch more than limit so hybrid ledger reranking has room to work.
        let fetch = (limit * 4).max(80);

        // Columns: 0=symbol_id,1=language,2=kind,3=line,4=doc,5=qname_orig,6=sig_orig,7=file_orig,8=score
        let sql = format!(
            "SELECT symbol_id, language, kind, line, doc,
                    qname_orig, sig_orig, file_orig,
                    bm25(asd_search_fts, 10.0, 5.0, 5.0, 2.0) AS score
             FROM asd_search_fts
             WHERE asd_search_fts MATCH ?1
             {kind_clause}
             {lang_clause}
             ORDER BY score
             LIMIT {fetch}"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let hits = stmt.query_map(params![match_expr], |row| {
            let bm25_raw: f64 = row.get(8)?;
            let sig_orig: Option<String> = row.get(6)?;
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
// Text expansion helpers
// ---------------------------------------------------------------------------

/// Expand a dotted qname like `App.MyModule.refreshDriftPlayhead` into a
/// string that contains both the original and all word fragments:
/// `"App.MyModule.refreshDriftPlayhead app mymodule refresh drift playhead"`.
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
/// "refreshDriftPlayhead" → ["refresh", "Drift", "Playhead"]
/// "DriftSynthPool"       → ["Drift", "Synth", "Pool"]
fn split_camel(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut starts = vec![0usize];

    for i in 1..bytes.len() {
        let prev = bytes[i - 1];
        let curr = bytes[i];
        // Transition: lower→upper ("dD") or digit→upper ("1D")
        if curr.is_ascii_uppercase() && (prev.is_ascii_lowercase() || prev.is_ascii_digit()) {
            starts.push(i);
        }
        // Transition: consecutive uppercase followed by lower ("XMLParser" → "XML","Parser")
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

        fts.upsert_file(&sym.file, std::slice::from_ref(&sym)).unwrap();
        assert!(fts.has_data());

        let hits = fts.search("playhead", &FtsFilters::default(), 10).unwrap();
        assert!(!hits.is_empty(), "should find by qname fragment");
        assert_eq!(hits[0].symbol_id, "sym_abc");

        let hits2 = fts.search("refresh drift", &FtsFilters::default(), 10).unwrap();
        assert!(!hits2.is_empty(), "should find multi-token");

        let hits3 = fts.search(
            "playhead",
            &FtsFilters { language: Some("python".into()), kind: None },
            10,
        ).unwrap();
        assert!(hits3.is_empty(), "language filter should exclude swift");
    }
}
