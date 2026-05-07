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
// Stopwords
// ---------------------------------------------------------------------------

/// English function words with no discriminative value for code search.
///
/// These are prepositions and conjunctions that appear naturally in
/// natural-language queries ("playhead **over** clips", "scroll **with**
/// velocity") but never appear as meaningful code identifiers. Filtering them
/// prevents FTS MATCH from finding false positives like Swift named parameters
/// (`punchIn(over existingClip:)`) or generic variable names.
///
/// Intentionally omits: "do", "in", "go" (language keywords), "no" (boolean
/// shorthand), "up", "down" (directional — common in audio/UI APIs).
pub const STOPWORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "be", "but", "by", "for", "from",
    "if",  "into", "is", "it", "nor", "not", "of", "on", "or", "so",
    "the", "to", "via", "vs", "yet", "with", "over", "about", "between",
    "than", "that", "this", "are", "was", "were", "has", "have", "had",
    "its", "our",
];

/// Returns true if the token is a stopword.
#[inline]
pub fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Symbol tier — controls default inclusion and scoring penalty.
///
/// - `0` Production: app/core/library source; ranked highest, included by default.
/// - `1` Utility: Preview, Sample, Editor extension, Generated, Mock, Stub, Fixture,
///        Demo — included by default but penalised in hybrid_boost.
/// - `2` Test: files in test/spec directories or with test naming conventions —
///        excluded by default; shown with `--include-tests`.
pub type SymbolTier = u8;

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
    /// Symbol tier: 0=production, 1=utility/preview/sample, 2=test.
    pub tier: SymbolTier,
}

/// Filters applied before BM25 ranking.
#[derive(Debug, Default, Clone)]
pub struct FtsFilters {
    pub kind: Option<String>,
    pub language: Option<String>,
    /// Include test symbols (files under test/tests/spec directories, etc.).
    /// Default: false — tests are excluded so production entry points rank first.
    pub include_tests: bool,
    /// Lowercase substring terms to exclude. Any candidate whose qname, file,
    /// doc, or signature contains one of these strings is dropped.
    pub exclude_terms: Vec<String>,
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
        // Version 4: replaces is_test UNINDEXED with tier UNINDEXED (0=prod, 1=utility, 2=test).
        // Any version mismatch drops and recreates — data is reproduced by next `asd index`.
        const SCHEMA_VER: i64 = 4;

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

