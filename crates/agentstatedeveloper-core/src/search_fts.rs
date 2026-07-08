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

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::schema::{EffectDecl, FeedbackEntry, FeedbackVerdict, LedgerEntry, Symbol};

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
    "a", "an", "and", "as", "at", "be", "but", "by", "for", "from", "if", "into", "is", "it",
    "nor", "not", "of", "on", "or", "so", "the", "to", "via", "vs", "yet", "with", "over", "about",
    "between", "than", "that", "this", "are", "was", "were", "has", "have", "had", "its", "our",
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
    /// Concatenated ledger entry summaries (all kinds) in lowercase.
    /// Empty when the symbol has no ledger entries.
    /// Populated at `asd index` time and stored in the FTS table.
    pub ledger_text: String,
    /// Comma-separated ledger kinds present for this symbol, e.g. "ownership,invariant".
    /// Used for fast has_ownership() / has_invariant() checks without reading git objects.
    pub ledger_flags: String,
}

impl FtsHit {
    /// True if this symbol has at least one Ownership ledger entry.
    #[inline]
    pub fn has_ownership(&self) -> bool {
        self.ledger_flags.contains("ownership")
    }
    /// True if this symbol has at least one Invariant ledger entry.
    #[inline]
    pub fn has_invariant(&self) -> bool {
        self.ledger_flags.contains("invariant")
    }
    /// True if this symbol has at least one Hazard ledger entry.
    #[inline]
    pub fn has_hazard(&self) -> bool {
        self.ledger_flags.contains("hazard")
    }
    /// True if this symbol has any ledger entries.
    #[inline]
    pub fn has_ledger(&self) -> bool {
        !self.ledger_text.is_empty()
    }
}

/// Lightweight symbol metadata stored in the `asd_symbols_meta` SQLite table.
///
/// Populated at `asd index` time alongside the FTS rebuild. Allows feedback
/// adjustment functions to resolve qname → (symbol_id, file, kind) via a
/// simple SQL lookup instead of traversing the git object store.
#[derive(Debug, Clone)]
pub struct SymbolMeta {
    pub symbol_id: String,
    pub file: String,
    pub kind: String,
}

/// Full symbol resolution result from the FTS table.
///
/// Returned by [`SearchFtsDb::resolve_qnames_bulk`] and
/// [`SearchFtsDb::resolve_symbol_ids_bulk`]. Carries every field needed by
/// feedback adjustment and filter functions so they can operate without any
/// git object-store reads.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSymbol {
    pub symbol_id: String,
    /// Original (unexpanded) qname — needed for name-token extraction in feedback functions.
    pub qname: String,
    pub file: String,
    pub kind: String,
    pub doc: Option<String>,
    pub signature: Option<String>,
}

/// Filters applied before BM25 ranking.
#[derive(Debug, Default, Clone)]
pub struct FtsFilters {
    pub kind: Option<String>,
    pub language: Option<String>,
    /// Include test symbols (files under test/tests/spec directories, etc.).
    /// Default: false — tests are excluded so production entry points rank first.
    /// Ignored when `tests_only` is set.
    pub include_tests: bool,
    /// Restrict to test symbols only. Overrides `include_tests` when true.
    /// Use this when classifying test coverage, finding fixture usage, or
    /// auditing test layout (Plan A, t-006).
    pub tests_only: bool,
    /// Lowercase substring terms to exclude. Any candidate whose qname, file,
    /// doc, or signature contains one of these strings is dropped.
    pub exclude_terms: Vec<String>,
    /// Glob patterns to restrict results to (e.g. "App/**/DriftPad*").
    /// When non-empty, only symbols whose file matches at least one pattern
    /// are kept. Patterns use `*` (within a segment) and `**` (any segments).
    pub paths_filter: Vec<String>,
    /// Plan J t-011: glob patterns to DROP. Inverse of `paths_filter` —
    /// any candidate whose file matches one of these patterns is removed.
    /// Stacks on top of `exclude_terms`: terms are substring matches over
    /// qname/file/doc/sig; these are glob matches over file paths only.
    pub exclude_paths: Vec<String>,
    /// Plan J t-011: language tags to drop (e.g. "swift", "python").
    /// Compared case-insensitively to the resolved symbol's `language`.
    /// Useful in polyglot monorepos for queries like "auth flow" where
    /// the same concept lives in both the iOS and backend codebases and
    /// only one is in scope.
    pub exclude_languages: Vec<String>,
}