        // asd_index_meta is a simple key-value store for index metadata.
        // It is NOT dropped on FTS schema version changes — it persists
        // across rebuilds so indexed_at survives schema upgrades.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asd_index_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        )?;

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
                tier       UNINDEXED,
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
        self.rebuild_refs(&symbols.iter().collect::<Vec<_>>())
    }

    /// Like [`rebuild`] but accepts a slice of references — avoids a copy when
    /// the caller already has a deduplicated `Vec<&Symbol>`.
    pub fn rebuild_refs(&self, symbols: &[&Symbol]) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "DELETE FROM asd_search_fts; DELETE FROM asd_search_meta;",
        )?;
        for sym in symbols {
            self.insert_symbol(sym)?;
        }
        // Stamp the rebuild time so staleness checks can compare against it.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT OR REPLACE INTO asd_index_meta (key, value) VALUES ('indexed_at', ?1)",
            params![now.to_string()],
        )?;
        Ok(())
    }

    /// Unix timestamp (seconds) of the last `rebuild()` call, or `None` if
    /// the index has never been built.
    pub fn last_indexed_at(&self) -> Option<i64> {
        self.conn.query_row(
            "SELECT value FROM asd_index_meta WHERE key = 'indexed_at' LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        ).ok().and_then(|s| s.parse().ok())
    }

    /// Number of rows in the FTS table (total indexed symbols).
    pub fn symbol_count(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM asd_search_fts", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    fn insert_symbol(&self, sym: &Symbol) -> rusqlite::Result<()> {
        let qname_exp = expand_identifier(&sym.qname);
        let sig_orig = sym.signature.as_deref().unwrap_or("");
        let sig_exp = if sig_orig.is_empty() { String::new() } else { expand_text(sig_orig) };
        let doc = sym.doc.as_deref().unwrap_or("");
        let file_exp = expand_text(&sym.file);
        let kind = format!("{:?}", sym.kind).to_lowercase();
        let tier = symbol_tier(&sym.file).to_string();

        self.conn.execute(
            "INSERT INTO asd_search_fts(
                 symbol_id, qname, signature, doc, file, language, kind, line,
                 qname_orig, sig_orig, file_orig, tier)
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
                tier,
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
            .filter(|t| t.len() >= 2 && !is_stopword(t))
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
        // Exclude tier=2 (tests) by default. Tier=1 (utility) is included but penalised
        // in hybrid_boost so production symbols rank above them.
        let test_clause = if filters.include_tests { "" } else { "AND tier != '2'" };

        // Fetch extra for hybrid ledger reranking.
        let fetch = (limit * 4).max(80);

        // Columns: 0=symbol_id,1=language,2=kind,3=line,4=doc,
        //          5=qname_orig,6=sig_orig,7=file_orig,8=tier,9=score
        let sql = format!(
            "SELECT symbol_id, language, kind, line, doc,
                    qname_orig, sig_orig, file_orig, tier,
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
            let tier_str: String = row.get(8).unwrap_or_default();
            let tier: SymbolTier = tier_str.parse().unwrap_or(0);
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
                tier,
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

    /// Secondary file-stem scan: return one representative symbol per file
    /// whose stored path contains `token` (case-insensitive substring match).
    ///
    /// Used to inject view/render files that BM25 misses when the query term
    /// appears only in the file name and not in any indexed symbol text.
    /// Returns at most `limit` (symbol_id, qname, file) triples.
    pub fn file_stem_candidates(
        &self,
        token: &str,
        filters: &FtsFilters,
        limit: usize,
    ) -> rusqlite::Result<Vec<FtsHit>> {
        if token.len() < 2 {
            return Ok(vec![]);
        }
        let test_clause = if filters.include_tests { "" } else { "AND tier != '2'" };
        let kind_clause = filters
            .kind
            .as_deref()
            .map(|k| format!("AND kind = '{}'", k.to_lowercase().replace('\'', "")))
            .unwrap_or_default();
        // One row per distinct file_orig; pick the lowest line number (class/module
        // declaration) as the representative symbol for that file.
        // Use MIN(line) in SELECT — SQLite picks the other non-aggregated columns
        // from the same row that has the minimum, so no HAVING clause is needed.
        let sql = format!(
            "SELECT symbol_id, language, kind, MIN(line) as line, doc,
                    qname_orig, sig_orig, file_orig, tier
             FROM asd_search_fts
             WHERE lower(file_orig) LIKE '%' || lower(?1) || '%'
             {test_clause}
             {kind_clause}
             GROUP BY file_orig
             LIMIT {limit}"
        );
        let token_owned = token.to_lowercase();
        let mut stmt = self.conn.prepare(&sql)?;
        let hits = stmt
            .query_map(params![token_owned], |row| {
                let sig_orig: Option<String> = row.get(6)?;
                let tier_str: String = row.get(8).unwrap_or_default();
                let tier: SymbolTier = tier_str.parse().unwrap_or(0);
                Ok(FtsHit {
                    bm25_score: 0.0,
                    symbol_id: row.get(0)?,
                    language: row.get(1)?,
                    kind: row.get(2)?,
                    line: row.get::<_, u32>(3).unwrap_or(0),
                    doc: row.get(4)?,
                    qname: row.get(5)?,
                    signature: sig_orig.filter(|s| !s.is_empty()),
                    file: row.get(7)?,
                    tier,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(hits)
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
        * 2.5;

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

    // Utility penalty: Preview/Sample/Editor/Generated symbols ranked below production.
    let tier_penalty = if hit.tier == 1 { -2.0 } else { 0.0 };

    path_boost + name_boost + tier_penalty
}

// ---------------------------------------------------------------------------
// Staleness helpers
// ---------------------------------------------------------------------------

/// Format a unix timestamp age as a human-readable string ("3h ago", "just now").
pub fn format_age(indexed_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - indexed_at).max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Return a stale-index warning string if the index is older than
/// `threshold_secs` (default: 3600 = 1 hour), or `None` if fresh.
pub fn stale_warning(db_path: &std::path::Path, threshold_secs: u64) -> Option<String> {
    let fts = SearchFtsDb::open(db_path).ok()?;
    if !fts.has_data() {
        return Some("asd: index is empty — run 'asd index <dir>' to build it.".to_string());
    }
    let indexed_at = fts.last_indexed_at()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_secs = (now - indexed_at).max(0) as u64;
    if age_secs > threshold_secs {
        Some(format!(
            "asd: index may be stale — last indexed {}. Run 'asd index <dir>' to update.",
            format_age(indexed_at)
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Agent output trimming
// ---------------------------------------------------------------------------

/// Default token budget for `--agent` mode (8 000 tokens ≈ 32 000 chars).
pub const AGENT_DEFAULT_BUDGET: usize = 8_000;

/// Trim a JSON value for LLM consumption.
///
/// 1. Recursively removes bulk fields: `body`, `doc`, `tokens`.
/// 2. Collapses low-signal arrays (`callers`, `callees`, `notes`,
///    `decisions_and_notes`, `proofs`, `ownership`, `commits`)
///    to at most `max_list` items.
/// 3. In `callers` / `callees` arrays, keeps only `qname` + `file`.
/// 4. Limits `recently_touched` to 3 files × `max_list` commits.
///
/// Returns the trimmed value. Does **not** mutate the input.
pub fn trim_for_agent(v: &serde_json::Value, max_list: usize) -> serde_json::Value {
    use serde_json::Value;

    // Fields to drop entirely.
    const DROP_FIELDS: &[&str] = &["body", "doc", "tokens"];
    // Arrays to truncate to max_list.
    const TRUNCATE_ARRAYS: &[&str] = &[
        "callers", "callees", "notes", "decisions_and_notes",
        "proofs", "ownership", "other_ledger",
    ];
    // Arrays where we simplify each item to {qname, file} only.
    const SIMPLIFY_REFS: &[&str] = &["callers", "callees"];

    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if DROP_FIELDS.contains(&k.as_str()) {
                    continue;
                }
                let trimmed = trim_for_agent(val, max_list);
                let final_val = if TRUNCATE_ARRAYS.contains(&k.as_str()) {
                    if let Value::Array(arr) = &trimmed {
                        let slice: Vec<Value> = arr
                            .iter()
                            .take(max_list)
                            .map(|item| {
                                if SIMPLIFY_REFS.contains(&k.as_str()) {
                                    // Keep only qname + file for call graph refs.
                                    if let Value::Object(obj) = item {
                                        let mut mini = serde_json::Map::new();
                                        if let Some(q) = obj.get("qname") { mini.insert("qname".into(), q.clone()); }
                                        if let Some(f) = obj.get("file") { mini.insert("file".into(), f.clone()); }
                                        Value::Object(mini)
                                    } else {
                                        item.clone()
                                    }
                                } else {
                                    item.clone()
                                }
                            })
                            .collect();
                        let truncated = arr.len() > max_list;
                        if truncated {
                            Value::Array({
                                let mut s = slice;
                                s.push(serde_json::json!(format!("... {} more", arr.len() - max_list)));
                                s
                            })
                        } else {
                            Value::Array(slice)
                        }
                    } else {
                        trimmed
                    }
                } else if k == "recently_touched" {
                    // Cap to 3 files, each with max_list commits.
                    if let Value::Array(files) = &trimmed {
                        let capped: Vec<Value> = files.iter().take(3).map(|file_entry| {
                            if let Value::Object(obj) = file_entry {
                                let mut m = obj.clone();
                                if let Some(Value::Array(commits)) = m.get_mut("commits") {
                                    commits.truncate(max_list);
                                }
                                Value::Object(m)
                            } else {
                                file_entry.clone()
                            }
                        }).collect();
                        Value::Array(capped)
                    } else {
                        trimmed
                    }
                } else {
                    trimmed
                };
                out.insert(k.clone(), final_val);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|i| trim_for_agent(i, max_list)).collect()),
        other => other.clone(),
    }
}

/// Estimate token count from a JSON string (rough: 1 token ≈ 4 chars).
pub fn estimate_tokens(json: &str) -> usize {
    json.len().saturating_add(3) / 4
}

// ---------------------------------------------------------------------------
// Intent mode
// ---------------------------------------------------------------------------

/// Parse and validate an intent string. Returns a static str or `None`
/// if the value is unrecognised.
///
/// Valid values: `bugfix`, `feature`, `refactor`, `test`, `architecture`, `ui`.
pub fn parse_intent(s: &str) -> Option<&'static str> {
    match s.to_lowercase().as_str() {
        "bugfix"       => Some("bugfix"),
        "feature"      => Some("feature"),
        "refactor"     => Some("refactor"),
        "test"         => Some("test"),
        "architecture" => Some("architecture"),
        "ui"           => Some("ui"),
        _ => None,
    }
}

/// One-line agent guidance for each intent.
pub fn intent_focus(intent: &str) -> &'static str {
    match intent {
        "bugfix"       => "Focus: callers that may be broken, effects, invariants to preserve, affected tests.",
        "feature"      => "Focus: callees to extend, ownership boundaries, empty extension points.",
        "refactor"     => "Focus: callers (blast radius), invariants that must hold, ownership constraints.",
        "test"         => "Focus: affected test symbols, effects under test, existing proof ledger entries.",
        "architecture" => "Focus: invariants, ownership boundaries, layer grouping, cross-layer effects.",
        "ui"           => "Focus: UI/ViewModel layers, effects that touch display state, scheduler coupling.",
        _              => "",
    }
}

/// Return the preferred layer display order for an intent.
/// The standard order is used for unlisted layers.
pub fn intent_layer_order(intent: &str) -> &'static [&'static str] {
    match intent {
        "ui"           => &["ui", "viewmodel", "scheduler", "core_model", "persistence", "utility", "tests", "other"],
        "architecture" => &["core_model", "persistence", "scheduler", "viewmodel", "ui", "utility", "tests", "other"],
        "bugfix"       => &["core_model", "scheduler", "persistence", "viewmodel", "ui", "tests", "utility", "other"],
        "test"         => &["tests", "core_model", "scheduler", "persistence", "viewmodel", "ui", "utility", "other"],
        _              => &["ui", "viewmodel", "scheduler", "core_model", "persistence", "utility", "tests", "other"],
    }
}

// ---------------------------------------------------------------------------
// Recency helpers
// ---------------------------------------------------------------------------

/// Per-file recency metadata derived from a single `git log` call.
#[derive(Debug, Clone)]
pub struct FileRecency {
    /// Days since last commit on this file (fractional). `None` if git
    /// is unavailable or the file has no git history.
    pub last_touched_days: Option<f64>,
    /// True when the file was last touched within `hot_days` days.
    pub hot: bool,
}

/// Return source file paths that are modified (dirty) since the last commit.
///
/// Uses `git status --short --untracked-files=no`. Returns an empty set when
/// git is unavailable or the working tree is clean.
pub fn git_dirty_files() -> std::collections::HashSet<String> {
    use std::process::Command as Proc;
    let out = Proc::new("git")
        .args(["status", "--short", "--untracked-files=no"])
        .output();
    let Ok(o) = out else { return std::collections::HashSet::new(); };
    if !o.status.success() { return std::collections::HashSet::new(); }
    const SRC_EXTS: &[&str] = &[
        ".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go",
        ".kt", ".java", ".rb", ".cs", ".m", ".mm", ".cpp", ".c",
    ];
    String::from_utf8_lossy(&o.stdout)
        .lines()
        // git status --short lines: "XY path" — path starts at index 3
        .filter(|l| l.len() > 3 && SRC_EXTS.iter().any(|ext| l.ends_with(ext)))
        .map(|l| l[3..].trim().to_string())
        .collect()
}

/// Heuristically propose a parallel test file path for a given source file.
///
/// Replaces `Sources/` → `Tests/` (or `src/` → `tests/`) and appends `Tests`
/// to the filename stem. Falls back to `Tests/<Stem>Tests.<ext>` when no
/// recognisable source directory is found.
/// Derive behavioural test hints from a symbol's own metadata (qname, signature,
/// doc) when no invariants or effects have been recorded yet (cold-start).
///
/// Returns up to 3 hints suitable for inclusion in `suggested_test_coverage`.
/// Callers should append these after invariant/effect hints, not replace them.
pub fn derive_cold_hints(qname: &str, signature: Option<&str>, doc: Option<&str>) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();

    // 1. Doc first sentence → "verify: <sentence>"
    if let Some(d) = doc {
        let first = d.lines().next().unwrap_or("").trim();
        let first = first.split('.').next().unwrap_or("").trim();
        // Only use if it reads like a description (starts with verb-ish word).
        if first.len() > 12 {
            hints.push(format!("verify: {}", first.to_lowercase()));
        }
    }

    // 2. Function/method name words → "verify <words> behavior"
    let leaf = qname
        .split(|c: char| c == '.' || c == ':' || c == '/')
        .last()
        .unwrap_or(qname);
    let words = split_identifier_words(leaf);
    if words.len() >= 2 {
        hints.push(format!("verify {} behavior", words.join(" ").to_lowercase()));
    } else if words.len() == 1 && hints.is_empty() {
        hints.push(format!("verify {} is correct", words[0].to_lowercase()));
    }

    // 3. Parameter names from signature → "verify effect of <params>"
    if let Some(sig) = signature {
        let params = extract_sig_param_names(sig);
        if !params.is_empty() && params.len() <= 4 {
            hints.push(format!("verify effect of {} on output", params.join(", ")));
        }
    }

    hints.truncate(3);
    hints
}

/// Split a camelCase or snake_case identifier into lowercase words.
fn split_identifier_words(s: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch == '_' || ch == '-' {
            if !cur.is_empty() { words.push(cur.clone()); cur.clear(); }
        } else if ch.is_uppercase() && !cur.is_empty() {
            words.push(cur.clone());
            cur.clear();
            cur.push(ch);
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() { words.push(cur); }
    words.into_iter().filter(|w| w.len() > 1).collect()
}

/// Extract meaningful parameter names from a function signature string.
/// Handles Swift, Rust, TypeScript, Python styles heuristically.
fn extract_sig_param_names(sig: &str) -> Vec<String> {
    // Grab what's between the outermost parens.
    let inner = match (sig.find('('), sig.rfind(')')) {
        (Some(a), Some(b)) if b > a + 1 => &sig[a + 1..b],
        _ => return vec![],
    };
    let mut names: Vec<String> = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() { continue; }
        // Take first token, strip leading `_` (Swift external labels), `&`, `mut`.
        let token = part
            .split(|c: char| c.is_whitespace() || c == ':')
            .find(|t| !t.is_empty())
            .unwrap_or("")
            .trim_start_matches(['_', '&'])
            .trim_start_matches("mut ")
            .to_string();
        if token.len() > 1 && token.chars().all(|c| c.is_alphanumeric() || c == '_') {
            names.push(token.to_lowercase());
        }
    }
    names
}

pub fn propose_test_path(source_file: &str) -> String {
    let path = std::path::Path::new(source_file);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("swift");
    // Try parallel test path by substituting common source dirs.
    let candidate = source_file
        .replace("/Sources/", "/Tests/")
        .replace("/Source/", "/Tests/")
        .replace("/src/", "/tests/");
    let parent = if candidate != source_file {
        std::path::Path::new(&candidate)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("Tests")
            .to_string()
    } else {
        "Tests".to_string()
    };
    format!("{parent}/{stem}Tests.{ext}")
}

/// Run one `git log` pass covering up to `scan_commits` commits and return
/// a map of relative file path → `FileRecency`.
///
/// Uses `--name-only --pretty=format:%ct` so each commit block looks like:
/// ```text
/// <unix_timestamp>
///
/// path/to/file.swift
/// another/file.swift
/// ```
///
/// The first commit that mentions a file is its "last touched" commit.
/// `hot_days` controls the `hot` flag (files modified within that window).
pub fn gather_recency(scan_commits: usize, hot_days: f64) -> std::collections::HashMap<String, FileRecency> {
    use std::collections::HashMap;
    use std::process::Command;

    let output = Command::new("git")
        .args([
            "log",
            &format!("-n{}", scan_commits),
            "--pretty=format:%ct",
            "--name-only",
        ])
        .output();

    let Ok(out) = output else { return HashMap::new() };
    if !out.status.success() { return HashMap::new() }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0);
    let hot_secs = hot_days * 86400.0;

    let text = String::from_utf8_lossy(&out.stdout);
    let mut current_ts: Option<f64> = None;
    let mut map: HashMap<String, FileRecency> = HashMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ts) = trimmed.parse::<f64>() {
            current_ts = Some(ts);
        } else if let Some(ts) = current_ts {
            // Only record the first (most recent) commit that mentions each file.
            map.entry(trimmed.to_string()).or_insert_with(|| {
                let days = (now_secs - ts) / 86400.0;
                FileRecency {
                    last_touched_days: Some(days.max(0.0)),
                    hot: (now_secs - ts) <= hot_secs,
                }
            });
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Symbol summary extraction
// ---------------------------------------------------------------------------

/// Extract a one-line human-readable summary for a symbol.
///
/// Priority:
/// 1. First sentence of the doc comment (up to `.` / `!` / `?` / newline,
///    capped at 120 chars). Trailing punctuation stripped.
/// 2. Condensed signature (first 100 chars, trimmed).
/// 3. Empty string.
/// Strip language-specific doc comment markers from a single line.
/// Handles Rust `///`/`//!`, C/Java/Swift `/**`/`*`/`*/`, Python `#`, Haskell/SQL `--`.
fn strip_doc_prefix(line: &str) -> &str {
    let s = line.trim();
    // Multi-char prefixes first.
    for prefix in &["///", "//!", "/**", "*/", "//"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest.trim_start_matches([' ', '\t']).trim_end_matches([' ', '\t']);
        }
    }
    // Single-char: leading `*` (continuation in /** ... */), `#`, `-`
    if s.starts_with("* ") || s == "*" {
        return s[1..].trim_start_matches([' ', '\t']);
    }
    for prefix in &["#", "--"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest.trim_start_matches([' ', '\t']);
        }
    }
    s
}

pub fn extract_summary(doc: Option<&str>, signature: Option<&str>) -> String {
    if let Some(d) = doc {
        // Strip per-line doc prefixes, join, then take first sentence.
        let cleaned: String = d
            .lines()
            .map(strip_doc_prefix)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if !cleaned.is_empty() {
            let end = cleaned
                .char_indices()
                .find_map(|(i, c)| {
                    if matches!(c, '.' | '!' | '?') { Some(i + c.len_utf8()) } else { None }
                })
                .unwrap_or(cleaned.len().min(120));
            let sentence = cleaned[..end.min(cleaned.len())].trim().trim_end_matches(['.', '!', '?']);
            if !sentence.is_empty() {
                return sentence.to_string();
            }
        }
    }
    if let Some(sig) = signature {
        let trimmed = sig.trim();
        if !trimmed.is_empty() {
            return trimmed.chars().take(100).collect();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Tier classification
// ---------------------------------------------------------------------------

/// Classify a source file into a symbol tier.
///
/// - `2` Test: excluded from results by default (`--include-tests` to include).
/// - `1` Utility: Preview / Sample / Editor / Generated / Mock — included by
///        default but penalised in `hybrid_boost` so production ranks first.
/// - `0` Production: all other source files.
pub fn symbol_tier(file: &str) -> SymbolTier {
    if is_test_file(file) { 2 } else if is_utility_file(file) { 1 } else { 0 }
}

/// Load user-defined layer overrides from `.asd/layers.toml` next to `db_path`.
///
/// Returns a vec of `(path_substring, layer_name)` pairs in declaration order.
/// Unknown layer names are silently skipped. Returns an empty vec if the file
/// does not exist or cannot be parsed.
///
/// Config format:
/// ```toml
/// # .asd/layers.toml
/// [patterns]
/// "Sources/Networking" = "persistence"
/// "Sources/Features"   = "core_model"
/// "Sources/UI"         = "ui"
/// ```
pub fn load_layer_overrides(db_path: &std::path::Path) -> Vec<(String, String)> {
    let candidates = [
        db_path.parent().map(|p| p.join(".asd").join("layers.toml")),
        db_path.parent().map(|p| p.join("layers.toml")),
    ];

    for maybe_path in candidates.iter().flatten() {
        let Ok(text) = std::fs::read_to_string(maybe_path) else { continue };
        #[derive(serde::Deserialize)]
        struct LayersFile { patterns: Option<toml::Table> }
        let Ok(parsed) = toml::from_str::<LayersFile>(&text) else { continue };
        let Some(patterns) = parsed.patterns else { continue };
        let pairs: Vec<(String, String)> = patterns
            .into_iter()
            .filter_map(|(k, v)| {
                let layer = v.as_str()?;
                validate_layer_name(layer).map(|l| (k.to_lowercase(), l.to_string()))
            })
            .collect();
        if !pairs.is_empty() {
            return pairs;
        }
    }
    vec![]
}

fn validate_layer_name(name: &str) -> Option<&'static str> {
    match name {
        "ui" => Some("ui"),
        "viewmodel" => Some("viewmodel"),
        "scheduler" => Some("scheduler"),
        "core_model" => Some("core_model"),
        "persistence" => Some("persistence"),
        "utility" => Some("utility"),
        "tests" => Some("tests"),
        "other" => Some("other"),
        _ => None,
    }
}

/// Classify a source file into a workflow layer for `asd investigate` grouping.
///
/// Returns one of: `"ui"`, `"viewmodel"`, `"core_model"`, `"scheduler"`,
/// `"persistence"`, `"utility"`, `"tests"`, or `"other"`.
///
/// `overrides` is a slice of `(path_substring, layer_name)` pairs loaded from
/// `.asd/layers.toml`. User entries are checked first in order; the first
/// substring match wins. Pass `&[]` to use only built-in patterns.
///
/// Classification is purely file-path based — directory names and filename
/// suffixes are matched against common patterns for each layer. The `tier`
/// argument is used so that tier-2 (test) files always land in `"tests"` and
/// tier-1 (utility) files always land in `"utility"` regardless of path.
pub fn classify_layer(file: &str, tier: SymbolTier, overrides: &[(String, String)]) -> &'static str {
    if tier == 2 { return "tests"; }
    if tier == 1 { return "utility"; }

    // User-defined overrides take priority over built-in patterns.
    let lower = file.to_lowercase();
    for (pattern, layer) in overrides {
        if lower.contains(pattern.as_str()) {
            if let Some(l) = validate_layer_name(layer) {
                return l;
            }
        }
    }

    let segments: Vec<&str> = lower.split(|c| c == '/' || c == '\\').collect();
    let filename = segments.last().copied().unwrap_or("");
    // Strip extension to check suffix patterns.
    let stem = filename.rsplit('.').nth(1).unwrap_or(filename);

    // Check directory components (all except filename) for layer keywords.
    let dirs: Vec<&str> = segments.iter().copied().rev().skip(1).collect();

    // --- Scheduler / Engine ---
    const SCHED_DIRS: &[&str] = &[
        "scheduler", "schedulers", "engine", "engines", "audio", "audioengine",
        "transport", "render", "renderer", "pipeline", "worker", "workers",
        "processing", "realtime", "dsp", "clock",
    ];
    const SCHED_SUFFIXES: &[&str] = &[
        "scheduler", "engine", "transport", "renderer", "pipeline",
        "worker", "processor", "synthesizer", "synth", "clock", "timer",
        "compiler", "loop",
    ];
    if dirs.iter().any(|d| SCHED_DIRS.contains(d))
        || SCHED_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "scheduler";
    }

    // --- Persistence ---
    const PERSIST_DIRS: &[&str] = &[
        "storage", "database", "db", "repository", "repositories",
        "cache", "datastore", "persistence", "migration", "migrations",
        "store", "dao",
    ];
    const PERSIST_SUFFIXES: &[&str] = &[
        "repository", "store", "database", "cache", "storage",
        "dao", "datasource", "migration",
    ];
    if dirs.iter().any(|d| PERSIST_DIRS.contains(d))
        || PERSIST_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "persistence";
    }

    // --- ViewModel / Presenter / Controller ---
    const VM_DIRS: &[&str] = &[
        "viewmodels", "viewmodel", "presenters", "presenter",
        "coordinators", "coordinator", "interactors", "interactor",
        "controllers", "controller", "states", "state", "routers", "router",
    ];
    const VM_SUFFIXES: &[&str] = &[
        "viewmodel", "viewstate", "presenter", "coordinator",
        "interactor", "statemanager", "controller", "observable",
        "environment", "router",
    ];
    if dirs.iter().any(|d| VM_DIRS.contains(d))
        || VM_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "viewmodel";
    }

    // --- UI ---
    const UI_DIRS: &[&str] = &[
        "views", "view", "screens", "screen", "pages", "page",
        "components", "component", "widgets", "widget",
        "cells", "viewcontrollers", "ui", "fragments",
    ];
    const UI_SUFFIXES: &[&str] = &[
        "view", "screen", "page", "component", "widget",
        "cell", "viewcontroller", "fragment", "layout", "button",
        "label", "panel", "sheet", "modal", "overlay", "header", "footer",
    ];
    if dirs.iter().any(|d| UI_DIRS.contains(d))
        || UI_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "ui";
    }

    // --- Core Model / Domain ---
    const MODEL_DIRS: &[&str] = &[
        "models", "model", "domain", "core", "entities", "entity",
        "services", "service", "usecases", "usecase", "business",
        "logic", "features", "feature",
    ];
    const MODEL_SUFFIXES: &[&str] = &[
        "model", "entity", "service", "usecase", "manager",
        "handler", "factory", "builder", "validator",
    ];
    if dirs.iter().any(|d| MODEL_DIRS.contains(d))
        || MODEL_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "core_model";
    }

    "other"
}

/// Like [`classify_layer`] but uses the symbol's qualified name as a secondary
/// hint when the file path alone yields `"other"`. Catches common cases where a
/// class named `FooViewModel` lives in a file named after the app rather than
/// the ViewModel.
pub fn classify_layer_sym(
    file: &str,
    qname: &str,
    tier: SymbolTier,
    overrides: &[(String, String)],
) -> &'static str {
    let layer = classify_layer(file, tier, overrides);
    if layer != "other" {
        return layer;
    }
    // Secondary: walk all qname components (split on `.`, `::`, `/`).
    // For method qnames like `ExampleFlowViewModel.refreshDriftPlayhead` the
    // method leaf won't match, but the class component will — so we check every
    // component and return the *highest-priority* layer found across any of them.
    let components: Vec<&str> = qname
        .split(|c| c == '.' || c == ':' || c == '/')
        .filter(|s| !s.is_empty())
        .collect();

    let mut found_viewmodel = false;
    let mut found_ui = false;
    let mut found_scheduler = false;
    let mut found_persistence = false;
    let mut found_core_model = false;

    for component in &components {
        let n = component.to_lowercase();
        if n.ends_with("viewmodel") || n.ends_with("controller") || n.ends_with("presenter")
            || n.ends_with("coordinator") || n.ends_with("interactor") || n.ends_with("viewstate")
            || n.ends_with("statemanager") || n.ends_with("router")
        {
            found_viewmodel = true;
        } else if n.ends_with("view") || n.ends_with("screen") || n.ends_with("page")
            || n.ends_with("cell") || n.ends_with("widget") || n.ends_with("button")
            || n.ends_with("label") || n.ends_with("panel") || n.ends_with("viewcontroller")
            || n.ends_with("sheet") || n.ends_with("overlay") || n.ends_with("header")
        {
            found_ui = true;
        } else if n.ends_with("scheduler") || n.ends_with("engine") || n.ends_with("compiler")
            || n.ends_with("processor") || n.ends_with("renderer") || n.ends_with("clock")
            || n.ends_with("timer") || n.ends_with("pipeline") || n.ends_with("worker")
            || n.ends_with("synthesizer") || n.ends_with("transport")
        {
            found_scheduler = true;
        } else if n.ends_with("repository") || n.ends_with("store") || n.ends_with("cache")
            || n.ends_with("dao") || n.ends_with("database") || n.ends_with("datasource")
        {
            found_persistence = true;
        } else if n.ends_with("model") || n.ends_with("entity") || n.ends_with("service")
            || n.ends_with("manager") || n.ends_with("handler") || n.ends_with("factory")
            || n.ends_with("validator") || n.ends_with("usecase") || n.ends_with("builder")
        {
            found_core_model = true;
        }
    }

    // Return in priority order: most specific wins.
    if found_viewmodel { return "viewmodel"; }
    if found_ui { return "ui"; }
    if found_scheduler { return "scheduler"; }
    if found_persistence { return "persistence"; }
    if found_core_model { return "core_model"; }
    "other"
}