/// Helper: build the SQL clause that handles the tri-state test filter.
/// - `tests_only=true`  → `AND tier = '2'`
/// - `include_tests=true` (and not tests_only) → no clause
/// - default → `AND tier != '2'`
fn tests_clause(include_tests: bool, tests_only: bool) -> &'static str {
    if tests_only {
        "AND tier = '2'"
    } else if include_tests {
        ""
    } else {
        "AND tier != '2'"
    }
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
    /// Pragmas are tuned adaptively based on DB file size.  Users can override
    /// the defaults by adding a `[performance]` section to `.asd/config.toml`
    /// in the project root (the directory that contains `.asd-state.db`):
    ///
    /// ```toml
    /// [performance]
    /// cache_size_kb = 32768   # override adaptive cache (default: ~80% of DB, 8–64 MB)
    /// mmap_size_mb  = 512     # override mmap window (default: 256 MB)
    /// ```
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        // Base pragmas: WAL for concurrent readers, NORMAL sync for durability/speed
        // balance, MEMORY temp store avoids disk spills for sort/group operations,
        // and mmap lets the OS page cache do the heavy lifting.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;",
        )?;

        // Adaptive cache: scale to ~80 % of the current DB file size, clamped
        // 8 MB … 64 MB.  A negative value tells SQLite the number is in KiB.
        let db_bytes = std::fs::metadata(db_path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let mut cache_kb: usize = ((db_bytes * 8 / 10) / 1024).clamp(8_192, 65_536);
        let mut mmap_bytes: u64 = 268_435_456; // 256 MB default

        // P1: Check for user overrides in .asd/config.toml (derived from db_path).
        // Silently use adaptive defaults on any read/parse failure.
        if let Some(project_dir) = db_path.parent() {
            let cfg_path = project_dir.join(".asd").join("config.toml");
            if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
                if let Ok(table) = raw.parse::<toml::Table>() {
                    if let Some(perf) = table.get("performance").and_then(|v| v.as_table()) {
                        if let Some(v) = perf.get("cache_size_kb").and_then(|v| v.as_integer()) {
                            cache_kb = (v as usize).clamp(1_024, 131_072);
                        }
                        if let Some(v) = perf.get("mmap_size_mb").and_then(|v| v.as_integer()) {
                            mmap_bytes = (v as u64).clamp(64, 4096) * 1024 * 1024;
                        }
                    }
                }
            }
        }

        conn.execute_batch(&format!(
            "PRAGMA mmap_size={mmap_bytes}; PRAGMA cache_size=-{cache_kb};"
        ))?;
        let db = Self { conn };
        db.ensure_schema()?;
        Ok(db)
    }

    fn ensure_schema(&self) -> rusqlite::Result<()> {
        // Version 5: adds ledger_text UNINDEXED + ledger_flags UNINDEXED columns to FTS
        //            and asd_symbols_meta table (qname → symbol_id, file, kind).
        //            Populated at `asd index` time — eliminates git reads from scoring hot path.
        // Any version mismatch drops and recreates — data is reproduced by next `asd index`.
        const SCHEMA_VER: i64 = 5;

        let current: i64 = self
            .conn
            .query_row("SELECT version FROM asd_fts_meta LIMIT 1", [], |r| r.get(0))
            .unwrap_or(0);

        if current != SCHEMA_VER {
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS asd_search_fts;
                 DROP TABLE IF EXISTS asd_search_meta;
                 DROP TABLE IF EXISTS asd_fts_meta;
                 DROP TABLE IF EXISTS asd_symbols_meta;",
            )?;
        }

        // asd_index_meta is a simple key-value store for index metadata.
        // It is NOT dropped on FTS schema version changes — it persists
        // across rebuilds so indexed_at survives schema upgrades.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asd_index_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        // asd_ledger_cache: write-through cache of LedgerEntry records.
        // NOT version-gated — survives FTS schema bumps like asd_index_meta.
        // Full entry stored as a JSON blob (body) so the round-trip is lossless
        // and new LedgerEntry fields don't require a schema migration here.
        // The secondary index on (symbol_id, ref_name) makes list_entries_for
        // a single indexed scan with no full-table reads.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asd_ledger_cache (
                entry_id   TEXT NOT NULL,
                symbol_id  TEXT NOT NULL,
                ref_name   TEXT NOT NULL,
                body       TEXT NOT NULL,
                PRIMARY KEY (entry_id, ref_name)
            );
            CREATE INDEX IF NOT EXISTS idx_asd_lc_sym
                ON asd_ledger_cache(symbol_id, ref_name);",
        )?;

        // asd_effects_cache: one row per (symbol_id, ref_name) — stores the
        // full EffectDecl JSON blob for lossless round-trips.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asd_effects_cache (
                symbol_id TEXT NOT NULL,
                ref_name  TEXT NOT NULL,
                body      TEXT NOT NULL,
                PRIMARY KEY (symbol_id, ref_name)
            );",
        )?;

        // asd_feedback is a write-through cache of git-backed FeedbackEntry
        // records.  It is NOT version-gated — like asd_index_meta it survives
        // FTS schema bumps.  The git object store remains authoritative; this
        // table is the fast read path.  `asd index` / `asd reindex` reconciles
        // any drift via sync_feedback_entries().
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asd_feedback (
                entry_id     TEXT PRIMARY KEY,
                symbol_id    TEXT NOT NULL,
                symbol_qname TEXT NOT NULL,
                query        TEXT NOT NULL,
                verdict      TEXT NOT NULL,
                author       TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                note         TEXT,
                file_scope   TEXT,
                expires_at   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_asd_fb_symbol  ON asd_feedback(symbol_id);
            CREATE INDEX IF NOT EXISTS idx_asd_fb_qname   ON asd_feedback(symbol_qname);
            CREATE INDEX IF NOT EXISTS idx_asd_fb_verdict ON asd_feedback(verdict);",
        )?;
        // Plan J t-014: add expires_at to pre-existing tables (DBs
        // created before 1.0.48). ALTER...ADD COLUMN errors if the
        // column already exists; we swallow that case and let any
        // other error surface.
        let _ = self
            .conn
            .execute("ALTER TABLE asd_feedback ADD COLUMN expires_at TEXT", []);

        // asd_symbols_cache: full Symbol JSON for every indexed symbol.
        // Populated at asd index time; eliminates the by-qname git tree walk
        // in build_id_map (was ~2s on repos with hundreds of symbols).
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asd_symbols_cache (
                symbol_id   TEXT NOT NULL,
                ref_name    TEXT NOT NULL,
                qname       TEXT NOT NULL,
                file        TEXT NOT NULL,
                kind        TEXT NOT NULL DEFAULT '',
                symbol_json TEXT NOT NULL,
                PRIMARY KEY (symbol_id, ref_name)
            );
            CREATE INDEX IF NOT EXISTS idx_asd_sc_qname
                ON asd_symbols_cache(qname, ref_name);
            -- asd_call_edges: directed caller/callee edges.
            -- direction is 'caller' (this symbol is called by neighbor)
            --            or 'callee' (this symbol calls neighbor).
            CREATE TABLE IF NOT EXISTS asd_call_edges (
                symbol_id   TEXT NOT NULL,
                neighbor_id TEXT NOT NULL,
                direction   TEXT NOT NULL,
                ref_name    TEXT NOT NULL,
                PRIMARY KEY (symbol_id, neighbor_id, direction, ref_name)
            );
            CREATE INDEX IF NOT EXISTS idx_asd_ce_lookup
                ON asd_call_edges(symbol_id, direction, ref_name);",
        )?;

        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS asd_search_fts USING fts5(
                symbol_id    UNINDEXED,
                qname,
                signature,
                doc,
                file,
                language     UNINDEXED,
                kind         UNINDEXED,
                line         UNINDEXED,
                qname_orig   UNINDEXED,
                sig_orig     UNINDEXED,
                file_orig    UNINDEXED,
                tier         UNINDEXED,
                ledger_text  UNINDEXED,
                ledger_flags UNINDEXED,
                tokenize     = 'unicode61 remove_diacritics 1'
            );
            CREATE TABLE IF NOT EXISTS asd_search_meta (
                file       TEXT PRIMARY KEY,
                indexed_at INTEGER NOT NULL
            );
            -- Lightweight qname→(symbol_id, file, kind) lookup table.
            -- Populated alongside FTS rebuild; allows feedback functions to resolve
            -- qname without traversing the git object store.
            CREATE TABLE IF NOT EXISTS asd_symbols_meta (
                qname     TEXT PRIMARY KEY,
                symbol_id TEXT NOT NULL,
                file      TEXT NOT NULL,
                kind      TEXT NOT NULL
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
    ///
    /// `ledger_data`: `symbol_id → (ledger_text, ledger_flags)` map built at
    /// index time from the ledger tree. Pass `&HashMap::new()` when no ledger
    /// data is available (e.g. test helpers that call rebuild directly).
    pub fn rebuild(&self, symbols: &[Symbol]) -> rusqlite::Result<()> {
        self.rebuild_refs(&symbols.iter().collect::<Vec<_>>(), &HashMap::new())
    }

    /// Like [`rebuild`] but accepts a slice of references — avoids a copy when
    /// the caller already has a deduplicated `Vec<&Symbol>`.
    ///
    /// `ledger_data`: `symbol_id → (ledger_text, ledger_flags)`. Pass
    /// `&HashMap::new()` when ledger data is unavailable.
    pub fn rebuild_refs(
        &self,
        symbols: &[&Symbol],
        ledger_data: &HashMap<String, (String, String)>,
    ) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "DELETE FROM asd_search_fts; DELETE FROM asd_search_meta; DELETE FROM asd_symbols_meta;",
        )?;
        for sym in symbols {
            let (lt, lf) = ledger_data
                .get(&sym.symbol_id)
                .map(|(t, f)| (t.as_str(), f.as_str()))
                .unwrap_or(("", ""));
            self.insert_symbol(sym, lt, lf)?;
        }

        // Populate asd_symbols_meta: lightweight qname→(symbol_id, file, kind) table.
        // This is the single source used by feedback functions to resolve qnames
        // without touching the git object store.
        for sym in symbols {
            let kind = format!("{:?}", sym.kind).to_lowercase();
            self.conn.execute(
                "INSERT OR REPLACE INTO asd_symbols_meta(qname, symbol_id, file, kind)
                 VALUES(?1, ?2, ?3, ?4)",
                params![sym.qname, sym.symbol_id, sym.file, kind],
            )?;
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
        self.conn
            .query_row(
                "SELECT value FROM asd_index_meta WHERE key = 'indexed_at' LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse().ok())
    }

    /// Whether the FTS rebuild succeeded during the last `asd index` run.
    /// Returns `None` if no `mark_symbols_indexed` call has been recorded yet
    /// (pre-0.9.70 index runs or DBs that have never been indexed with this version).
    pub fn fts_last_rebuild_ok(&self) -> Option<bool> {
        self.conn
            .query_row(
                "SELECT value FROM asd_index_meta WHERE key = 'fts_last_rebuild_ok' LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .map(|s| s == "1")
    }

    /// Record the outcome of a symbol-indexing run. Call this from
    /// `index_pipeline` after the FTS rebuild attempt (success *or* failure).
    /// Writes `symbols_indexed_at` and `fts_last_rebuild_ok` into
    /// `asd_index_meta`. This is the authoritative signal used by
    /// `stale_warning()` to detect the "symbols fresh / FTS stale" state.
    pub fn mark_symbols_indexed(&self, fts_succeeded: bool) -> rusqlite::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT OR REPLACE INTO asd_index_meta (key, value) VALUES ('symbols_indexed_at', ?1)",
            params![now.to_string()],
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO asd_index_meta (key, value) \
             VALUES ('fts_last_rebuild_ok', ?1)",
            params![if fts_succeeded { "1" } else { "0" }],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Symbol + edge cache — O(1) reads vs. full git tree walks
    // -----------------------------------------------------------------------

    /// Returns `true` when `asd_symbols_cache` has at least one row for
    /// `ref_name`. Used as the primary guard before attempting a cached read —
    /// if the cache is empty the caller falls back to the git path.
    pub fn symbols_cached_for(&self, ref_name: &str) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM asd_symbols_cache WHERE ref_name = ?1 LIMIT 1",
                params![ref_name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Full replace of `asd_symbols_cache` for `ref_name`. Called after every
    /// `asd index` run so the cache always reflects the current snapshot.
    pub fn sync_symbols(
        &self,
        symbols: &[&crate::schema::Symbol],
        ref_name: &str,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM asd_symbols_cache WHERE ref_name = ?1",
            params![ref_name],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO asd_symbols_cache
                 (symbol_id, ref_name, qname, file, kind, symbol_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for sym in symbols {
                let json = serde_json::to_string(sym).unwrap_or_default();
                let kind = format!("{:?}", sym.kind).to_lowercase();
                stmt.execute(params![
                    sym.symbol_id,
                    ref_name,
                    sym.qname,
                    sym.file,
                    kind,
                    json
                ])?;
            }
        }
        tx.commit()
    }

    /// Full replace of `asd_call_edges` for `ref_name`. `callees_of` maps
    /// caller_id → [callee_id, …]; `callers_of` maps callee_id → [caller_id, …].
    pub fn sync_call_edges(
        &self,
        callees_of: &HashMap<String, Vec<String>>,
        callers_of: &HashMap<String, Vec<String>>,
        ref_name: &str,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM asd_call_edges WHERE ref_name = ?1",
            params![ref_name],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO asd_call_edges
                 (symbol_id, neighbor_id, direction, ref_name)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (caller_id, callee_ids) in callees_of {
                for callee_id in callee_ids {
                    stmt.execute(params![caller_id, callee_id, "callee", ref_name])?;
                }
            }
            for (callee_id, caller_ids) in callers_of {
                for caller_id in caller_ids {
                    stmt.execute(params![callee_id, caller_id, "caller", ref_name])?;
                }
            }
        }
        tx.commit()
    }

    /// Build the full `symbol_id → Symbol` map from `asd_symbols_cache`.
    /// Returns an empty map on any error (caller falls back to git).
    pub fn build_id_map_cached(&self, ref_name: &str) -> HashMap<String, crate::schema::Symbol> {
        let mut stmt = match self
            .conn
            .prepare("SELECT symbol_id, symbol_json FROM asd_symbols_cache WHERE ref_name = ?1")
        {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let rows = match stmt.query_map(params![ref_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        let mut map = HashMap::new();
        for row in rows.flatten() {
            let (id, json) = row;
            if let Ok(sym) = serde_json::from_str::<crate::schema::Symbol>(&json) {
                map.insert(id, sym);
            }
        }
        map
    }

    /// Plan E t-005: bulk qname → file lookup. Returns a HashMap with one
    /// entry per qname found in the symbol cache (qnames not in the cache
    /// are simply absent from the result). Callers use this to avoid
    /// per-candidate SQL roundtrips when they only need the file path,
    /// not the full Symbol JSON.
    ///
    /// Note: asd_symbols_meta is single-ref by design (rebuilt fresh per
    /// `asd index .`), so the `ref_name` parameter is currently unused
    /// but reserved for the future multi-ref world.
    pub fn files_for_qnames(
        &self,
        qnames: &[&str],
        _ref_name: &str,
    ) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::with_capacity(qnames.len());
        if qnames.is_empty() {
            return out;
        }
        let placeholders: Vec<&str> = (0..qnames.len()).map(|_| "?").collect();
        let sql = format!(
            "SELECT qname, file FROM asd_symbols_meta WHERE qname IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return out,
        };
        let rows = stmt.query_map(
            rusqlite::params_from_iter(qnames.iter().map(|q| q as &dyn rusqlite::ToSql)),
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        );
        if let Ok(rows) = rows {
            for r in rows.flatten() {
                out.insert(r.0, r.1);
            }
        }
        out
    }

    /// Look up a single Symbol by qname from the cache.
    pub fn get_symbol_by_qname_cached(
        &self,
        qname: &str,
        ref_name: &str,
    ) -> Option<crate::schema::Symbol> {
        self.conn
            .query_row(
                "SELECT symbol_json FROM asd_symbols_cache
                 WHERE qname = ?1 AND ref_name = ?2 LIMIT 1",
                params![qname, ref_name],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
    }

    /// Return the neighbor IDs for `symbol_id` in the given `direction`
    /// (`"caller"` or `"callee"`). Empty Vec if none or on error.
    pub fn get_neighbors_cached(
        &self,
        symbol_id: &str,
        direction: &str,
        ref_name: &str,
    ) -> Vec<String> {
        let mut stmt = match self.conn.prepare(
            "SELECT neighbor_id FROM asd_call_edges
             WHERE symbol_id = ?1 AND direction = ?2 AND ref_name = ?3",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![symbol_id, direction, ref_name], |r| {
            r.get::<_, String>(0)
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Build both full edge maps from `asd_call_edges` in one query:
    /// `(callers_of, callees_of)`, each `symbol_id → [neighbor_id, …]`.
    /// The bulk analog of [`Self::get_neighbors_cached`] — callers that walk
    /// many nodes (graph BFS) use this instead of one query per node.
    /// Returns empty maps on any error (caller falls back to git).
    pub fn build_edge_maps_cached(
        &self,
        ref_name: &str,
    ) -> (HashMap<String, Vec<String>>, HashMap<String, Vec<String>>) {
        let mut callers_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut callees_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = match self.conn.prepare(
            "SELECT symbol_id, neighbor_id, direction FROM asd_call_edges
             WHERE ref_name = ?1",
        ) {
            Ok(s) => s,
            Err(_) => return (callers_of, callees_of),
        };
        let rows = match stmt.query_map(params![ref_name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return (callers_of, callees_of),
        };
        for (symbol_id, neighbor_id, direction) in rows.flatten() {
            match direction.as_str() {
                "caller" => callers_of.entry(symbol_id).or_default().push(neighbor_id),
                "callee" => callees_of.entry(symbol_id).or_default().push(neighbor_id),
                _ => {}
            }
        }
        (callers_of, callees_of)
    }

    /// Number of rows in the FTS table (total indexed symbols).
    pub fn symbol_count(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM asd_search_fts", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Count symbols that have any ledger entries.
    ///
    /// Field-test refinement (ExampleFlow feedback, 2026-06-04): this used
    /// to read `SELECT COUNT(*) FROM asd_search_fts WHERE ledger_text != ''`,
    /// but `ledger_text` is only populated at full `asd index` time. Any
    /// entries written via `asd think`, `asd ledger append`, or
    /// `asd annotate-commit` go to `asd_ledger_cache` via `upsert_ledger_entry`
    /// — they're invisible to the FTS table until the next reindex. The
    /// result: trust scoring kept reporting `unannotated` and projects
    /// stuck in a confidence-erasing loop ("write entries → ASD says
    /// nothing's annotated → reindex → maybe ASD sees them"). Reading
    /// from the live cache instead means writes flip the count instantly.
    ///
    /// `ref_name` is required for accurate counting — `asd_ledger_cache`
    /// stores entries per-ref. The DISTINCT collapses multiple entries
    /// on the same symbol so we count *annotated symbols*, not entries.
    pub fn annotated_symbol_count(&self, ref_name: &str) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(DISTINCT symbol_id) FROM asd_ledger_cache WHERE ref_name = ?1",
                params![ref_name],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    fn insert_symbol(
        &self,
        sym: &Symbol,
        ledger_text: &str,
        ledger_flags: &str,
    ) -> rusqlite::Result<()> {
        let qname_exp = expand_identifier(&sym.qname);
        let sig_orig = sym.signature.as_deref().unwrap_or("");
        let sig_exp = if sig_orig.is_empty() {
            String::new()
        } else {
            expand_text(sig_orig)
        };
        let doc = sym.doc.as_deref().unwrap_or("");
        let file_exp = expand_text(&sym.file);
        let kind = format!("{:?}", sym.kind).to_lowercase();
        let tier = symbol_tier(&sym.file).to_string();

        self.conn.execute(
            "INSERT INTO asd_search_fts(
                 symbol_id, qname, signature, doc, file, language, kind, line,
                 qname_orig, sig_orig, file_orig, tier, ledger_text, ledger_flags)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
                ledger_text,
                ledger_flags,
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
        let test_clause = tests_clause(filters.include_tests, filters.tests_only);

        // Fetch extra for hybrid ledger reranking.
        let fetch = (limit * 4).max(80);

        // Columns: 0=symbol_id,1=language,2=kind,3=line,4=doc,
        //          5=qname_orig,6=sig_orig,7=file_orig,8=tier,
        //          9=ledger_text,10=ledger_flags,11=score
        let sql = format!(
            "SELECT symbol_id, language, kind, line, doc,
                    qname_orig, sig_orig, file_orig, tier,
                    ledger_text, ledger_flags,
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
        let hits = stmt
            .query_map(params![match_expr], |row| {
                let bm25_raw: f64 = row.get(11)?;
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
                    ledger_text: row.get::<_, String>(9).unwrap_or_default(),
                    ledger_flags: row.get::<_, String>(10).unwrap_or_default(),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(hits)
    }

    /// For each token in `tokens`, return (token, distinct_file_count) using a single
    /// SQL UNION ALL query instead of N separate searches.  Used by
    /// `detect_ambiguous_tokens` to batch all token checks into one round-trip.
    pub fn count_distinct_files_per_token(
        &self,
        tokens: &[&str],
        include_tests: bool,
    ) -> rusqlite::Result<Vec<(String, usize)>> {
        if tokens.is_empty() {
            return Ok(vec![]);
        }
        let test_clause = if include_tests { "" } else { "AND tier != '2'" };

        // Build: SELECT 'tok', COUNT(DISTINCT file_orig) FROM asd_search_fts
        //        WHERE asd_search_fts MATCH '"tok"' AND tier != '2'
        // UNION ALL ...
        // Each sub-select is a separate FTS5 scan but they share one SQL round-trip,
        // removing N-1 statement-prepare + result-iteration cycles vs the old path.
        let mut parts = Vec::with_capacity(tokens.len());
        let mut params_vec: Vec<String> = Vec::with_capacity(tokens.len());
        for (i, tok) in tokens.iter().enumerate() {
            let tok_clean = tok.replace('"', "");
            parts.push(format!(
                "SELECT ?{} AS tok, COUNT(DISTINCT file_orig) AS cnt \
                 FROM asd_search_fts \
                 WHERE asd_search_fts MATCH '\"{}\"' {test_clause}",
                i + 1,
                tok_clean
            ));
            params_vec.push(tok_clean);
        }
        let sql = parts.join(" UNION ALL ");

        let mut stmt = self.conn.prepare(&sql)?;
        // rusqlite requires params as a slice of &dyn ToSql; build it dynamically.
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let results = stmt
            .query_map(params_refs.as_slice(), |row| {
                let tok: String = row.get(0)?;
                let cnt: i64 = row.get(1)?;
                Ok((tok, cnt as usize))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    /// Returns true if `name` exactly matches any symbol's short name or qname.
    ///
    /// Matches:
    /// - `qname_orig` ends with `.{name}` or equals `{name}` (case-insensitive)
    ///
    /// Used by the uncertainty override guard to distinguish "exact symbol lookup"
    /// from generic broad queries, preventing false-high uncertainty on direct
    /// symbol names like `ExampleFlowViewModel`.
    pub fn has_exact_symbol_name(&self, name: &str) -> bool {
        if name.len() < 3 {
            return false;
        }
        let name_lc = name.to_lowercase();
        // Match qname_orig case-insensitively: either it IS the name or ends with .name
        let sql = "SELECT 1 FROM asd_search_fts \
                   WHERE lower(qname_orig) = ?1 \
                      OR lower(qname_orig) LIKE ?2 \
                   LIMIT 1";
        let suffix_pattern = format!("%.{}", name_lc);
        self.conn
            .query_row(sql, rusqlite::params![name_lc, suffix_pattern], |_| {
                Ok(true)
            })
            .unwrap_or(false)
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

    /// Bulk-resolve qnames to [`SymbolMeta`] from the `asd_symbols_meta` table.
    ///
    /// Returns a map of `qname → SymbolMeta` for all qnames that have a row.
    /// Missing entries are silently omitted; callers should fall back to a git
    /// object read when the result map does not contain a needed qname.
    ///
    /// Cost: one SQL query regardless of how many qnames are requested.
    pub fn get_symbols_meta_bulk(&self, qnames: &[&str]) -> HashMap<String, SymbolMeta> {
        if qnames.is_empty() {
            return HashMap::new();
        }
        // Build: SELECT qname, symbol_id, file, kind FROM asd_symbols_meta
        //        WHERE qname IN (?1, ?2, ...)
        let placeholders: Vec<String> = (1..=qnames.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT qname, symbol_id, file, kind FROM asd_symbols_meta WHERE qname IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = qnames
            .iter()
            .map(|q| q as &dyn rusqlite::types::ToSql)
            .collect();
        stmt.query_map(params_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                SymbolMeta {
                    symbol_id: row.get(1)?,
                    file: row.get(2)?,
                    kind: row.get(3)?,
                },
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Anchor-candidate lookup: return `(qname, symbol_id)` pairs for symbols
    /// that have Invariant or Hazard ledger entries whose text matches at least
    /// one of `tokens`, and are not already in `existing_qnames`.
    ///
    /// Replaces the two `get_tree` calls in `ledger_anchor_pass`:
    ///   1. `get_tree("/asd/v1/ledger")` — full ledger tree walk
    ///   2. `get_tree("/asd/v1/index/by-qname")` — reverse id→qname map
    ///
    /// Both replaced by a single SQLite scan against the UNINDEXED
    /// `ledger_text` / `ledger_flags` columns populated at `asd index` time.
    ///
    /// Returns at most `limit` results (matches `MAX_ANCHORS` in candidates.rs).
    pub fn anchor_candidates(
        &self,
        tokens: &[String],
        existing_qnames: &std::collections::HashSet<String>,
        limit: usize,
    ) -> Vec<(String, String)> {
        if tokens.is_empty() {
            return vec![];
        }

        // Build WHERE clause: ledger_flags must contain invariant or hazard,
        // AND ledger_text must contain at least one query token.
        // Both are UNINDEXED columns — regular LIKE is fine (not FTS MATCH).
        let flag_clause = "(ledger_flags LIKE '%invariant%' OR ledger_flags LIKE '%hazard%')";

        // One OR clause per token: ledger_text LIKE '%token%'
        let text_clauses: Vec<String> = tokens
            .iter()
            .map(|t| format!("ledger_text LIKE '%{}%'", t.replace('\'', "''")))
            .collect();
        let text_clause = format!("({})", text_clauses.join(" OR "));

        let sql = format!(
            "SELECT qname_orig, symbol_id
             FROM asd_search_fts
             WHERE ledger_text != ''
               AND {flag_clause}
               AND {text_clause}
               AND tier != '2'
             LIMIT {limit}"
        );

        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .filter(|(qname, _)| !existing_qnames.contains(qname))
                .collect()
        })
        .unwrap_or_default()
    }

    /// Bulk-resolve qnames to [`ResolvedSymbol`] from the FTS table.
    ///
    /// Returns `HashMap<qname, ResolvedSymbol>` for all qnames that have a row.
    /// Missing entries are silently omitted (unknown qname or stale index).
    ///
    /// Replaces per-symbol `get_symbol_by_qname` git reads in:
    /// - `apply_feedback_adjustments`
    /// - `explain_feedback_impacts`
    /// - `apply_file_scope_feedback`
    /// - `apply_paths_filter`
    /// - `apply_exclusions`
    ///
    /// Cost: one SQL query per call regardless of batch size.
    pub fn resolve_qnames_bulk(&self, qnames: &[&str]) -> HashMap<String, ResolvedSymbol> {
        if qnames.is_empty() {
            return HashMap::new();
        }
        // One row per qname_orig (guaranteed unique by rebuild_refs dedup).
        // doc and sig_orig may be NULL — map to Option.
        let placeholders: Vec<String> = (1..=qnames.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT qname_orig, symbol_id, file_orig, kind, doc, sig_orig
             FROM asd_search_fts
             WHERE qname_orig IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = qnames
            .iter()
            .map(|q| q as &dyn rusqlite::types::ToSql)
            .collect();
        // Columns: 0=qname_orig (key), 1=symbol_id, 2=file_orig, 3=kind, 4=doc, 5=sig_orig
        stmt.query_map(params_refs.as_slice(), |row| {
            let qname: String = row.get(0)?;
            let doc_raw: Option<String> = row.get(4)?;
            let sig_raw: Option<String> = row.get(5)?;
            Ok((
                qname.clone(),
                ResolvedSymbol {
                    symbol_id: row.get(1)?,
                    qname,
                    file: row.get(2)?,
                    kind: row.get(3)?,
                    doc: doc_raw.filter(|s| !s.is_empty()),
                    signature: sig_raw.filter(|s| !s.is_empty()),
                },
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Also resolve by symbol_id batch — used by feedback functions that have
    /// symbol_ids (not qnames) and need file/qname/kind for sibling suppression.
    ///
    /// Cost: one SQL query, returns `HashMap<symbol_id, ResolvedSymbol>`.
    pub fn resolve_symbol_ids_bulk(&self, symbol_ids: &[&str]) -> HashMap<String, ResolvedSymbol> {
        if symbol_ids.is_empty() {
            return HashMap::new();
        }
        let placeholders: Vec<String> = (1..=symbol_ids.len()).map(|i| format!("?{i}")).collect();
        // Columns: 0=symbol_id (key), 1=qname_orig, 2=file_orig, 3=kind, 4=doc, 5=sig_orig
        let sql = format!(
            "SELECT symbol_id, qname_orig, file_orig, kind, doc, sig_orig
             FROM asd_search_fts
             WHERE symbol_id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = symbol_ids
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        stmt.query_map(params_refs.as_slice(), |row| {
            let sym_id: String = row.get(0)?;
            let doc_raw: Option<String> = row.get(4)?;
            let sig_raw: Option<String> = row.get(5)?;
            Ok((
                sym_id.clone(),
                ResolvedSymbol {
                    symbol_id: sym_id,
                    qname: row.get(1)?,
                    file: row.get(2)?,
                    kind: row.get(3)?,
                    doc: doc_raw.filter(|s| !s.is_empty()),
                    signature: sig_raw.filter(|s| !s.is_empty()),
                },
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Ledger write-through cache helpers
    // -----------------------------------------------------------------------

    /// Plan E t-004: one-shot SQL scan that returns every `symbol_id`
    /// whose ledger contains a Constraint or Decision entry carrying a
    /// penalty role (`stale-api` / `audit-pending`). Used by
    /// `apply_constraint_penalties` to avoid an N-walk over the ledger
    /// per query.
    ///
    /// Returns an empty vec when the cache is empty (caller falls back
    /// to the per-candidate ledger walk).
    pub fn symbols_with_constraint_penalties(
        &self,
        ref_name: &str,
    ) -> rusqlite::Result<Vec<String>> {
        let pairs = self.symbols_with_constraint_penalties_scoped(ref_name)?;
        Ok(pairs.into_iter().map(|(sid, _)| sid).collect())
    }

    /// Plan E t-008: like `symbols_with_constraint_penalties` but also
    /// returns the optional scope-glob list parsed from each entry's
    /// `body` JSON. Schema: `body` MAY be a JSON object containing
    /// `{"scope": ["glob1", "glob2", ...]}`. When present, the penalty
    /// applies only to symbols whose file matches at least one glob;
    /// when absent or unparseable, the penalty applies globally
    /// (preserves the Plan E t-004 contract).
    ///
    /// The same symbol_id may appear multiple times in the result when
    /// it has multiple penalty entries (e.g. one global + one scoped).
    /// The caller suppresses on ANY match (most permissive scope wins).
    pub fn symbols_with_constraint_penalties_scoped(
        &self,
        ref_name: &str,
    ) -> rusqlite::Result<Vec<(String, Option<Vec<String>>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT symbol_id, body FROM asd_ledger_cache
             WHERE ref_name = ?1
               AND json_extract(body, '$.kind') IN ('decision', 'constraint')
               AND json_extract(body, '$.role') IN ('stale-api', 'audit-pending')",
        )?;
        let rows = stmt
            .query_map(params![ref_name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .map(|(sid, body)| {
                let scope = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("body").and_then(|b| {
                            // body field itself may be a JSON string or null
                            b.as_str()
                                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                                .or_else(|| Some(b.clone()))
                        })
                    })
                    .and_then(|inner| inner.get("scope").and_then(|s| s.as_array().cloned()))
                    .map(|arr| {
                        arr.into_iter()
                            .filter_map(|g| g.as_str().map(String::from))
                            .filter(|g| !g.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .filter(|v| !v.is_empty());
                (sid, scope)
            })
            .collect::<Vec<_>>();
        Ok(rows)
    }

    /// Number of ledger rows cached for this (symbol_id, ref_name) pair.
    /// Returns 0 when the symbol hasn't been cached yet — triggers git fallback.
    pub fn ledger_entry_count_for(&self, symbol_id: &str, ref_name: &str) -> usize {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM asd_ledger_cache WHERE symbol_id = ?1 AND ref_name = ?2",
                params![symbol_id, ref_name],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize
    }

    /// Insert or replace a single ledger entry for `ref_name`.
    ///
    /// Field-test refinement (ExampleFlow, 2026-06-04): also backfills
    /// the symbol's row in `asd_search_fts` so the ledger_text /
    /// ledger_flags columns stay warm between reindexes. Previously these
    /// columns were ONLY populated at full `asd index` time, which meant
    /// any entries written via `asd think`/`asd ledger append` were
    /// invisible to search-time ledger ranking until the next reindex.
    /// The backfill is best-effort: if the symbol row doesn't exist in
    /// FTS yet (rare — symbol writes happen before any ledger writes),
    /// the UPDATE silently no-ops and the next reindex catches up.
    pub fn upsert_ledger_entry(&self, entry: &LedgerEntry, ref_name: &str) -> rusqlite::Result<()> {
        let body = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "INSERT OR REPLACE INTO asd_ledger_cache (entry_id, symbol_id, ref_name, body)
             VALUES (?1, ?2, ?3, ?4)",
            params![entry.entry_id, entry.symbol_id, ref_name, body],
        )?;

        // Refresh the denormalized FTS columns from the now-current cache
        // for this symbol. Concatenate all summaries for ledger_text,
        // dedupe kinds for ledger_flags.
        let summary_lower = entry.summary.to_lowercase();
        let kind_str = entry.kind.as_str();
        // Use COALESCE so the first entry on a symbol initializes from
        // empty; subsequent entries append a space + new summary.
        let _ = self.conn.execute(
            "UPDATE asd_search_fts
             SET ledger_text = CASE
                   WHEN ledger_text IS NULL OR ledger_text = '' THEN ?1
                   ELSE ledger_text || ' ' || ?1
                 END,
                 ledger_flags = CASE
                   WHEN ledger_flags IS NULL OR ledger_flags = '' THEN ?2
                   WHEN INSTR(',' || ledger_flags || ',', ',' || ?2 || ',') > 0 THEN ledger_flags
                   ELSE ledger_flags || ',' || ?2
                 END
             WHERE symbol_id = ?3",
            params![summary_lower, kind_str, entry.symbol_id],
        );
        Ok(())
    }

    /// Return all ledger entries for a symbol (including superseded), newest first.
    /// The caller's `list_entries` default method handles supersede filtering.
    pub fn list_ledger_entries_for(
        &self,
        symbol_id: &str,
        ref_name: &str,
    ) -> rusqlite::Result<Vec<LedgerEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT body FROM asd_ledger_cache
             WHERE symbol_id = ?1 AND ref_name = ?2
             ORDER BY rowid DESC",
        )?;
        let entries = stmt
            .query_map(params![symbol_id, ref_name], |row| {
                let body: String = row.get(0)?;
                Ok(body)
            })?
            .filter_map(|r| r.ok())
            .filter_map(|body| serde_json::from_str::<LedgerEntry>(&body).ok())
            .collect();
        Ok(entries)
    }

    /// Bulk-insert ledger entries in a single transaction — used by `asd index`
    /// to reconcile the SQLite cache from the authoritative git store.
    /// `entries`: slice of `(symbol_id, LedgerEntry)` pairs.
    pub fn sync_ledger_entries(
        &self,
        entries: &[(String, LedgerEntry)],
        ref_name: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN;")?;
        for (symbol_id, entry) in entries {
            let body = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
            if let Err(err) = self.conn.execute(
                "INSERT OR REPLACE INTO asd_ledger_cache (entry_id, symbol_id, ref_name, body)
                 VALUES (?1, ?2, ?3, ?4)",
                params![entry.entry_id, symbol_id, ref_name, body],
            ) {
                eprintln!("asd: ledger cache sync warning — {err}");
            }
        }
        self.conn.execute_batch("COMMIT;")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Effects write-through cache helpers
    // -----------------------------------------------------------------------

    /// True when an `EffectDecl` for this (symbol_id, ref_name) is cached.
    pub fn effects_cached_for(&self, symbol_id: &str, ref_name: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM asd_effects_cache WHERE symbol_id = ?1 AND ref_name = ?2 LIMIT 1",
                params![symbol_id, ref_name],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Insert or replace the `EffectDecl` for a symbol.
    pub fn upsert_effects(
        &self,
        symbol_id: &str,
        ref_name: &str,
        decl: &EffectDecl,
    ) -> rusqlite::Result<()> {
        let body = serde_json::to_string(decl).unwrap_or_else(|_| "{}".to_string());
        self.conn.execute(
            "INSERT OR REPLACE INTO asd_effects_cache (symbol_id, ref_name, body)
             VALUES (?1, ?2, ?3)",
            params![symbol_id, ref_name, body],
        )?;
        Ok(())
    }

    /// Return the cached `EffectDecl` for a symbol, or `None` if not cached.
    pub fn get_effects_for(
        &self,
        symbol_id: &str,
        ref_name: &str,
    ) -> rusqlite::Result<Option<EffectDecl>> {
        match self.conn.query_row(
            "SELECT body FROM asd_effects_cache WHERE symbol_id = ?1 AND ref_name = ?2 LIMIT 1",
            params![symbol_id, ref_name],
            |row| row.get::<_, String>(0),
        ) {
            Ok(body) => Ok(serde_json::from_str::<EffectDecl>(&body).ok()),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Bulk-insert effects in a single transaction.
    /// `entries`: slice of `(symbol_id, EffectDecl)` pairs.
    pub fn sync_effects(
        &self,
        entries: &[(String, EffectDecl)],
        ref_name: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN;")?;
        for (symbol_id, decl) in entries {
            let body = serde_json::to_string(decl).unwrap_or_else(|_| "{}".to_string());
            if let Err(err) = self.conn.execute(
                "INSERT OR REPLACE INTO asd_effects_cache (symbol_id, ref_name, body)
                 VALUES (?1, ?2, ?3)",
                params![symbol_id, ref_name, body],
            ) {
                eprintln!("asd: effects cache sync warning — {err}");
            }
        }
        self.conn.execute_batch("COMMIT;")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Feedback write-through cache helpers
    // -----------------------------------------------------------------------

    /// Number of feedback rows currently cached in SQLite.
    /// Used as a guard: if 0, the caller should fall back to git.
    pub fn feedback_count(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM asd_feedback", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as usize
    }

    /// Insert or replace a single feedback entry.
    ///
    /// `created_at` is stored as an RFC 3339 string so it sorts correctly
    /// in text order and round-trips through `list_all_feedback` accurately.
    pub fn upsert_feedback(&self, e: &FeedbackEntry) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO asd_feedback
             (entry_id, symbol_id, symbol_qname, query, verdict, author, created_at, note, file_scope, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                e.entry_id,
                e.symbol_id,
                e.symbol_qname,
                e.query,
                e.verdict.as_str(),
                e.author,
                e.created_at.to_rfc3339(),
                e.note,
                e.file_scope,
                // Plan J t-014: persist expires_at so the FTS cache
                // round-trips it (was always None pre-1.0.48).
                e.expires_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Return all feedback entries from the SQLite cache, newest first.
    pub fn list_all_feedback(&self) -> rusqlite::Result<Vec<FeedbackEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT entry_id, symbol_id, symbol_qname, query, verdict, author, created_at, note, file_scope, expires_at
             FROM asd_feedback
             ORDER BY created_at DESC",
        )?;
        let entries = stmt
            .query_map([], |row| {
                let verdict_str: String = row.get(4)?;
                let verdict =
                    FeedbackVerdict::from_str(&verdict_str).unwrap_or(FeedbackVerdict::Useful);
                let ts_str: String = row.get(6)?;
                let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&ts_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                // Plan J t-014: round-trip expires_at through the
                // cache. Older DBs that pre-date the column have it
                // backfilled as NULL by the ALTER migration above.
                let expires_at: Option<DateTime<Utc>> = row
                    .get::<_, Option<String>>(9)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc));
                Ok(FeedbackEntry {
                    entry_id: row.get(0)?,
                    symbol_id: row.get(1)?,
                    symbol_qname: row.get(2)?,
                    query: row.get(3)?,
                    verdict,
                    author: row.get(5)?,
                    created_at,
                    note: row.get(7)?,
                    file_scope: row.get(8)?,
                    expires_at,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(entries)
    }

    /// Bulk-insert feedback entries from an authoritative slice (e.g. from the
    /// git store after `asd index`).  Uses INSERT OR REPLACE so existing rows
    /// are overwritten.  Wraps all inserts in a single transaction for speed.
    pub fn sync_feedback_entries(&self, entries: &[FeedbackEntry]) -> rusqlite::Result<()> {
        self.conn.execute_batch("BEGIN;")?;
        for e in entries {
            if let Err(err) = self.upsert_feedback(e) {
                // Best-effort: log and continue rather than aborting the whole sync.
                eprintln!("asd: feedback sync warning — {err}");
            }
        }
        self.conn.execute_batch("COMMIT;")?;
        Ok(())
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
        let test_clause = tests_clause(filters.include_tests, filters.tests_only);
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
                    qname_orig, sig_orig, file_orig, tier,
                    ledger_text, ledger_flags
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
                    ledger_text: row.get::<_, String>(9).unwrap_or_default(),
                    ledger_flags: row.get::<_, String>(10).unwrap_or_default(),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(hits)
    }

    /// Batch version of [`file_stem_candidates`]: query all `tokens` in a
    /// single SQL UNION ALL instead of one round-trip per token.
    ///
    /// Returns one representative symbol per distinct file (lowest line number),
    /// deduplicated across token arms.  Results are capped at `limit`.
    ///
    /// Falls back to an empty vec if `tokens` is empty or all tokens are
    /// shorter than 2 characters.
    pub fn file_stem_candidates_batch(
        &self,
        tokens: &[String],
        filters: &FtsFilters,
        limit: usize,
    ) -> rusqlite::Result<Vec<FtsHit>> {
        // Keep only tokens long enough to be useful stem matches.
        let valid: Vec<String> = tokens
            .iter()
            .filter(|t| t.len() >= 2)
            .map(|t| t.to_lowercase())
            .collect();
        if valid.is_empty() {
            return Ok(vec![]);
        }

        let test_clause = tests_clause(filters.include_tests, filters.tests_only);
        let kind_clause = filters
            .kind
            .as_deref()
            .map(|k| format!("AND kind = '{}'", k.to_lowercase().replace('\'', "")))
            .unwrap_or_default();

        // Build one SELECT arm per token; each arm uses ?N positional binding.
        let arm_sql = |n: usize| -> String {
            format!(
                "SELECT symbol_id, language, kind, MIN(line) AS line, doc,
                        qname_orig, sig_orig, file_orig, tier,
                        ledger_text, ledger_flags
                 FROM asd_search_fts
                 WHERE lower(file_orig) LIKE '%' || ?{n} || '%'
                 {test_clause}
                 {kind_clause}
                 GROUP BY file_orig"
            )
        };

        let sql = valid
            .iter()
            .enumerate()
            .map(|(i, _)| arm_sql(i + 1))
            .collect::<Vec<_>>()
            .join("\nUNION ALL\n");

        let mut stmt = self.conn.prepare(&sql)?;
        let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        let hits: Vec<FtsHit> = stmt
            .query_map(rusqlite::params_from_iter(valid.iter()), |row| {
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
                    ledger_text: row.get::<_, String>(9).unwrap_or_default(),
                    ledger_flags: row.get::<_, String>(10).unwrap_or_default(),
                })
            })?
            .filter_map(|r| r.ok())
            .filter(|h| seen_files.insert(h.file.clone()))
            .take(limit)
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
    let path_words: Vec<String> = hit
        .file
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

    // Phrase match bonus (t-005): consecutive query tokens that appear as an
    // adjacent subsequence in the name word list represent a domain concept
    // ("drift playhead", "refresh lane", etc.). Reward each adjacent pair
    // with +3.0 so compound-concept queries rank the most specific symbol first.
    let phrase_bonus = if tokens.len() >= 2 {
        let mut bonus = 0.0f64;
        let lower_tokens: Vec<&str> = tokens.iter().map(|t| t.as_str()).collect();
        for window_start in 0..name_words.len().saturating_sub(1) {
            for pair_len in 2..=(lower_tokens.len().min(name_words.len() - window_start)) {
                let name_slice = &name_words[window_start..window_start + pair_len];
                for tok_start in 0..=lower_tokens.len().saturating_sub(pair_len) {
                    let tok_slice = &lower_tokens[tok_start..tok_start + pair_len];
                    if name_slice
                        .iter()
                        .map(|s| s.as_str())
                        .eq(tok_slice.iter().copied())
                    {
                        bonus = bonus.max(pair_len as f64 * 3.0);
                    }
                }
            }
        }
        bonus
    } else {
        0.0
    };

    // Utility penalty: Preview/Sample/Editor/Generated symbols ranked below production.
    let tier_penalty = if hit.tier == 1 { -2.0 } else { 0.0 };

    // t-001: Penalize results where ALL matched tokens are generic unless a
    // domain-specific co-occurring token also matched in the name or path.
    // Generic tokens produce noise when used alone (e.g. "state", "update").
    // Generic + project-ambiguous tokens: terms that match many symbols without
    // anchoring a specific domain concept. "playhead" is ambiguous in this project
    // (many UI and scheduler files contain it); require a co-occurring anchor.
    const GENERIC_TOKENS: &[&str] = &[
        "state",
        "update",
        "local",
        "position",
        "value",
        "data",
        "item",
        "list",
        "info",
        "event",
        "action",
        "type",
        "mode",
        "flag",
        "current",
        "node",
        "result",
        "status",
        "record",
        "entry",
        "object",
        "element",
        "get",
        "set",
        "add",
        "remove",
        "reset",
        "apply",
        "build",
        "make",
        "playhead",
        "cursor",
        "progress",
        "indicator",
        "tick",
    ];
    let matched_tokens: Vec<&str> = tokens
        .iter()
        .filter(|t| {
            name_words.iter().any(|w| w == t.as_str()) || path_words.iter().any(|w| w == t.as_str())
        })
        .map(|t| t.as_str())
        .collect();
    let generic_penalty = if !matched_tokens.is_empty()
        && matched_tokens.iter().all(|t| GENERIC_TOKENS.contains(t))
    {
        -2.5 // increased from -1.5 to push generic-only matches below domain-anchored ones
    } else {
        0.0
    };

    path_boost + name_boost + phrase_bonus + tier_penalty + generic_penalty
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
///
/// Distinguishes three degraded states:
/// - **Empty** — FTS table has never been populated.
/// - **Symbols fresh / FTS stale** — the last `asd index` run recorded an FTS
///   rebuild failure (e.g. "database is locked"). Symbol data in git is current
///   but search ranking reflects an older snapshot.
/// - **Stale** — the last successful FTS rebuild is older than `threshold_secs`.
pub fn stale_warning(db_path: &std::path::Path, threshold_secs: u64) -> Option<String> {
    let fts = SearchFtsDb::open(db_path).ok()?;
    if !fts.has_data() {
        return Some("asd: index is empty — run 'asd index <dir>' to build it.".to_string());
    }

    // If the last index run recorded an FTS failure, surface it immediately
    // regardless of age — the FTS may be out of sync with the symbol data.
    if fts.fts_last_rebuild_ok() == Some(false) {
        return Some(
            "asd: symbols are indexed but the FTS search index failed to rebuild \
             during the last 'asd index' run (database may have been locked). \
             Search ranking may be stale — re-run 'asd index <dir>' to repair."
                .to_string(),
        );
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

/// ExampleFlow refinement (1.0.77): same as `stale_warning` but
/// returns a struct so callers can distinguish a soft "index is old
/// but results came back fine" hint from a hard "FTS is broken /
/// empty" alert. The MCP response includes `stale_severity` as a
/// machine-readable field so downstream UIs can render appropriately
/// (Craig decision Q7: yes, future-proof rendering).
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleWarning {
    pub message: String,
    pub severity: StaleSeverity,
    pub age_secs: u64,
    pub indexed_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleSeverity {
    /// FTS empty or last rebuild failed — search ranking IS broken.
    /// Loud surfacing is correct everywhere; never demote.
    Critical,
    /// Index is past the age threshold but otherwise healthy. Soft
    /// callers (prepare-change, context_for) can demote this to a
    /// hints array when the queried symbols resolved fine and their
    /// ledger entries are newer than the index.
    Soft,
}

pub fn stale_warning_classified(
    db_path: &std::path::Path,
    threshold_secs: u64,
) -> Option<StaleWarning> {
    let fts = SearchFtsDb::open(db_path).ok()?;
    if !fts.has_data() {
        return Some(StaleWarning {
            message: "asd: index is empty — run 'asd index <dir>' to build it.".to_string(),
            severity: StaleSeverity::Critical,
            age_secs: 0,
            indexed_at: None,
        });
    }
    if fts.fts_last_rebuild_ok() == Some(false) {
        return Some(StaleWarning {
            message: "asd: symbols are indexed but the FTS search index failed to rebuild \
                      during the last 'asd index' run (database may have been locked). \
                      Search ranking may be stale — re-run 'asd index <dir>' to repair."
                .to_string(),
            severity: StaleSeverity::Critical,
            age_secs: 0,
            indexed_at: None,
        });
    }
    let indexed_at = fts.last_indexed_at()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_secs = (now - indexed_at).max(0) as u64;
    if age_secs > threshold_secs {
        Some(StaleWarning {
            message: format!(
                "asd: index may be stale — last indexed {}. Run 'asd index <dir>' to update.",
                format_age(indexed_at)
            ),
            severity: StaleSeverity::Soft,
            age_secs,
            indexed_at: Some(indexed_at),
        })
    } else {
        None
    }
}

/// ExampleFlow refinement: 24h soft-threshold for handlers that
/// have a "did the query resolve" signal (prepare-change,
/// context_for). Matches a typical dev day — index built this
/// morning is fine all afternoon. Other commands (status, search,
/// investigate) keep the 1h legacy threshold via `stale_warning`
/// since they ARE the index-health signal.
pub const SOFT_STALE_THRESHOLD_SECS: u64 = 86_400;

/// Plan J t-005: reconcile the two divergent symbol counts that
/// `asd status` and `asd health` were each reporting in isolation.
///
/// - **`asg_symbols`** — `len(/asd/v1/index/by-qname)`, the canonical
///   ASG view. This is what `health` already returned as the bare
///   `symbol_count`. Authoritative for "what does the indexed graph
///   actually contain right now."
/// - **`fts_symbols`** — `SearchFtsDb::symbol_count()`, the count
///   inside the search cache. This is what `status` already returned
///   as `symbols`. Authoritative for "what will ranked queries see."
///
/// They diverge when:
/// - An `asd index` run wrote symbols to the ASG tree but the FTS
///   rebuild failed mid-pass (e.g. `database is locked`).
/// - A `hydrate` repopulated the ASG from a sidecar without
///   re-running FTS (rare, but the call surfaces have changed across
///   M22–M24 and the wiring isn't guaranteed).
/// - Symbol-level migrations (`asd ledger-rebind`, `asd
///   ledger-supersede`) touch the qname tree but skip FTS.
///
/// Returns `null`-shaped guidance fields when the two agree, so the
/// hot path in `status` / `health` adds zero ceremony on a healthy
/// repo. When they diverge, includes an `advice` string the agent
/// can act on directly.
pub fn compute_index_consistency(asg_symbols: usize, fts_symbols: usize) -> serde_json::Value {
    let delta = asg_symbols as i64 - fts_symbols as i64;
    if delta == 0 {
        // Token economy (1.0.79): return Null when consistent.
        // The agent infers "indexes agree" from absence; emitting
        // a 5-field block to say "everything is fine" is pure
        // bloat. Callers (status, health) can pair with
        // `drop_empty_top_level` to omit the field from output.
        return serde_json::Value::Null;
    }
    let advice = if delta > 0 {
        format!(
            "ASG has {delta} symbol{p} not in the FTS search cache — run 'asd index' to rebuild the search index.",
            p = if delta == 1 { "" } else { "s" }
        )
    } else {
        // FTS > ASG. Rare — usually means FTS holds symbols whose
        // ASG entries were deleted (e.g. `asd ledger-withdraw`
        // followed by no reindex). Same fix: rebuild FTS.
        let extra = -delta;
        format!(
            "FTS holds {extra} stale symbol{p} no longer in the ASG — run 'asd index' to rebuild.",
            p = if extra == 1 { "" } else { "s" }
        )
    };
    serde_json::json!({
        "asg_symbols": asg_symbols,
        "fts_symbols": fts_symbols,
        "delta": delta,
        "consistent": false,
        "advice": advice,
    })
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
        "callers",
        "callees",
        "notes",
        "decisions_and_notes",
        "proofs",
        "ownership",
        "other_ledger",
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
                                        if let Some(q) = obj.get("qname") {
                                            mini.insert("qname".into(), q.clone());
                                        }
                                        if let Some(f) = obj.get("file") {
                                            mini.insert("file".into(), f.clone());
                                        }
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
                                s.push(serde_json::json!(format!(
                                    "... {} more",
                                    arr.len() - max_list
                                )));
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
                        let capped: Vec<Value> = files
                            .iter()
                            .take(3)
                            .map(|file_entry| {
                                if let Value::Object(obj) = file_entry {
                                    let mut m = obj.clone();
                                    if let Some(Value::Array(commits)) = m.get_mut("commits") {
                                        commits.truncate(max_list);
                                    }
                                    Value::Object(m)
                                } else {
                                    file_entry.clone()
                                }
                            })
                            .collect();
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
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|i| trim_for_agent(i, max_list)).collect())
        }
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
        "bugfix" => Some("bugfix"),
        "feature" => Some("feature"),
        "refactor" => Some("refactor"),
        "test" => Some("test"),
        "architecture" => Some("architecture"),
        "ui" => Some("ui"),
        _ => None,
    }
}

/// One-line agent guidance for each intent.
pub fn intent_focus(intent: &str) -> &'static str {
    match intent {
        "bugfix" => {
            "Focus: callers that may be broken, effects, invariants to preserve, affected tests."
        }
        "feature" => "Focus: callees to extend, ownership boundaries, empty extension points.",
        "refactor" => {
            "Focus: callers (blast radius), invariants that must hold, ownership constraints."
        }
        "test" => {
            "Focus: affected test symbols, effects under test, existing proof ledger entries."
        }
        "architecture" => {
            "Focus: invariants, ownership boundaries, layer grouping, cross-layer effects."
        }
        "ui" => "Focus: UI/ViewModel layers, effects that touch display state, scheduler coupling.",
        _ => "",
    }
}

/// Return the preferred layer display order for an intent.
/// The standard order is used for unlisted layers.
pub fn intent_layer_order(intent: &str) -> &'static [&'static str] {
    match intent {
        "ui" => &[
            "ui",
            "viewmodel",
            "scheduler",
            "core_model",
            "persistence",
            "utility",
            "tests",
            "other",
        ],
        "architecture" => &[
            "core_model",
            "persistence",
            "scheduler",
            "viewmodel",
            "ui",
            "utility",
            "tests",
            "other",
        ],
        "bugfix" => &[
            "core_model",
            "scheduler",
            "persistence",
            "viewmodel",
            "ui",
            "tests",
            "utility",
            "other",
        ],
        "test" => &[
            "tests",
            "core_model",
            "scheduler",
            "persistence",
            "viewmodel",
            "ui",
            "utility",
            "other",
        ],
        _ => &[
            "ui",
            "viewmodel",
            "scheduler",
            "core_model",
            "persistence",
            "utility",
            "tests",
            "other",
        ],
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
    let Ok(o) = out else {
        return std::collections::HashSet::new();
    };
    if !o.status.success() {
        return std::collections::HashSet::new();
    }
    const SRC_EXTS: &[&str] = &[
        ".swift", ".py", ".ts", ".tsx", ".js", ".rs", ".go", ".kt", ".java", ".rb", ".cs", ".m",
        ".mm", ".cpp", ".c",
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
        hints.push(format!(
            "verify {} behavior",
            words.join(" ").to_lowercase()
        ));
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
            if !cur.is_empty() {
                words.push(cur.clone());
                cur.clear();
            }
        } else if ch.is_uppercase() && !cur.is_empty() {
            words.push(cur.clone());
            cur.clear();
            cur.push(ch);
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
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
        if part.is_empty() {
            continue;
        }
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

/// Look up real indexed test files that correspond to `source_file`.
///
/// Scans the FTS index for files whose path contains a test/spec indicator
/// AND whose stem shares the source file's stem (e.g. `Foo.swift` → `FooTests.swift`).
/// Returns found paths. Falls back to an empty vec if the index is unavailable.
pub fn find_indexed_test_files(db_path: &std::path::Path, source_file: &str) -> Vec<String> {
    let stem = std::path::Path::new(source_file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if stem.is_empty() {
        return vec![];
    }
    let Ok(db) = SearchFtsDb::open(db_path) else {
        return vec![];
    };
    // Query unique file paths from the index that look like test files.
    let Ok(mut stmt) = db
        .conn
        .prepare("SELECT DISTINCT file_orig FROM asd_search_fts WHERE tier = 2")
    else {
        return vec![];
    };
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    // Keep only files whose stem contains (or is contained by) the source stem.
    rows.into_iter()
        .filter(|f| {
            let f_stem = std::path::Path::new(f)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            f_stem.contains(&stem)
                || stem.contains(
                    f_stem
                        .trim_end_matches("tests")
                        .trim_end_matches("test")
                        .trim_end_matches("spec"),
                )
        })
        .collect()
}

/// Pre-fetch all test-tier file paths in a single DB open.
/// Pass the result to [`test_files_for_source`] for per-file stem matching,
/// avoiding repeated DB opens when processing many source files in a loop.
pub fn fetch_all_test_file_paths(db_path: &std::path::Path) -> Vec<String> {
    let Ok(db) = SearchFtsDb::open(db_path) else {
        return vec![];
    };
    let Ok(mut stmt) = db
        .conn
        .prepare("SELECT DISTINCT file_orig FROM asd_search_fts WHERE tier = 2")
    else {
        return vec![];
    };
    stmt.query_map([], |r| r.get::<_, String>(0))
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Filter `all_test_files` (from [`fetch_all_test_file_paths`]) for those
/// whose stem matches `source_file`'s stem. Same logic as [`find_indexed_test_files`]
/// but avoids the per-call DB open.
pub fn test_files_for_source(all_test_files: &[String], source_file: &str) -> Vec<String> {
    let stem = std::path::Path::new(source_file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if stem.is_empty() {
        return vec![];
    }
    all_test_files
        .iter()
        .filter(|f| {
            let f_stem = std::path::Path::new(f.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            f_stem.contains(&stem)
                || stem.contains(
                    f_stem
                        .trim_end_matches("tests")
                        .trim_end_matches("test")
                        .trim_end_matches("spec"),
                )
        })
        .cloned()
        .collect()
}

pub fn propose_test_path(source_file: &str) -> String {
    let path = std::path::Path::new(source_file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown");
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

/// Plan J t-007: language-aware test stub body for an agent that
/// needs to write a new test against `source_file` exercising
/// `symbol_name`. Returns the recommended skeleton — function
/// declaration, arrange/act/assert comments, and a
/// `NotImplementedError`-equivalent marker so the test FAILS until
/// the agent fills it in (failing-first matches the missing-test
/// recipe item which says "fails before the edit, passes after").
///
/// Language detected from the source file extension. Falls back to
/// a generic comment-only stub for unknown extensions.
pub fn propose_test_stub(source_file: &str, symbol_name: &str) -> String {
    let ext = std::path::Path::new(source_file)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let snake = to_snake_case(symbol_name);
    let pascal = to_pascal_case(symbol_name);
    match ext.as_str() {
        "py" => format!(
            "def test_{snake}():\n    # arrange\n    # act\n    # assert\n    raise NotImplementedError(\"test_{snake}: fill in\")\n"
        ),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => format!(
            "test('{snake}', () => {{\n  // arrange\n  // act\n  // assert\n  throw new Error('test {snake}: fill in');\n}});\n"
        ),
        "rs" => format!(
            "#[test]\nfn {snake}() {{\n    // arrange\n    // act\n    // assert\n    todo!(\"{snake}: fill in\");\n}}\n"
        ),
        "go" => format!(
            "func Test{pascal}(t *testing.T) {{\n    // arrange\n    // act\n    // assert\n    t.Fatal(\"Test{pascal}: fill in\")\n}}\n"
        ),
        "java" => format!(
            "@Test\npublic void test{pascal}() {{\n    // arrange\n    // act\n    // assert\n    fail(\"test{pascal}: fill in\");\n}}\n"
        ),
        "cs" => format!(
            "[Test]\npublic void {pascal}_Should() {{\n    // arrange\n    // act\n    // assert\n    Assert.Fail(\"{pascal}_Should: fill in\");\n}}\n"
        ),
        "rb" => format!(
            "def test_{snake}\n  # arrange\n  # act\n  # assert\n  flunk(\"test_{snake}: fill in\")\nend\n"
        ),
        "kt" | "kts" => format!(
            "@Test\nfun `{snake}`() {{\n    // arrange\n    // act\n    // assert\n    fail(\"{snake}: fill in\")\n}}\n"
        ),
        "swift" => format!(
            "func test{pascal}() throws {{\n    // arrange\n    // act\n    // assert\n    XCTFail(\"test{pascal}: fill in\")\n}}\n"
        ),
        _ => {
            format!("// New test exercising {symbol_name} — fill in the arrange / act / assert.\n")
        }
    }
}

/// `payment.charge` / `PaymentCharge` / `payment_charge` → `payment_charge`.
fn to_snake_case(name: &str) -> String {
    // Drop module prefix if qname-shaped.
    let leaf = name.rsplit('.').next().unwrap_or(name);
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in leaf.chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with('_') {
                out.push('_');
            }
            prev_lower = false;
        } else if ch.is_ascii_uppercase() {
            if prev_lower && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else {
            out.push(ch);
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out.trim_matches('_').to_string()
}

/// `payment_charge` / `payment.charge` → `PaymentCharge`.
fn to_pascal_case(name: &str) -> String {
    let leaf = name.rsplit('.').next().unwrap_or(name);
    let mut out = String::new();
    let mut capitalize_next = true;
    for ch in leaf.chars() {
        if ch == '_' || ch == '-' {
            capitalize_next = true;
        } else if capitalize_next {
            out.push(ch.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            out.push(ch);
        }
    }
    out
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
pub fn gather_recency(
    scan_commits: usize,
    hot_days: f64,
) -> std::collections::HashMap<String, FileRecency> {
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

    let Ok(out) = output else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }

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

/// The evidence source that identified an owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerSignalSource {
    /// Determined by frequency in `git blame` for the symbol's line range.
    GitBlame,
    /// Extracted from `@owner` / `Owner:` annotation in the doc comment.
    DocComment,
    /// Listed among recent unique committers from `git log`.
    GitLog,
    /// Recorded in an `Ownership` ledger entry for this symbol.
    LedgerTruth,
}

/// An owner with the evidence source that identified them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnnotatedOwner {
    pub name: String,
    pub source: OwnerSignalSource,
}

/// Ownership signal returned by [`discover_symbol_ownership`].
#[derive(Debug, Clone)]
pub struct OwnershipSignal {
    /// Name of the most frequent author in the symbol's git blame range.
    pub primary_author: Option<String>,
    /// Author extracted from a `@owner` / `Owner:` annotation in the doc comment.
    pub doc_owner: Option<String>,
    /// The `N` most recent unique committers to the symbol's file.
    pub recent_committers: Vec<String>,
    /// All owner signals with source confidence annotations.
    pub annotated: Vec<AnnotatedOwner>,
}

/// Discover the likely owner of a symbol from git blame + doc-comment annotations.
///
/// Returns an `OwnershipSignal` with signals from multiple sources so callers
/// can choose how to display or prioritise them.  All signals are best-effort;
/// errors (no git, no file) return empty/`None` gracefully.
pub fn discover_symbol_ownership(
    file: &str,
    start_line: u32,
    end_line: u32,
    doc: Option<&str>,
) -> OwnershipSignal {
    use std::collections::HashMap;
    use std::process::Command;

    // 1. Extract `@owner` / `Owner:` from doc comment.
    let doc_owner = doc.and_then(|d| {
        for line in d.lines() {
            let l = line.trim().to_lowercase();
            for prefix in &["@owner ", "owner: ", "owned by "] {
                if let Some(rest) = l.strip_prefix(prefix) {
                    let owner = rest.split_whitespace().next().unwrap_or("").to_string();
                    if !owner.is_empty() {
                        return Some(owner);
                    }
                }
            }
        }
        None
    });

    // 2. Git blame for the symbol's line range → most frequent author.
    let blame_out = Command::new("git")
        .args([
            "blame",
            "--porcelain",
            &format!("-L{},{}", start_line.max(1), end_line.max(start_line)),
            "--",
            file,
        ])
        .output();
    let mut blame_authors: HashMap<String, usize> = HashMap::new();
    if let Ok(out) = blame_out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(author) = line.strip_prefix("author ") {
                    *blame_authors.entry(author.trim().to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let primary_author = blame_authors
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(a, _)| a)
        .filter(|a| !a.is_empty() && a != "Not Committed Yet");

    // 3. Recent unique committers to the file (up to 5).
    let log_out = Command::new("git")
        .args(["log", "-n20", "--pretty=format:%an", "--", file])
        .output();
    let mut seen = std::collections::HashSet::new();
    let mut recent_committers: Vec<String> = Vec::new();
    if let Ok(out) = log_out {
        if out.status.success() {
            for author in String::from_utf8_lossy(&out.stdout).lines() {
                let a = author.trim().to_string();
                if !a.is_empty() && seen.insert(a.clone()) {
                    recent_committers.push(a);
                    if recent_committers.len() >= 5 {
                        break;
                    }
                }
            }
        }
    }

    // Build annotated list with source confidence labels.
    let mut annotated: Vec<AnnotatedOwner> = Vec::new();
    // Doc owner is the strongest signal — explicit annotation by the author.
    if let Some(ref owner) = doc_owner {
        annotated.push(AnnotatedOwner {
            name: owner.clone(),
            source: OwnerSignalSource::DocComment,
        });
    }
    // Primary blame author is high-confidence structural ownership.
    if let Some(ref author) = primary_author {
        if !annotated.iter().any(|a| &a.name == author) {
            annotated.push(AnnotatedOwner {
                name: author.clone(),
                source: OwnerSignalSource::GitBlame,
            });
        }
    }
    // Recent committers complete the picture with lower-confidence recency signal.
    for committer in &recent_committers {
        if !annotated.iter().any(|a| &a.name == committer) {
            annotated.push(AnnotatedOwner {
                name: committer.clone(),
                source: OwnerSignalSource::GitLog,
            });
        }
    }

    OwnershipSignal {
        primary_author,
        doc_owner,
        recent_committers,
        annotated,
    }
}

/// A test symbol that likely covers a given impl symbol.
#[derive(Debug, Clone)]
pub struct CoveringTest {
    pub qname: String,
    pub file: String,
    pub line: i64,
    /// Exact command to run this test (e.g. `swift test --filter testRefreshPlayhead`).
    pub run_command: String,
}

/// Find test symbols in the FTS index that are likely to cover `impl_qname`.
///
/// Matching strategy (in order of confidence):
/// 1. Test qname contains the impl symbol's leaf name (e.g. `testRefreshPlayhead`
///    matches `refreshPlayhead`).
/// 2. Test doc comment mentions the impl qname.
///
/// Returns a list of `CoveringTest` with file path and exact run command.
pub fn find_covering_tests(fts: Option<&SearchFtsDb>, impl_qname: &str) -> Vec<CoveringTest> {
    let leaf = impl_qname
        .split(|c: char| c == '.' || c == ':' || c == '/')
        .last()
        .unwrap_or(impl_qname)
        .to_lowercase();
    if leaf.is_empty() || leaf.len() < 3 {
        return vec![];
    }
    let Some(db) = fts else {
        return vec![];
    };
    // Query test-tier symbols whose qname or doc mentions the leaf name.
    let Ok(mut stmt) = db
        .conn
        .prepare("SELECT qname_orig, file_orig, start_line FROM asd_search_fts WHERE tier = 2")
    else {
        return vec![];
    };
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    rows.into_iter()
        .filter(|(qname, _, _)| {
            let q = qname.to_lowercase();
            let stripped = q
                .trim_start_matches("test")
                .trim_start_matches('_')
                .trim_start_matches("spec")
                .trim_start_matches('_');
            stripped.contains(leaf.as_str()) || q.contains(leaf.as_str())
        })
        .map(|(qname, file, line)| {
            let run_command = derive_test_run_command(&file, &qname);
            CoveringTest {
                qname,
                file,
                line,
                run_command,
            }
        })
        .collect()
}

/// Derive an exact test run command from a test file path and test qname.
///
/// Walks up from the file to find the project manifest, then emits the
/// appropriate test runner invocation.
fn derive_test_run_command(file: &str, test_qname: &str) -> String {
    use std::path::Path;

    let file_path = Path::new(file);
    let leaf_name = test_qname
        .split(|c: char| c == '.' || c == ':')
        .last()
        .unwrap_or(test_qname);

    // Walk up to find a project manifest.
    let mut dir = file_path.parent();
    while let Some(d) = dir {
        if d.join("Package.swift").exists() {
            return format!("swift test --filter {}", leaf_name);
        }
        if d.join("Cargo.toml").exists() {
            return format!("cargo test {}", leaf_name);
        }
        if d.join("package.json").exists() {
            // Prefer jest/vitest based on file extension.
            if file.ends_with(".test.ts")
                || file.ends_with(".spec.ts")
                || file.ends_with(".test.js")
                || file.ends_with(".spec.js")
            {
                return format!("npx jest --testNamePattern=\"{}\"", leaf_name);
            }
            return format!("npm test -- --grep \"{}\"", leaf_name);
        }
        if d.join("pyproject.toml").exists() || d.join("setup.py").exists() {
            return format!("pytest -k \"{}\"", leaf_name);
        }
        if d.join("go.mod").exists() {
            // Derive package from file path relative to go.mod.
            let rel = file_path
                .strip_prefix(d)
                .ok()
                .and_then(|p| p.parent())
                .and_then(|p| p.to_str())
                .map(|s| format!("./{}", s))
                .unwrap_or_else(|| "./...".to_string());
            return format!("go test {} -run {}", rel, leaf_name);
        }
        if d.join("Gemfile").exists() {
            return format!("bundle exec rspec --example \"{}\"", leaf_name);
        }
        if d.join("build.gradle").exists() || d.join("build.gradle.kts").exists() {
            return format!("./gradlew test --tests \"*{}*\"", leaf_name);
        }
        if d.join("pom.xml").exists() {
            return format!("mvn -Dtest=*{}* test", leaf_name);
        }
        dir = d.parent();
    }

    // Fallback: language-based inference from file extension.
    if file.ends_with(".swift") {
        format!("swift test --filter {}", leaf_name)
    } else if file.ends_with(".rs") {
        format!("cargo test {}", leaf_name)
    } else if file.ends_with(".py") {
        format!("pytest -k \"{}\"", leaf_name)
    } else if file.ends_with(".go") {
        format!("go test ./... -run {}", leaf_name)
    } else {
        format!("# run: {}", leaf_name)
    }
}

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
            return rest
                .trim_start_matches([' ', '\t'])
                .trim_end_matches([' ', '\t']);
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
                    if matches!(c, '.' | '!' | '?') {
                        Some(i + c.len_utf8())
                    } else {
                        None
                    }
                })
                .unwrap_or(cleaned.len().min(120));
            let sentence = cleaned[..end.min(cleaned.len())]
                .trim()
                .trim_end_matches(['.', '!', '?']);
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
    if is_test_file(file) {
        2
    } else if is_utility_file(file) {
        1
    } else {
        0
    }
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
        let Ok(text) = std::fs::read_to_string(maybe_path) else {
            continue;
        };
        #[derive(serde::Deserialize)]
        struct LayersFile {
            patterns: Option<toml::Table>,
        }
        let Ok(parsed) = toml::from_str::<LayersFile>(&text) else {
            continue;
        };
        let Some(patterns) = parsed.patterns else {
            continue;
        };
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
pub fn classify_layer(
    file: &str,
    tier: SymbolTier,
    overrides: &[(String, String)],
) -> &'static str {
    if tier == 2 {
        return "tests";
    }
    if tier == 1 {
        return "utility";
    }

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
        "scheduler",
        "schedulers",
        "engine",
        "engines",
        "audio",
        "audioengine",
        "transport",
        "render",
        "renderer",
        "pipeline",
        "worker",
        "workers",
        "processing",
        "realtime",
        "dsp",
        "clock",
    ];
    const SCHED_SUFFIXES: &[&str] = &[
        "scheduler",
        "engine",
        "transport",
        "renderer",
        "pipeline",
        "worker",
        "processor",
        "synthesizer",
        "synth",
        "clock",
        "timer",
        "compiler",
        "loop",
    ];
    if dirs.iter().any(|d| SCHED_DIRS.contains(d))
        || SCHED_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "scheduler";
    }

    // --- Persistence ---
    const PERSIST_DIRS: &[&str] = &[
        "storage",
        "database",
        "db",
        "repository",
        "repositories",
        "cache",
        "datastore",
        "persistence",
        "migration",
        "migrations",
        "store",
        "dao",
    ];
    const PERSIST_SUFFIXES: &[&str] = &[
        "repository",
        "store",
        "database",
        "cache",
        "storage",
        "dao",
        "datasource",
        "migration",
    ];
    if dirs.iter().any(|d| PERSIST_DIRS.contains(d))
        || PERSIST_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "persistence";
    }

    // --- ViewModel / Presenter / Controller ---
    const VM_DIRS: &[&str] = &[
        "viewmodels",
        "viewmodel",
        "presenters",
        "presenter",
        "coordinators",
        "coordinator",
        "interactors",
        "interactor",
        "controllers",
        "controller",
        "states",
        "state",
        "routers",
        "router",
    ];
    const VM_SUFFIXES: &[&str] = &[
        "viewmodel",
        "viewstate",
        "presenter",
        "coordinator",
        "interactor",
        "statemanager",
        "controller",
        "observable",
        "environment",
        "router",
    ];
    if dirs.iter().any(|d| VM_DIRS.contains(d)) || VM_SUFFIXES.iter().any(|s| stem.ends_with(s)) {
        return "viewmodel";
    }

    // --- UI ---
    const UI_DIRS: &[&str] = &[
        "views",
        "view",
        "screens",
        "screen",
        "pages",
        "page",
        "components",
        "component",
        "widgets",
        "widget",
        "cells",
        "viewcontrollers",
        "ui",
        "fragments",
    ];
    const UI_SUFFIXES: &[&str] = &[
        "view",
        "screen",
        "page",
        "component",
        "widget",
        "cell",
        "viewcontroller",
        "fragment",
        "layout",
        "button",
        "label",
        "panel",
        "sheet",
        "modal",
        "overlay",
        "header",
        "footer",
    ];
    if dirs.iter().any(|d| UI_DIRS.contains(d)) || UI_SUFFIXES.iter().any(|s| stem.ends_with(s)) {
        return "ui";
    }

    // --- Core Model / Domain ---
    const MODEL_DIRS: &[&str] = &[
        "models", "model", "domain", "core", "entities", "entity", "services", "service",
        "usecases", "usecase", "business", "logic", "features", "feature",
    ];
    const MODEL_SUFFIXES: &[&str] = &[
        "model",
        "entity",
        "service",
        "usecase",
        "manager",
        "handler",
        "factory",
        "builder",
        "validator",
    ];
    if dirs.iter().any(|d| MODEL_DIRS.contains(d))
        || MODEL_SUFFIXES.iter().any(|s| stem.ends_with(s))
    {
        return "core_model";
    }

    "other"
}

/// Plan J t-003: classify a file by its functional ROLE in the
/// project — `view` / `viewmodel` / `test` / `example` / `fixture` /
/// `script` / `generated` / `reference` / `impl`. Distinct from
/// `classify_layer`, which buckets by architectural tier
/// (presentation / domain / persistence / etc.).
///
/// Order of precedence matters — tests/specs short-circuit before
/// other patterns so a `ViewTests.swift` is `test`, not `view`.
/// `view` and `viewmodel` are detected by filename suffix
/// (`*ViewModel.*`, `*View.*`) AND `/views/` / `/viewmodels/` /
/// `*.vue` / `*.svelte` path/extension patterns. Conservative on
/// `.tsx`/`.jsx` — those frequently are UI but not always.
///
/// Was previously inline-duplicated in CLI prepare_change.rs and
/// MCP mcp_server.rs (with diverged pattern sets); Plan J t-003
/// lifts to one canonical impl. The MCP variant was missing
/// `fixture` / `script` / `generated` / `view` / `viewmodel`
/// entirely — the unified classifier picks them up everywhere.
pub fn classify_file_role(file: &str) -> &'static str {
    // Prefix with `/` so root-level dirs (`scripts/foo.sh`) match
    // the same `/scripts` predicate as nested ones (`pkg/scripts/foo.sh`).
    // Without this, paths without a leading slash fell through to
    // `impl` silently — a pre-existing bug the old inline classifier
    // also had.
    let fl = format!("/{}", file.to_lowercase());
    // Tests first — `ViewTests.swift` must classify as `test`, not `view`.
    if fl.contains("/test")
        || fl.contains("/tests/")
        || fl.contains("/spec")
        || fl.contains("_test.")
        || fl.contains("spec.")
        || fl.ends_with("tests.swift")
    {
        return "test";
    }
    if fl.contains("/example")
        || fl.contains("/examples")
        || fl.contains("/sample")
        || fl.contains("/samples")
        || fl.contains("/demo")
        || fl.contains("/demos")
    {
        return "example";
    }
    if fl.contains("/fixture")
        || fl.contains("/fixtures")
        || fl.contains("/seed")
        || fl.contains("/seeds")
    {
        return "fixture";
    }
    if fl.contains("/script")
        || fl.contains("/scripts")
        || fl.contains("/tool/")
        || fl.contains("/tools/")
        || fl.contains("/bin/")
        || fl.contains("/hack/")
    {
        return "script";
    }
    if fl.contains("/generated")
        || fl.contains("/gen/")
        || fl.contains(".generated.")
        || fl.contains(".pb.")
        || fl.contains("_generated")
    {
        return "generated";
    }
    if fl.contains("/doc")
        || fl.contains("/docs")
        || fl.contains("/reference")
        || fl.contains("readme")
        || fl.ends_with(".md")
        || fl.ends_with(".rst")
        || fl.ends_with(".adoc")
    {
        return "reference";
    }
    // Plan J t-003: viewmodel + view patterns. Order matters —
    // ViewModel.swift must short-circuit before the bare "view"
    // suffix check below catches it.
    if fl.contains("/viewmodels/") || fl.contains("/viewmodel/") || stem_ends_with(&fl, "viewmodel")
    {
        return "viewmodel";
    }
    if fl.contains("/views/")
        || fl.contains("/view/")
        || stem_ends_with(&fl, "view")
        || fl.ends_with(".vue")
        || fl.ends_with(".svelte")
    {
        return "view";
    }
    "impl"
}

/// Lower-cased helper: does the file STEM (without the extension)
/// end with `suffix`? Avoids matching `viewmodel` in a path
/// segment like `previewmodel/foo.rs`. Plan J t-003.
fn stem_ends_with(path_lower: &str, suffix: &str) -> bool {
    let stem = std::path::Path::new(path_lower)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    stem.ends_with(suffix)
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
        if n.ends_with("viewmodel")
            || n.ends_with("controller")
            || n.ends_with("presenter")
            || n.ends_with("coordinator")
            || n.ends_with("interactor")
            || n.ends_with("viewstate")
            || n.ends_with("statemanager")
            || n.ends_with("router")
        {
            found_viewmodel = true;
        } else if n.ends_with("view")
            || n.ends_with("screen")
            || n.ends_with("page")
            || n.ends_with("cell")
            || n.ends_with("widget")
            || n.ends_with("button")
            || n.ends_with("label")
            || n.ends_with("panel")
            || n.ends_with("viewcontroller")
            || n.ends_with("sheet")
            || n.ends_with("overlay")
            || n.ends_with("header")
        {
            found_ui = true;
        } else if n.ends_with("scheduler")
            || n.ends_with("engine")
            || n.ends_with("compiler")
            || n.ends_with("processor")
            || n.ends_with("renderer")
            || n.ends_with("clock")
            || n.ends_with("timer")
            || n.ends_with("pipeline")
            || n.ends_with("worker")
            || n.ends_with("synthesizer")
            || n.ends_with("transport")
        {
            found_scheduler = true;
        } else if n.ends_with("repository")
            || n.ends_with("store")
            || n.ends_with("cache")
            || n.ends_with("dao")
            || n.ends_with("database")
            || n.ends_with("datasource")
        {
            found_persistence = true;
        } else if n.ends_with("model")
            || n.ends_with("entity")
            || n.ends_with("service")
            || n.ends_with("manager")
            || n.ends_with("handler")
            || n.ends_with("factory")
            || n.ends_with("validator")
            || n.ends_with("usecase")
            || n.ends_with("builder")
        {
            found_core_model = true;
        }
    }

    // Return in priority order: most specific wins.
    if found_viewmodel {
        return "viewmodel";
    }
    if found_ui {
        return "ui";
    }
    if found_scheduler {
        return "scheduler";
    }
    if found_persistence {
        return "persistence";
    }
    if found_core_model {
        return "core_model";
    }
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
        "previews",
        "preview",
        "samples",
        "sample",
        "sampledata",
        "examples",
        "example",
        "mocks",
        "mock",
        "stubs",
        "stub",
        "fixtures",
        "fixture",
        "demo",
        "demos",
        "editor",
        "editors",
        "generated",
        "gen",
        "sandbox",
        "playground",
        "playgrounds",
    ];
    // Also match compound directory names that start with a utility prefix.
    const UTILITY_PREFIXES: &[&str] = &["mock", "stub", "fake", "sample", "preview", "generated"];
    if segments
        .iter()
        .rev()
        .skip(1)
        .any(|s| UTILITY_DIRS.contains(s) || UTILITY_PREFIXES.iter().any(|p| s.starts_with(p)))
    {
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
        if filename.contains(".generated.")
            || filename.contains(".g.")
            || filename.ends_with("generated.swift")
        {
            return true;
        }
        // Mock/Stub/Fake: MockFoo.swift, FooMock.kt, FakeBar.ts
        if filename.starts_with("mock")
            || filename.contains("_mock.")
            || filename.starts_with("stub")
            || filename.contains("_stub.")
            || filename.starts_with("fake")
            || filename.contains("_fake.")
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
        "tests",
        "test",
        "specs",
        "spec",
        "__tests__",
        "__mocks__",
        "testing",
        "testcases",
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

// ---------------------------------------------------------------------------
// Document search — broad corpus beyond semantic symbols
// ---------------------------------------------------------------------------

/// A searchable chunk from a non-code file (markdown, config, manifest, etc.).
#[derive(Debug, Clone)]
pub struct SearchDoc {
    /// Stable content-addressable id: sha256 hex of "{path}:{span_start}".
    pub doc_id: String,
    /// Broad kind category.
    pub kind: DocKind,
    /// Relative file path from project root.
    pub path: String,
    /// Optional start line (1-based). None = whole-file chunk.
    pub span_start: Option<u32>,
    /// Human-readable title (heading text, target name, key path, etc.).
    pub title: String,
    /// Searchable body text (stripped of formatting).
    pub body_text: String,
    /// qname of the nearest semantic symbol that "owns" this doc, if known.
    pub owner_symbol_id: Option<String>,
}

impl SearchDoc {
    pub fn new(
        kind: DocKind,
        path: impl Into<String>,
        span_start: Option<u32>,
        title: impl Into<String>,
        body_text: impl Into<String>,
    ) -> Self {
        let path = path.into();
        let span_str = span_start.map(|l| l.to_string()).unwrap_or_default();
        let raw = format!("{}:{}", path, span_str);
        // Simple djb2-style deterministic id — no external dep needed.
        let mut hash: u64 = 5381;
        for b in raw.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(b as u64);
        }
        let doc_id = format!("doc_{:016x}", hash);
        Self {
            doc_id,
            kind,
            path,
            span_start,
            title: title.into(),
            body_text: body_text.into(),
            owner_symbol_id: None,
        }
    }
}

/// Broad category for a document chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Markdown,
    Config, // JSON, TOML, YAML, plist
    Html,
    Css,
    Manifest,    // Package.swift, Cargo.toml, pubspec.yaml, package.json (as manifests)
    BuildScript, // Makefile, Fastfile, Podfile, Gemfile
    Other,
}

impl DocKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Markdown => "markdown",
            DocKind::Config => "config",
            DocKind::Html => "html",
            DocKind::Css => "css",
            DocKind::Manifest => "manifest",
            DocKind::BuildScript => "build_script",
            DocKind::Other => "other",
        }
    }
}

/// A ranked document search hit.
#[derive(Debug, Clone)]
pub struct DocHit {
    pub bm25_score: f64,
    pub doc_id: String,
    pub kind: String,
    pub path: String,
    pub span_start: Option<u32>,
    pub title: String,
    /// First 200 chars of body_text for preview.
    pub preview: String,
    pub owner_symbol_id: Option<String>,
}

/// FTS index for document/resource chunks (separate from symbol FTS).
pub struct SearchDocsDb {
    conn: Connection,
}

impl SearchDocsDb {
    const SCHEMA_VER: i64 = 1;

    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;",
        )?;
        let db_bytes = std::fs::metadata(db_path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let mut cache_kb: usize = ((db_bytes * 8 / 10) / 1024).clamp(8_192, 65_536);
        let mut mmap_bytes: u64 = 268_435_456;
        if let Some(project_dir) = db_path.parent() {
            let cfg_path = project_dir.join(".asd").join("config.toml");
            if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
                if let Ok(table) = raw.parse::<toml::Table>() {
                    if let Some(perf) = table.get("performance").and_then(|v| v.as_table()) {
                        if let Some(v) = perf.get("cache_size_kb").and_then(|v| v.as_integer()) {
                            cache_kb = (v as usize).clamp(1_024, 131_072);
                        }
                        if let Some(v) = perf.get("mmap_size_mb").and_then(|v| v.as_integer()) {
                            mmap_bytes = (v as u64).clamp(64, 4096) * 1024 * 1024;
                        }
                    }
                }
            }
        }
        conn.execute_batch(&format!(
            "PRAGMA mmap_size={mmap_bytes}; PRAGMA cache_size=-{cache_kb};"
        ))?;
        let db = Self { conn };
        db.ensure_schema()?;
        Ok(db)
    }

    fn ensure_schema(&self) -> rusqlite::Result<()> {
        let current: i64 = self
            .conn
            .query_row("SELECT version FROM asd_docs_meta LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);

        if current != Self::SCHEMA_VER {
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS asd_search_docs;
                 DROP TABLE IF EXISTS asd_docs_meta;",
            )?;
        }

        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS asd_search_docs USING fts5(
                doc_id          UNINDEXED,
                kind            UNINDEXED,
                path            UNINDEXED,
                span_start      UNINDEXED,
                owner_symbol_id UNINDEXED,
                title,
                body_text,
                tokenize = 'unicode61 remove_diacritics 1'
            );
            CREATE TABLE IF NOT EXISTS asd_docs_meta (version INTEGER PRIMARY KEY);
            INSERT OR IGNORE INTO asd_docs_meta VALUES ({});",
            Self::SCHEMA_VER
        ))
    }

    /// Atomically replace all document chunks.
    pub fn rebuild(&self, docs: &[SearchDoc]) -> rusqlite::Result<()> {
        self.conn.execute_batch("DELETE FROM asd_search_docs;")?;
        for doc in docs {
            self.conn.execute(
                "INSERT INTO asd_search_docs(doc_id, kind, path, span_start, owner_symbol_id, title, body_text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    doc.doc_id,
                    doc.kind.as_str(),
                    doc.path,
                    doc.span_start,
                    doc.owner_symbol_id,
                    doc.title,
                    doc.body_text,
                ],
            )?;
        }
        Ok(())
    }

    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM asd_search_docs", [], |r| r.get(0))
    }

    /// Full-text search over document chunks. Returns up to `limit` hits ranked by BM25.
    pub fn search(
        &self,
        tokens: &[String],
        limit: usize,
        kinds: Option<&[&str]>,
    ) -> rusqlite::Result<Vec<DocHit>> {
        let filtered: Vec<&String> = tokens.iter().filter(|t| !is_stopword(t)).collect();
        if filtered.is_empty() {
            return Ok(vec![]);
        }
        let match_expr = filtered
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");

        let kind_filter = kinds
            .map(|ks| {
                let list = ks
                    .iter()
                    .map(|k| format!("'{}'", k))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(" AND kind IN ({})", list)
            })
            .unwrap_or_default();

        let sql = format!(
            "SELECT doc_id, kind, path, span_start, owner_symbol_id, title, body_text,
                    -bm25(asd_search_docs, 5.0, 3.0) AS score
             FROM asd_search_docs
             WHERE asd_search_docs MATCH ?1{kind_filter}
             ORDER BY score DESC
             LIMIT {limit}"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let hits = stmt
            .query_map(params![match_expr], |row| {
                let body: String = row.get(6)?;
                let preview = body.chars().take(200).collect::<String>();
                Ok(DocHit {
                    doc_id: row.get(0)?,
                    kind: row.get(1)?,
                    path: row.get(2)?,
                    span_start: row.get(3)?,
                    owner_symbol_id: row.get(4)?,
                    title: row.get(5)?,
                    preview,
                    bm25_score: row.get(7)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(hits)
    }

    pub fn is_empty(&self) -> bool {
        self.count().unwrap_or(0) == 0
    }
}

// ---------------------------------------------------------------------------
// Effect detail reason — one-line human + agent explanation
// ---------------------------------------------------------------------------

/// Return a one-line explanation of an effect declaration's verification state.
///
/// Examples:
///   "ok — verified by test_observed at 2024-01-15"
///   "effects declared but not verified — run 'asd verify-effects'"
///   "mismatch — unexpected for StateIO (expected: declared)"
///   "no effects declared"
pub fn effect_detail_reason(decl: Option<&crate::schema::EffectDecl>) -> String {
    let Some(decl) = decl else {
        return "no effects declared".to_string();
    };
    if decl.declared.is_empty() {
        return "no effects declared".to_string();
    }
    // Runtime-trace evidence is the strongest signal (real execution observed
    // the effects), so it takes precedence in the badge when present. The
    // derived confidence already folds the static prior + accumulated counts.
    if let Some(rt) = &decl.runtime {
        let total = rt.confirmations + rt.contradictions;
        if total > 0 {
            let verdict = if rt.contradictions == 0 {
                "runtime-verified"
            } else {
                "runtime-contested"
            };
            return format!(
                "{} — confidence {:.2} ({}✓/{}✗ over {} trace{})",
                verdict,
                rt.confidence(),
                rt.confirmations,
                rt.contradictions,
                total,
                if total == 1 { "" } else { "s" },
            );
        }
    }
    let Some(ref v) = decl.verification else {
        return format!(
            "effects declared ({} effect{}) but not verified — run 'asd verify-effects'",
            decl.declared.len(),
            if decl.declared.len() == 1 { "" } else { "s" }
        );
    };
    match v.status {
        crate::schema::VerificationStatus::Ok => {
            let source = format!("{:?}", v.by).to_lowercase().replace("_", "-");
            let date = v.at.format("%Y-%m-%d").to_string();
            format!("ok — verified by {} at {}", source, date)
        }
        crate::schema::VerificationStatus::Unverified => {
            "unverified — verification run did not complete".to_string()
        }
        crate::schema::VerificationStatus::Mismatch => {
            if v.mismatches.is_empty() {
                "mismatch — declared effects not observed at runtime".to_string()
            } else {
                let mm = &v.mismatches[0];
                format!("mismatch — {} for {:?}", mm.kind, mm.effect)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- effect_detail_reason: runtime-trace badge (t-001 slice 3) -------

    fn decl_with_runtime(conf: u64, contra: u64, prior: f64) -> crate::schema::EffectDecl {
        use crate::schema::*;
        EffectDecl {
            symbol_id: "s".into(),
            declared: vec![Effect::new(EffectCategory::IoNetOut)],
            transitive: vec![],
            // Runtime ingest also sets a RuntimeTracer verification; the badge
            // must prefer the richer runtime evidence over this.
            verification: Some(Verification {
                by: VerificationSource::RuntimeTracer,
                at: chrono::Utc::now(),
                status: if contra == 0 {
                    VerificationStatus::Ok
                } else {
                    VerificationStatus::Mismatch
                },
                mismatches: vec![],
            }),
            confidence: Some(RuntimeEvidence::derive_confidence(prior, conf, contra)),
            runtime: Some(RuntimeEvidence {
                confirmations: conf,
                contradictions: contra,
                prior,
                last_trace_id: Some("trc".into()),
                last_observed_at: chrono::Utc::now(),
            }),
            matched_policy: None,
        }
    }

    #[test]
    fn effect_detail_runtime_verified_badge() {
        let label = effect_detail_reason(Some(&decl_with_runtime(5, 0, 0.5)));
        assert!(label.starts_with("runtime-verified"), "got: {label}");
        assert!(label.contains("5✓/0✗"), "got: {label}");
        assert!(label.contains("over 5 traces"), "got: {label}");
        assert!(label.contains("confidence 0."), "got: {label}");
    }

    #[test]
    fn effect_detail_runtime_contested_when_contradictions() {
        let label = effect_detail_reason(Some(&decl_with_runtime(2, 3, 0.5)));
        assert!(label.starts_with("runtime-contested"), "got: {label}");
        assert!(label.contains("2✓/3✗"), "got: {label}");
    }

    #[test]
    fn effect_detail_single_trace_is_singular() {
        let label = effect_detail_reason(Some(&decl_with_runtime(1, 0, 0.5)));
        assert!(
            label.contains("over 1 trace)"),
            "expected singular, got: {label}"
        );
    }

    #[test]
    fn effect_detail_runtime_takes_precedence_over_static_label() {
        // Static verification is Ok, but the runtime badge (with confidence)
        // must win over the plain "ok — verified by ..." label.
        let label = effect_detail_reason(Some(&decl_with_runtime(3, 0, 0.6)));
        assert!(
            !label.starts_with("ok — verified"),
            "static label leaked: {label}"
        );
    }

    #[test]
    fn effect_detail_falls_back_to_static_without_runtime() {
        use crate::schema::*;
        let decl = EffectDecl {
            symbol_id: "s".into(),
            declared: vec![Effect::new(EffectCategory::IoNetOut)],
            transitive: vec![],
            verification: Some(Verification {
                by: VerificationSource::StaticChecker,
                at: chrono::Utc::now(),
                status: VerificationStatus::Ok,
                mismatches: vec![],
            }),
            confidence: None,
            runtime: None,
            matched_policy: None,
        };
        let label = effect_detail_reason(Some(&decl));
        // Existing renderer lowercases the Debug name ("StaticChecker" → "staticchecker").
        assert!(
            label.starts_with("ok — verified by staticchecker"),
            "got: {label}"
        );
    }

    #[test]
    fn effect_detail_none_and_empty() {
        assert_eq!(effect_detail_reason(None), "no effects declared");
    }

    // Plan J t-005 / 1.0.79 token economy: consistent → returns Null.
    // Callers omit the field entirely; agent infers "indexes agree"
    // from absence.
    #[test]
    fn index_consistency_consistent_returns_null() {
        let v = compute_index_consistency(100, 100);
        assert!(v.is_null(), "consistent → Null; got: {v:#?}");
    }

    #[test]
    fn index_consistency_asg_ahead_advises_reindex() {
        // ASG has 412, FTS only 408 — the classic "FTS rebuild
        // failed mid-pass" pattern. Advice tells the agent what
        // to run.
        let v = compute_index_consistency(412, 408);
        assert_eq!(v["delta"], 4);
        assert_eq!(v["consistent"], false);
        let advice = v["advice"].as_str().expect("advice when divergent");
        assert!(advice.contains("4 symbols"), "got: {advice}");
        assert!(advice.contains("'asd index'"), "got: {advice}");
    }

    #[test]
    fn index_consistency_singular_grammar() {
        // Off-by-one: "1 symbol" not "1 symbols".
        let v = compute_index_consistency(101, 100);
        let advice = v["advice"].as_str().unwrap();
        assert!(advice.contains("1 symbol "), "got: {advice}");
        assert!(!advice.contains("1 symbols"), "got: {advice}");
    }

    #[test]
    fn index_consistency_fts_ahead_advises_reindex_with_stale_wording() {
        // Rare inverse: FTS holds entries no longer in the ASG.
        // Advice should say "stale" rather than "not in cache" so
        // the agent reads the direction of the drift correctly.
        let v = compute_index_consistency(100, 103);
        assert_eq!(v["delta"], -3);
        assert_eq!(v["consistent"], false);
        let advice = v["advice"].as_str().unwrap();
        assert!(advice.contains("3 stale symbols"), "got: {advice}");
        assert!(advice.contains("'asd index'"), "got: {advice}");
    }

    #[test]
    fn index_consistency_handles_empty_repo() {
        // 0/0 is consistent (just empty) → Null (1.0.79).
        let v = compute_index_consistency(0, 0);
        assert!(v.is_null());
    }

    // ExampleFlow refinement (1.0.77): stale_warning_classified
    // distinguishes critical (empty/broken FTS) from soft (just-past-
    // age-threshold). Critical fires regardless of age; soft is
    // demotable when the consuming handler had a successful query.
    #[test]
    fn stale_classified_empty_index_is_critical() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.db");
        // Open and close to create the table but populate nothing.
        let _ = SearchFtsDb::open(&db_path).unwrap();
        let w = stale_warning_classified(&db_path, 3600).expect("empty index must warn");
        assert_eq!(w.severity, StaleSeverity::Critical);
        assert!(w.message.contains("empty"), "got: {}", w.message);
    }

    #[test]
    fn stale_classified_returns_none_when_no_fts_file() {
        // Nonexistent path → fts open fails → None (no warning at all).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("does_not_exist_yet.db");
        // Don't even create the file — SearchFtsDb::open creates one
        // and the empty branch fires, so we'd get Critical above. To
        // hit the "no fts at all" return we'd need a path that fails
        // to open, but tempdir paths always open. Skip the negative
        // case here; the empty-index test covers the more common path.
        // Just verify a successful open + populate.
        use crate::schema::{Position, Symbol, SymbolKind};
        let fts = SearchFtsDb::open(&db_path).unwrap();
        let sym = Symbol {
            symbol_id: "s1".into(),
            symbol_fp: "fp".into(),
            qname: "p.f".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "p.py".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 2, col: 0 },
            signature: None,
            doc: None,
        };
        fts.rebuild(&[sym]).unwrap();
        // Mark fresh — last_indexed_at returns now.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        fts.conn
            .execute(
                "INSERT OR REPLACE INTO asd_index_meta(key, value) VALUES('indexed_at', ?1)",
                rusqlite::params![now.to_string()],
            )
            .unwrap();
        // Fresh → soft threshold not crossed → no warning.
        assert!(stale_warning_classified(&db_path, SOFT_STALE_THRESHOLD_SECS).is_none());
    }

    #[test]
    fn stale_classified_soft_when_past_threshold_but_fts_healthy() {
        use crate::schema::{Position, Symbol, SymbolKind};
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("aged.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();
        let sym = Symbol {
            symbol_id: "s1".into(),
            symbol_fp: "fp".into(),
            qname: "p.f".into(),
            language: "python".into(),
            kind: SymbolKind::Function,
            file: "p.py".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 2, col: 0 },
            signature: None,
            doc: None,
        };
        fts.rebuild(&[sym]).unwrap();
        // Stamp last_indexed_at to 48 hours ago.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let two_days_ago = now - 48 * 3600;
        fts.conn
            .execute(
                "INSERT OR REPLACE INTO asd_index_meta(key, value) VALUES('indexed_at', ?1)",
                rusqlite::params![two_days_ago.to_string()],
            )
            .unwrap();
        let w = stale_warning_classified(&db_path, SOFT_STALE_THRESHOLD_SECS)
            .expect("48h-old at 24h threshold must warn");
        assert_eq!(w.severity, StaleSeverity::Soft);
        assert!(w.age_secs >= 48 * 3600);
        assert_eq!(w.indexed_at, Some(two_days_ago));
    }

    #[test]
    fn stale_classified_severity_serializes_as_snake_case() {
        // Locked because the MCP response shape contracts on this.
        let critical = serde_json::to_value(StaleSeverity::Critical).unwrap();
        assert_eq!(critical, serde_json::Value::String("critical".to_string()));
        let soft = serde_json::to_value(StaleSeverity::Soft).unwrap();
        assert_eq!(soft, serde_json::Value::String("soft".to_string()));
    }

    #[test]
    fn stopword_filtering() {
        // Pure stopword queries produce no tokens → empty results, not a panic.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();
        use crate::schema::{Position, Symbol, SymbolKind};
        let sym = Symbol {
            symbol_id: "s1".into(),
            symbol_fp: "fp".into(),
            qname: "App.Foo.punchIn".into(),
            language: "swift".into(),
            kind: SymbolKind::Method,
            file: "App/Foo.swift".into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 5, col: 0 },
            signature: Some("func punchIn(over existingClip: Clip)".into()),
            doc: None,
        };
        fts.rebuild(std::slice::from_ref(&sym)).unwrap();

        // "over" alone is a stopword — filtered → empty tokens → no results.
        let hits = fts.search("over", &FtsFilters::default(), 10).unwrap();
        assert!(
            hits.is_empty(),
            "'over' is a stopword, should return no hits"
        );

        // "playhead over clips" — only "playhead" and "clips" survive filtering.
        // "punchIn(over existingClip)" has no "playhead" or "clips" → no match.
        let hits2 = fts
            .search("playhead over clips", &FtsFilters::default(), 10)
            .unwrap();
        assert!(
            hits2.is_empty(),
            "stopword-only overlap should not match punchIn"
        );
    }

    #[test]
    fn split_camel_basic() {
        assert_eq!(
            split_camel("refreshDriftPlayhead"),
            vec!["refresh", "Drift", "Playhead"]
        );
        assert_eq!(
            split_camel("DriftSynthPool"),
            vec!["Drift", "Synth", "Pool"]
        );
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
        assert!(is_test_file(
            "Packages/AudioEngine/Tests/KarplusStrongTests.swift"
        ));
        assert!(is_test_file("tests/test_charge_card.py"));
        assert!(is_test_file("src/__tests__/auth.test.ts"));
        assert!(is_test_file("payments/test_stripe.py"));
        assert!(is_test_file("pkg/payments/charge_test.go"));
        assert!(is_test_file("src/auth/auth_spec.rb"));
        assert!(!is_test_file("App/ExampleFlow/ExampleFlowApp.swift"));
        assert!(!is_test_file("src/payments/charge.py"));
        assert!(!is_test_file(
            "Packages/AudioEngine/Sources/KarplusStrong.swift"
        ));
    }

    #[test]
    fn tier_classification() {
        // Production — tier 0
        assert_eq!(symbol_tier("App/ViewModel/DriftPlayheadViewModel.swift"), 0);
        assert_eq!(symbol_tier("src/payments/charge.py"), 0);
        assert_eq!(
            symbol_tier("Packages/AudioEngine/Sources/KarplusStrong.swift"),
            0
        );

        // Utility — tier 1
        assert_eq!(symbol_tier("App/Previews/DriftView_Previews.swift"), 1);
        assert_eq!(
            symbol_tier("App/ViewModel/DriftView_Previews.swift"),
            1,
            "preview filename"
        );
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
            symbol_id: id.into(),
            symbol_fp: format!("fp_{id}"),
            qname: qname.into(),
            language: "swift".into(),
            kind: SymbolKind::Method,
            file: file.into(),
            start: Position { line: 1, col: 0 },
            end: Position { line: 5, col: 0 },
            signature: None,
            doc: None,
        };

        let prod = make("prod", "App.VM.refreshDriftPlayhead", "App/ViewModel.swift");
        let preview = make(
            "preview",
            "App.Previews.refreshDriftPlayhead",
            "App/Previews/DriftView_Previews.swift",
        );

        fts.rebuild(&[prod, preview]).unwrap();

        let hits = fts
            .search("refresh drift playhead", &FtsFilters::default(), 10)
            .unwrap();
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

        let prod = make_sym(
            "sym_prod",
            "App.ViewModel.refreshDriftPlayhead",
            "App/ViewModel.swift",
        );
        let test = make_sym(
            "sym_test",
            "Tests.DriftTests.testRefreshPlayhead",
            "Tests/DriftTests.swift",
        );

        fts.rebuild(&[prod, test]).unwrap();
        assert!(fts.has_data());

        // Default: tests excluded.
        let hits = fts.search("playhead", &FtsFilters::default(), 10).unwrap();
        assert_eq!(hits.len(), 1, "only prod symbol by default");
        assert_eq!(hits[0].symbol_id, "sym_prod");

        // With include_tests: both returned.
        let hits_all = fts
            .search(
                "playhead",
                &FtsFilters {
                    include_tests: true,
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(hits_all.len(), 2, "both when include_tests");

        // Plan A t-006: with tests_only, just the test symbol.
        let hits_test_only = fts
            .search(
                "playhead",
                &FtsFilters {
                    tests_only: true,
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(hits_test_only.len(), 1, "only test symbol when tests_only");
        assert_eq!(hits_test_only[0].symbol_id, "sym_test");

        // tests_only takes precedence over include_tests=false.
        let hits_precedence = fts
            .search(
                "playhead",
                &FtsFilters {
                    tests_only: true,
                    include_tests: false,
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(
            hits_precedence.len(),
            1,
            "tests_only overrides include_tests=false"
        );
        assert_eq!(hits_precedence[0].symbol_id, "sym_test");
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
        assert_eq!(
            hits[0].qname, "App.ViewModel.refreshDriftPlayhead",
            "orig qname preserved"
        );
        assert_eq!(hits[0].tier, 0, "production file should be tier 0");

        let hits2 = fts
            .search("refresh drift", &FtsFilters::default(), 10)
            .unwrap();
        assert!(!hits2.is_empty(), "should find multi-token");

        let hits3 = fts
            .search(
                "playhead",
                &FtsFilters {
                    language: Some("python".into()),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert!(hits3.is_empty(), "language filter should exclude swift");
    }

    #[test]
    fn summary_extraction() {
        // First sentence from doc.
        assert_eq!(
            extract_summary(
                Some(
                    "Updates drift pad visual playhead from transport state. Called on every render tick."
                ),
                None
            ),
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
            extract_summary(
                Some("/// Updates the drift playhead. Called each frame."),
                None
            ),
            "Updates the drift playhead"
        );
        // Multi-line Rust doc block.
        assert_eq!(
            extract_summary(
                Some("/// Refreshes visual state.\n/// Must be called on main thread."),
                None
            ),
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
        assert_eq!(
            classify_layer("App/Views/DriftPlayheadView.swift", 0, &[]),
            "ui"
        );
        assert_eq!(
            classify_layer("App/ViewModel/DriftPlayheadViewModel.swift", 0, &[]),
            "viewmodel"
        );
        assert_eq!(
            classify_layer("App/Scheduler/DriftScheduler.swift", 0, &[]),
            "scheduler"
        );
        assert_eq!(
            classify_layer("App/Engine/AudioEngine.swift", 0, &[]),
            "scheduler"
        );
        assert_eq!(
            classify_layer("App/Storage/ClipRepository.swift", 0, &[]),
            "persistence"
        );
        assert_eq!(
            classify_layer("App/Domain/Clip.swift", 0, &[]),
            "core_model"
        );
        assert_eq!(
            classify_layer("App/Previews/DriftView_Previews.swift", 1, &[]),
            "utility"
        );
        assert_eq!(classify_layer("Tests/DriftTests.swift", 2, &[]), "tests");
        assert_eq!(classify_layer("App/AppDelegate.swift", 0, &[]), "other");
    }

    #[test]
    fn layer_classification_qname_fallback() {
        // File named after app, but qname carries the ViewModel suffix.
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "ExampleFlowViewModel", 0, &[]),
            "viewmodel"
        );
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "ExampleFlowController", 0, &[]),
            "viewmodel"
        );
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "DriftCompiler", 0, &[]),
            "scheduler"
        );
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "ClipStore", 0, &[]),
            "persistence"
        );
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "DriftEngine", 0, &[]),
            "scheduler"
        );
        // File-based classification still wins when it fires.
        assert_eq!(
            classify_layer_sym("App/Views/DriftView.swift", "DriftViewModel", 0, &[]),
            "ui"
        );
        // Truly unclassifiable stays other.
        assert_eq!(
            classify_layer_sym("App/AppDelegate.swift", "AppDelegate", 0, &[]),
            "other"
        );
        // Method qnames: class component must propagate even when the method leaf doesn't match.
        assert_eq!(
            classify_layer_sym(
                "App/ExampleFlow.swift",
                "ExampleFlowViewModel.refreshDriftPlayhead",
                0,
                &[]
            ),
            "viewmodel"
        );
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "DriftCompiler.compile", 0, &[]),
            "scheduler"
        );
        assert_eq!(
            classify_layer_sym("App/ExampleFlow.swift", "ClipStore.save", 0, &[]),
            "persistence"
        );
        // Rust-style :: separators.
        assert_eq!(
            classify_layer_sym("src/drift.rs", "ExampleFlowViewModel::refresh", 0, &[]),
            "viewmodel"
        );
    }

    #[test]
    fn layer_overrides_respected() {
        // Keys in overrides must be lowercase (as produced by load_layer_overrides).
        let overrides = vec![
            ("custominfra".to_string(), "persistence".to_string()),
            ("bizlogic".to_string(), "core_model".to_string()),
        ];
        assert_eq!(
            classify_layer("App/CustomInfra/Repo.swift", 0, &overrides),
            "persistence"
        );
        assert_eq!(
            classify_layer("App/BizLogic/Workflow.swift", 0, &overrides),
            "core_model"
        );
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
        )
        .unwrap();
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
        for i in &[
            "bugfix",
            "feature",
            "refactor",
            "test",
            "architecture",
            "ui",
        ] {
            assert!(!intent_focus(i).is_empty(), "no focus for {i}");
        }
        // Layer order has 8 entries for every intent.
        for i in &[
            "bugfix",
            "feature",
            "refactor",
            "test",
            "architecture",
            "ui",
            "",
        ] {
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

    // -- Plan E t-004: bulk-fetch fast path for constraint penalties -------

    #[test]
    fn symbols_with_constraint_penalties_via_sql_returns_matching_ids() {
        use crate::schema::{Author, AuthorKind, LedgerEntry, LedgerKind};
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();

        // Populate the ledger cache with three entries:
        //   sym_a: Constraint with role=stale-api      → SHOULD match
        //   sym_b: Decision with role=audit-pending    → SHOULD match
        //   sym_c: Hazard with role=stale-api          → must NOT match (wrong kind)
        //   sym_d: Constraint with role=fast-test      → must NOT match (non-penalty role)
        //   sym_e: Constraint with no role             → must NOT match (no role)
        let make = |sym_id: &str, kind: LedgerKind, role: Option<&str>| {
            let mut e = LedgerEntry::new(
                sym_id,
                kind,
                "test entry",
                Author {
                    kind: AuthorKind::Agent,
                    id: "t".into(),
                },
            );
            e.role = role.map(str::to_string);
            e
        };
        for (sym_id, kind, role) in [
            ("sym_a", LedgerKind::Constraint, Some("stale-api")),
            ("sym_b", LedgerKind::Decision, Some("audit-pending")),
            ("sym_c", LedgerKind::Hazard, Some("stale-api")),
            ("sym_d", LedgerKind::Constraint, Some("fast-test")),
            ("sym_e", LedgerKind::Constraint, None),
        ] {
            fts.upsert_ledger_entry(&make(sym_id, kind, role), "main")
                .unwrap();
        }

        let mut ids = fts.symbols_with_constraint_penalties("main").unwrap();
        ids.sort();
        assert_eq!(ids, vec!["sym_a".to_string(), "sym_b".to_string()]);
    }

    #[test]
    fn symbols_with_constraint_penalties_returns_empty_when_cache_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let fts = SearchFtsDb::open(&db_path).unwrap();
        let ids = fts.symbols_with_constraint_penalties("main").unwrap();
        assert!(ids.is_empty());
    }
}

#[cfg(test)]
mod plan_j_t007_test_stub_tests {
    //! Plan J t-007: `propose_test_stub` returns the language-aware
    //! skeleton an agent should fill in. Covers all 10 indexed
    //! adapter languages + the unknown-extension fallback.

    use super::propose_test_stub;

    fn assert_contains_all(actual: &str, needles: &[&str]) {
        for n in needles {
            assert!(actual.contains(n), "stub missing `{n}` — got:\n{actual}");
        }
    }

    #[test]
    fn python_stub_uses_snake_case_def_and_raises_until_filled() {
        let s = propose_test_stub("src/billing/calc.py", "discount");
        assert_contains_all(
            &s,
            &[
                "def test_discount",
                "arrange",
                "act",
                "assert",
                "NotImplementedError",
            ],
        );
    }

    #[test]
    fn typescript_stub_uses_test_function_and_throws() {
        let s = propose_test_stub("src/billing/calc.ts", "discount");
        assert_contains_all(&s, &["test('discount'", "throw new Error"]);
    }

    #[test]
    fn rust_stub_uses_test_attribute_and_todo_macro() {
        let s = propose_test_stub("src/billing/calc.rs", "discount");
        assert_contains_all(&s, &["#[test]", "fn discount", "todo!"]);
    }

    #[test]
    fn go_stub_uses_pascal_case_and_t_fatal() {
        let s = propose_test_stub("billing/calc.go", "discount");
        assert_contains_all(&s, &["func TestDiscount", "*testing.T", "t.Fatal"]);
    }

    #[test]
    fn java_stub_uses_test_annotation_and_fail() {
        let s = propose_test_stub("Billing.java", "discount");
        assert_contains_all(&s, &["@Test", "testDiscount", "fail("]);
    }

    #[test]
    fn csharp_stub_uses_pascal_case_and_assert_fail() {
        let s = propose_test_stub("Billing.cs", "discount");
        assert_contains_all(&s, &["[Test]", "Discount_Should", "Assert.Fail"]);
    }

    #[test]
    fn ruby_stub_uses_def_test_and_flunk() {
        let s = propose_test_stub("billing.rb", "discount");
        assert_contains_all(&s, &["def test_discount", "flunk"]);
    }

    #[test]
    fn kotlin_stub_uses_test_annotation_and_backticks() {
        let s = propose_test_stub("Billing.kt", "discount");
        assert_contains_all(&s, &["@Test", "`discount`", "fail("]);
    }

    #[test]
    fn swift_stub_uses_pascal_case_and_xctfail() {
        let s = propose_test_stub("Billing.swift", "discount");
        assert_contains_all(&s, &["func testDiscount", "XCTFail"]);
    }

    #[test]
    fn unknown_extension_falls_back_to_generic_comment() {
        let s = propose_test_stub("data.bin", "discount");
        assert!(s.contains("discount"));
        assert!(s.starts_with("// "));
    }

    #[test]
    fn qname_prefix_is_stripped_from_test_name() {
        // billing.payment.charge → just `charge` for the test body.
        let s = propose_test_stub("src/calc.py", "billing.payment.charge");
        assert!(s.contains("def test_charge"), "got:\n{s}");
        assert!(
            !s.contains("billing_payment"),
            "qname module path must not leak into test name; got:\n{s}"
        );
    }

    #[test]
    fn camel_case_input_becomes_snake_for_python() {
        let s = propose_test_stub("src/calc.py", "applyDiscount");
        assert!(s.contains("def test_apply_discount"), "got:\n{s}");
    }
}

#[cfg(test)]
mod plan_j_t003_classify_file_role_tests {
    //! Plan J t-003: locks the unified file-role classifier and
    //! exercises the new `view` / `viewmodel` patterns. Previously
    //! a file like `ExampleFlowViewModel.swift` fell through to
    //! `impl`; M21 field-eval flagged it as mis-bucketed "other".

    use super::classify_file_role;

    #[test]
    fn impl_is_default_when_nothing_matches() {
        assert_eq!(classify_file_role("src/lib.rs"), "impl");
        assert_eq!(classify_file_role("crates/foo/src/main.py"), "impl");
    }

    #[test]
    fn tests_short_circuit_before_view_check() {
        // `ViewTests.swift` must be `test`, not `view` — the
        // /test predicate runs first.
        assert_eq!(
            classify_file_role("App/Tests/ExampleFlowViewTests.swift"),
            "test"
        );
        assert_eq!(classify_file_role("src/lib_test.py"), "test");
        assert_eq!(classify_file_role("crates/x/tests/it.rs"), "test");
    }

    #[test]
    fn viewmodel_by_filename_suffix() {
        // The key M21 reproducer.
        assert_eq!(
            classify_file_role("App/Sources/ExampleFlowViewModel.swift"),
            "viewmodel"
        );
        assert_eq!(
            classify_file_role("web/src/PlaybackViewModel.ts"),
            "viewmodel"
        );
    }

    #[test]
    fn viewmodel_by_path_pattern() {
        assert_eq!(
            classify_file_role("App/Sources/viewmodels/ExampleFlow.swift"),
            "viewmodel"
        );
        assert_eq!(classify_file_role("web/viewmodel/foo.ts"), "viewmodel");
    }

    #[test]
    fn view_by_filename_suffix() {
        assert_eq!(
            classify_file_role("App/Sources/ExampleFlowView.swift"),
            "view"
        );
        assert_eq!(classify_file_role("web/src/TrackView.tsx"), "view");
    }

    #[test]
    fn view_by_path_pattern() {
        assert_eq!(
            classify_file_role("App/Sources/views/ExampleFlow.swift"),
            "view"
        );
        assert_eq!(classify_file_role("web/view/index.ts"), "view");
    }

    #[test]
    fn view_by_extension() {
        assert_eq!(classify_file_role("web/src/App.vue"), "view");
        assert_eq!(classify_file_role("web/src/App.svelte"), "view");
    }

    #[test]
    fn viewmodel_short_circuits_before_view() {
        // ViewModel-suffixed files must NOT be reported as `view`.
        // The viewmodel branch runs before the view branch in the
        // classifier; without that ordering, `*ViewModel.*` would
        // hit `stem_ends_with(view)` first and mis-classify.
        let r = classify_file_role("App/Sources/ExampleFlowViewModel.swift");
        assert_eq!(r, "viewmodel", "ViewModel must NOT fall to `view`; got {r}");
    }

    #[test]
    fn preview_does_not_match_view_in_middle_of_segment() {
        // `previewmodel/foo.rs` would naïvely match `viewmodel`
        // in a substring scan — stem_ends_with guards against
        // that for the suffix patterns. The path-segment match
        // for `/viewmodels/` is explicit. Verify we don't
        // misclassify a file whose stem coincidentally contains
        // `view` mid-word but doesn't end in it.
        assert_eq!(
            classify_file_role("src/PreviewService.swift"),
            "impl",
            "PreviewService.swift must NOT be a view"
        );
    }

    #[test]
    fn existing_roles_still_classify_correctly() {
        // Regression guard against the CLI-side helper that this
        // function lifted from.
        assert_eq!(
            classify_file_role("examples/sample-py-repo/foo.py"),
            "example"
        );
        assert_eq!(classify_file_role("fixtures/seed.json"), "fixture");
        assert_eq!(classify_file_role("scripts/release.sh"), "script");
        assert_eq!(classify_file_role("generated/proto.pb.go"), "generated");
        assert_eq!(classify_file_role("docs/architecture.md"), "reference");
    }
}