/// Heuristically determine whether a symbol's source file is a utility file
/// (Preview, Sample, Editor extension, Generated, Mock, Stub, Fixture, Demo).
/// These are compiled into the app but are not production logic.
fn is_utility_file(file: &str) -> bool {
    let lower = file.to_lowercase();
    let segments: Vec<&str> = lower.split(|c| c == '/' || c == '\\').collect();

    // Directory names that indicate non-production utility trees.
    const UTILITY_DIRS: &[&str] = &[
        "previews", "preview", "samples", "sample", "sampledata", "examples", "example",
        "mocks", "mock", "stubs", "stub", "fixtures", "fixture",
        "demo", "demos", "editor", "editors", "generated", "gen",
        "sandbox", "playground", "playgrounds",
    ];
    // Also match compound directory names that start with a utility prefix.
    const UTILITY_PREFIXES: &[&str] = &["mock", "stub", "fake", "sample", "preview", "generated"];
    if segments.iter().rev().skip(1).any(|s| {
        UTILITY_DIRS.contains(s) || UTILITY_PREFIXES.iter().any(|p| s.starts_with(p))
    }) {
        return true;
    }

    // Filename patterns.
    if let Some(filename) = segments.last() {
        // Swift Previews: FooView_Previews.swift, FooPreview.swift
        if filename.contains("preview") || filename.contains("_previews") {
            return true;
        }
        // SwiftUI canvas / sample data
        if filename.contains("sampledata") || filename.contains("sample_data") {
            return true;
        }
        // Generated files: Foo.generated.swift, Foo.g.swift, FooGenerated.swift
        if filename.contains(".generated.") || filename.contains(".g.") || filename.ends_with("generated.swift") {
            return true;
        }
        // Mock/Stub/Fake: MockFoo.swift, FooMock.kt, FakeBar.ts
        if filename.starts_with("mock") || filename.contains("_mock.")
            || filename.starts_with("stub") || filename.contains("_stub.")
            || filename.starts_with("fake") || filename.contains("_fake.")
        {
            return true;
        }
        // Xcode Playground files
        if filename.ends_with(".playground") {
            return true;
        }
    }

    false
}

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
    fn stopword_filtering() {
        // Pure stopword queries produce no tokens → empty results, not a panic.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();
        use crate::schema::{Position, Symbol, SymbolKind};
        let sym = Symbol {
            symbol_id: "s1".into(), symbol_fp: "fp".into(),
            qname: "App.Foo.punchIn".into(), language: "swift".into(),
            kind: SymbolKind::Method,
            file: "App/Foo.swift".into(),
            start: Position { line: 1, col: 0 }, end: Position { line: 5, col: 0 },
            signature: Some("func punchIn(over existingClip: Clip)".into()),
            doc: None,
        };
        fts.rebuild(std::slice::from_ref(&sym)).unwrap();

        // "over" alone is a stopword — filtered → empty tokens → no results.
        let hits = fts.search("over", &FtsFilters::default(), 10).unwrap();
        assert!(hits.is_empty(), "'over' is a stopword, should return no hits");

        // "playhead over clips" — only "playhead" and "clips" survive filtering.
        // "punchIn(over existingClip)" has no "playhead" or "clips" → no match.
        let hits2 = fts.search("playhead over clips", &FtsFilters::default(), 10).unwrap();
        assert!(hits2.is_empty(), "stopword-only overlap should not match punchIn");
    }

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
    fn tier_classification() {
        // Production — tier 0
        assert_eq!(symbol_tier("App/ViewModel/DriftPlayheadViewModel.swift"), 0);
        assert_eq!(symbol_tier("src/payments/charge.py"), 0);
        assert_eq!(symbol_tier("Packages/AudioEngine/Sources/KarplusStrong.swift"), 0);

        // Utility — tier 1
        assert_eq!(symbol_tier("App/Previews/DriftView_Previews.swift"), 1);
        assert_eq!(symbol_tier("App/ViewModel/DriftView_Previews.swift"), 1, "preview filename");
        assert_eq!(symbol_tier("Mocks/MockAudioEngine.swift"), 1);
        assert_eq!(symbol_tier("App/SampleData/TimelineFixture.swift"), 1);
        assert_eq!(symbol_tier("App/Generated/Schema.generated.swift"), 1);

        // Test — tier 2
        assert_eq!(symbol_tier("Tests/DriftTests/PlayheadTests.swift"), 2);
        assert_eq!(symbol_tier("tests/test_charge.py"), 2);
        assert_eq!(symbol_tier("pkg/payments/charge_test.go"), 2);
    }

    #[test]
    fn utility_symbols_rank_below_production() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();

        use crate::schema::{Position, Symbol, SymbolKind};
        let make = |id: &str, qname: &str, file: &str| Symbol {
            symbol_id: id.into(), symbol_fp: format!("fp_{id}"),
            qname: qname.into(), language: "swift".into(),
            kind: SymbolKind::Method, file: file.into(),
            start: Position { line: 1, col: 0 }, end: Position { line: 5, col: 0 },
            signature: None, doc: None,
        };

        let prod = make("prod", "App.VM.refreshDriftPlayhead", "App/ViewModel.swift");
        let preview = make("preview", "App.Previews.refreshDriftPlayhead", "App/Previews/DriftView_Previews.swift");

        fts.rebuild(&[prod, preview]).unwrap();

        let hits = fts.search("refresh drift playhead", &FtsFilters::default(), 10).unwrap();
        assert_eq!(hits.len(), 2, "both prod and utility included by default");
        // Production should rank first due to tier penalty on utility.
        assert_eq!(hits[0].symbol_id, "prod", "production ranks before preview");
        assert_eq!(hits[0].tier, 0);
        assert_eq!(hits[1].tier, 1);
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
        assert_eq!(hits[0].tier, 0, "production file should be tier 0");

        let hits2 = fts.search("refresh drift", &FtsFilters::default(), 10).unwrap();
        assert!(!hits2.is_empty(), "should find multi-token");

        let hits3 = fts.search(
            "playhead",
            &FtsFilters { language: Some("python".into()), ..Default::default() },
            10,
        ).unwrap();
        assert!(hits3.is_empty(), "language filter should exclude swift");
    }

    #[test]
    fn summary_extraction() {
        // First sentence from doc.
        assert_eq!(
            extract_summary(Some("Updates drift pad visual playhead from transport state. Called on every render tick."), None),
            "Updates drift pad visual playhead from transport state"
        );
        // Doc with newline — joined, first sentence extracted.
        assert_eq!(
            extract_summary(Some("Short summary.\nMore detail here."), None),
            "Short summary"
        );
        // Falls back to signature when doc absent.
        assert_eq!(
            extract_summary(None, Some("func refreshDriftPlayhead() -> Void")),
            "func refreshDriftPlayhead() -> Void"
        );
        // Returns empty when neither present.
        assert_eq!(extract_summary(None, None), "");

        // Doc prefix stripping — Rust triple-slash.
        assert_eq!(
            extract_summary(Some("/// Updates the drift playhead. Called each frame."), None),
            "Updates the drift playhead"
        );
        // Multi-line Rust doc block.
        assert_eq!(
            extract_summary(Some("/// Refreshes visual state.\n/// Must be called on main thread."), None),
            "Refreshes visual state"
        );
        // JavaDoc / Swift style — trailing dot stripped.
        assert_eq!(
            extract_summary(Some("/** Computes clip boundaries. */"), None),
            "Computes clip boundaries"
        );
        // Python # prefix.
        assert_eq!(
            extract_summary(Some("# Process the audio buffer."), None),
            "Process the audio buffer"
        );
    }

    #[test]
    fn layer_classification() {
        assert_eq!(classify_layer("App/Views/DriftPlayheadView.swift", 0, &[]), "ui");
        assert_eq!(classify_layer("App/ViewModel/DriftPlayheadViewModel.swift", 0, &[]), "viewmodel");
        assert_eq!(classify_layer("App/Scheduler/DriftScheduler.swift", 0, &[]), "scheduler");
        assert_eq!(classify_layer("App/Engine/AudioEngine.swift", 0, &[]), "scheduler");
        assert_eq!(classify_layer("App/Storage/ClipRepository.swift", 0, &[]), "persistence");
        assert_eq!(classify_layer("App/Domain/Clip.swift", 0, &[]), "core_model");
        assert_eq!(classify_layer("App/Previews/DriftView_Previews.swift", 1, &[]), "utility");
        assert_eq!(classify_layer("Tests/DriftTests.swift", 2, &[]), "tests");
        assert_eq!(classify_layer("App/AppDelegate.swift", 0, &[]), "other");
    }

    #[test]
    fn layer_classification_qname_fallback() {
        // File named after app, but qname carries the ViewModel suffix.
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "ExampleFlowViewModel", 0, &[]), "viewmodel");
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "ExampleFlowController", 0, &[]), "viewmodel");
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "DriftCompiler", 0, &[]), "scheduler");
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "ClipStore", 0, &[]), "persistence");
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "DriftEngine", 0, &[]), "scheduler");
        // File-based classification still wins when it fires.
        assert_eq!(classify_layer_sym("App/Views/DriftView.swift", "DriftViewModel", 0, &[]), "ui");
        // Truly unclassifiable stays other.
        assert_eq!(classify_layer_sym("App/AppDelegate.swift", "AppDelegate", 0, &[]), "other");
        // Method qnames: class component must propagate even when the method leaf doesn't match.
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "ExampleFlowViewModel.refreshDriftPlayhead", 0, &[]), "viewmodel");
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "DriftCompiler.compile", 0, &[]), "scheduler");
        assert_eq!(classify_layer_sym("App/ExampleFlow.swift", "ClipStore.save", 0, &[]), "persistence");
        // Rust-style :: separators.
        assert_eq!(classify_layer_sym("src/drift.rs", "ExampleFlowViewModel::refresh", 0, &[]), "viewmodel");
    }

    #[test]
    fn layer_overrides_respected() {
        // Keys in overrides must be lowercase (as produced by load_layer_overrides).
        let overrides = vec![
            ("custominfra".to_string(), "persistence".to_string()),
            ("bizlogic".to_string(), "core_model".to_string()),
        ];
        assert_eq!(classify_layer("App/CustomInfra/Repo.swift", 0, &overrides), "persistence");
        assert_eq!(classify_layer("App/BizLogic/Workflow.swift", 0, &overrides), "core_model");
        // Non-matching path falls through to built-in rules.
        assert_eq!(classify_layer("App/Views/Foo.swift", 0, &overrides), "ui");
    }

    #[test]
    fn load_layer_overrides_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let overrides = load_layer_overrides(&tmp.path().join("asd.db"));
        assert!(overrides.is_empty());
    }

    #[test]
    fn load_layer_overrides_parses_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let asd_dir = tmp.path().join(".asd");
        std::fs::create_dir_all(&asd_dir).unwrap();
        std::fs::write(
            asd_dir.join("layers.toml"),
            "[patterns]\nInfra = \"persistence\"\nBusiness = \"core_model\"\n",
        ).unwrap();
        // load_layer_overrides takes the db_path (not the project root) and uses its parent.
        let overrides = load_layer_overrides(&tmp.path().join("asd.db"));
        assert_eq!(overrides.len(), 2);
        assert!(overrides.contains(&("infra".to_string(), "persistence".to_string())));
        assert!(overrides.contains(&("business".to_string(), "core_model".to_string())));
    }

    #[test]
    fn intent_parsing() {
        assert_eq!(parse_intent("bugfix"), Some("bugfix"));
        assert_eq!(parse_intent("ARCHITECTURE"), Some("architecture"));
        assert_eq!(parse_intent("ui"), Some("ui"));
        assert_eq!(parse_intent("typo"), None);
        // Each valid intent has non-empty focus guidance.
        for i in &["bugfix", "feature", "refactor", "test", "architecture", "ui"] {
            assert!(!intent_focus(i).is_empty(), "no focus for {i}");
        }
        // Layer order has 8 entries for every intent.
        for i in &["bugfix", "feature", "refactor", "test", "architecture", "ui", ""] {
            assert_eq!(intent_layer_order(i).len(), 8);
        }
    }

    #[test]
    fn gather_recency_no_crash_outside_repo() {
        // gather_recency must not panic even if git is unavailable or returns non-zero.
        // In a git repo this should return a non-empty map; outside one it returns empty.
        // Either way it must not panic.
        let result = gather_recency(10, 14.0);
        // No assertion on content — just verify it didn't panic and types are correct.
        for (file, rec) in &result {
            assert!(!file.is_empty());
            if let Some(days) = rec.last_touched_days {
                assert!(days >= 0.0);
            }
        }
    }
}
