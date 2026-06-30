//! Feedback store — durable (query, symbol, verdict) triples.
//!
//! Agents and users record verdicts on search results via `asd feedback mark`
//! or the MCP `feedback_mark` tool. Verdicts are stored in the ASD sidecar
//! and applied as score adjustments in `apply_feedback_adjustments`.
//!
//! ## SQLite write-through cache
//!
//! `AsgFeedbackStore` optionally holds a borrowed `fts` connection — when
//! present, `list_all` returns the SQLite cache (fast, ~0 git reads) and
//! `record` additionally writes to SQLite after the git commit.  The git
//! object store remains authoritative: if SQLite is empty (e.g. first run
//! after `git pull`), we fall back to the git tree walk and re-populate the
//! cache as a side effect.  Running `asd index` / `asd reindex` calls
//! `sync_feedback_entries` for a full reconciliation.

use agentstategraph::{CommitOptions, Repository};
use agentstategraph_core::IntentCategory;
use chrono::{DateTime, Utc};

use crate::engine::Engine;
use crate::error::Result;
use crate::paths;
use crate::schema::{FeedbackEntry, FeedbackVerdict};
use crate::search_fts::SearchFtsDb;

// ---------------------------------------------------------------------------
// Plan J t-016: feedback decay
// ---------------------------------------------------------------------------

/// Default half-life for feedback boost decay, in days.
///
/// At 90 days, a verdict 3 months old gets 50% of its original weight,
/// 6 months → 25%, 1 year → 6%. Matches the spec's intuition that
/// "Useful +1.5 boosts stay forever" was wrong: an opinion about a
/// codebase from 6 months ago is much less likely to still apply than
/// one from yesterday. Code drifts, ownership changes, names get
/// refactored, mental models age out.
///
/// Picking 90 days specifically: half-life of one quarter aligns with
/// most teams' planning cadence — a verdict survives one quarter at
/// near-full weight, then meaningfully fades. Tunable via the
/// half-life parameter of `decay_factor` for callers that want a
/// different curve (longer half-life for slow-moving codebases, shorter
/// for high-churn ones).
pub const DEFAULT_FEEDBACK_HALF_LIFE_DAYS: f64 = 90.0;

/// Pure decay function. Returns a multiplier in `[0.0, 1.0]` for an
/// observation of `age_days` against `half_life_days`. Uses the
/// classical exponential decay: `factor = 0.5 ^ (age / half_life)`.
///
/// Properties (locked by unit tests):
///   - age = 0           → factor = 1.0 (full weight)
///   - age = half_life   → factor = 0.5
///   - age = 2*half_life → factor = 0.25
///   - age = 10*half_life → factor ≈ 0.001 (effectively gone)
///   - negative age (clock skew, future-dated entry) → factor = 1.0
///     (defensive: don't *amplify* boosts for future timestamps)
///   - half_life ≤ 0     → factor = 1.0 (decay disabled)
pub fn decay_factor(age_days: f64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    if age_days <= 0.0 {
        return 1.0;
    }
    0.5_f64.powf(age_days / half_life_days)
}

/// Compute the decay multiplier for an entry whose `created_at` is
/// known, given the current time. Used at feedback-boost application
/// time to scale `Useful` verdicts by age.
///
/// `now` is taken as a parameter (rather than calling `Utc::now()`
/// internally) so tests can pass fixed timestamps and so multiple
/// decay computations in the same scoring pass share a single
/// consistent "now".
pub fn decay_for_entry(
    created_at: DateTime<Utc>,
    now: DateTime<Utc>,
    half_life_days: f64,
) -> f64 {
    let age_secs = (now - created_at).num_seconds() as f64;
    let age_days = age_secs / 86_400.0;
    decay_factor(age_days, half_life_days)
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub trait FeedbackStore {
    /// Record a verdict for a (query, symbol) pair.
    fn record(&self, ref_name: &str, entry: &FeedbackEntry, agent_id: &str) -> Result<()>;

    /// All feedback entries recorded for a specific symbol.
    fn list_for_symbol(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<FeedbackEntry>>;

    /// Every feedback entry in the store.
    fn list_all(&self, ref_name: &str) -> Result<Vec<FeedbackEntry>>;

    /// Flatten all feedback into (symbol_id, query, verdict, created_at)
    /// tuples for use in `apply_feedback_adjustments`. Plan J t-016 added
    /// `created_at` so the boost arithmetic can decay Useful verdicts by
    /// age — a 6-month-old opinion about a codebase shouldn't carry the
    /// same weight as yesterday's.
    fn flat_verdicts(
        &self,
        ref_name: &str,
    ) -> Result<Vec<(String, String, FeedbackVerdict, DateTime<Utc>)>> {
        Ok(self
            .list_all(ref_name)?
            .into_iter()
            .filter(|e| e.file_scope.is_none())
            .map(|e| (e.symbol_id, e.query, e.verdict, e.created_at))
            .collect())
    }

    /// Flatten file-scoped feedback into (file_glob, verdict, query, created_at)
    /// tuples for use in `apply_file_scope_feedback`. Plan J t-016: same
    /// reason — decay file-scope Useful verdicts by age.
    fn flat_file_scope_verdicts(
        &self,
        ref_name: &str,
    ) -> Result<Vec<(String, FeedbackVerdict, String, DateTime<Utc>)>> {
        Ok(self
            .list_all(ref_name)?
            .into_iter()
            .filter_map(|e| {
                e.file_scope
                    .map(|glob| (glob, e.verdict, e.query, e.created_at))
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// ASG-backed implementation
// ---------------------------------------------------------------------------

pub struct AsgFeedbackStore<'a> {
    pub repo: &'a Repository,
    /// Borrowed FTS connection from the owning `Engine`.  When `Some`,
    /// enables the SQLite write-through cache: `list_all` reads from SQLite
    /// if populated, `record` writes to SQLite after the git commit.
    pub fts: Option<&'a SearchFtsDb>,
}

impl<'a> AsgFeedbackStore<'a> {
    /// Construct without SQLite caching (tests, internal calls).
    pub fn new(repo: &'a Repository) -> Self {
        Self { repo, fts: None }
    }
    /// Convenience: borrow the FTS connection already open in `engine`.
    pub fn from_engine(engine: &'a Engine) -> Self {
        Self {
            repo: &engine.repo,
            fts: engine.fts.as_ref(),
        }
    }
}

impl<'a> FeedbackStore for AsgFeedbackStore<'a> {
    fn record(&self, ref_name: &str, entry: &FeedbackEntry, agent_id: &str) -> Result<()> {
        // Git is always written first — it's the authoritative store.
        let path = paths::feedback_entry_path(&entry.symbol_id, &entry.entry_id);
        let value = serde_json::to_value(entry)?;
        let opts = CommitOptions::new(
            agent_id,
            IntentCategory::Refine,
            format!(
                "feedback {} for {}",
                entry.verdict.as_str(),
                entry.symbol_qname
            ),
        );
        self.repo.set_json(ref_name, &path, &value, opts)?;
        // Best-effort SQLite write-through; failures are non-fatal.
        if let Some(fts) = self.fts {
            let _ = fts.upsert_feedback(entry);
        }
        Ok(())
    }

    fn list_for_symbol(&self, ref_name: &str, symbol_id: &str) -> Result<Vec<FeedbackEntry>> {
        let prefix = paths::feedback_symbol_path(symbol_id);
        match self.repo.get_tree(ref_name, &prefix) {
            Ok(serde_json::Value::Object(map)) => {
                let mut entries: Vec<FeedbackEntry> = map
                    .values()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect();
                entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(entries)
            }
            _ => Ok(vec![]),
        }
    }

    fn list_all(&self, ref_name: &str) -> Result<Vec<FeedbackEntry>> {
        // Fast path: SQLite cache — zero git tree walks when populated.
        if let Some(fts) = self.fts {
            if fts.feedback_count() > 0 {
                if let Ok(entries) = fts.list_all_feedback() {
                    return Ok(entries);
                }
            }
        }

        // Authoritative git path — also runs when SQLite is empty (e.g. first
        // run after `git pull`).  Re-populate the cache as a side effect so
        // subsequent calls are fast.
        let prefix = format!("{}/feedback", paths::ASD_ROOT);
        let mut entries = Vec::new();
        if let Ok(serde_json::Value::Object(by_symbol)) = self.repo.get_tree(ref_name, &prefix) {
            for symbol_val in by_symbol.values() {
                if let serde_json::Value::Object(symbol_entries) = symbol_val {
                    for ev in symbol_entries.values() {
                        if let Ok(e) = serde_json::from_value::<FeedbackEntry>(ev.clone()) {
                            entries.push(e);
                        }
                    }
                }
            }
        }
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Populate the SQLite cache for the next call — best effort.
        if !entries.is_empty() {
            if let Some(fts) = self.fts {
                let _ = fts.sync_feedback_entries(&entries);
            }
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod plan_j_t016_decay_tests {
    //! Plan J t-016: feedback decay arithmetic.
    //!
    //! Locks the pure `decay_factor` + `decay_for_entry` helpers
    //! against the spec'd properties (zero age → 1.0, half-life →
    //! 0.5, multiple half-lives → geometric falloff, defensive
    //! handling of clock skew and zero half_life).

    use super::*;
    use chrono::Duration;

    fn approx(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-6
    }

    #[test]
    fn zero_age_returns_full_weight() {
        assert!(approx(decay_factor(0.0, 90.0), 1.0));
    }

    #[test]
    fn half_life_returns_one_half() {
        assert!(approx(decay_factor(90.0, 90.0), 0.5));
    }

    #[test]
    fn two_half_lives_returns_one_quarter() {
        assert!(approx(decay_factor(180.0, 90.0), 0.25));
    }

    #[test]
    fn ten_half_lives_effectively_zero() {
        let f = decay_factor(900.0, 90.0);
        assert!(f < 0.002, "10× half-life should decay to ~0.1%; got {f}");
        assert!(f > 0.0, "but still strictly positive");
    }

    #[test]
    fn negative_age_clamps_to_full_weight() {
        // Defensive against clock skew or future-dated entries:
        // a verdict from the "future" must not get AMPLIFIED above
        // 1.0 — that would let a malicious or buggy timestamp turn
        // a +1.5 boost into +1000.
        assert_eq!(decay_factor(-1.0, 90.0), 1.0);
        assert_eq!(decay_factor(-90.0, 90.0), 1.0);
    }

    #[test]
    fn zero_or_negative_half_life_disables_decay() {
        // Lets callers opt out of decay entirely without a
        // wrapper or branch — pass half_life = 0 → all weights
        // return 1.0 regardless of age.
        assert_eq!(decay_factor(365.0, 0.0), 1.0);
        assert_eq!(decay_factor(365.0, -10.0), 1.0);
    }

    #[test]
    fn quarter_year_at_default_half_life_matches_spec() {
        // 90-day half-life with a 90-day-old verdict → 50% weight.
        // 6-month verdict → 25%. The DESIGN.md spec wording
        // ("6 months should have less weight than yesterday's")
        // is operationalized by this curve.
        let day_old = decay_factor(1.0, DEFAULT_FEEDBACK_HALF_LIFE_DAYS);
        let six_months = decay_factor(180.0, DEFAULT_FEEDBACK_HALF_LIFE_DAYS);
        assert!(day_old > 0.98, "1-day-old must be near full weight; got {day_old}");
        assert!(approx(six_months, 0.25), "6-month must be ~25%; got {six_months}");
        assert!(
            day_old > six_months * 3.0,
            "yesterday's weight must dominate 6-month-old by >3×"
        );
    }

    #[test]
    fn decay_for_entry_threads_timestamps_correctly() {
        // Test the timestamp wrapper — equivalent of decay_factor
        // but takes DateTime<Utc> instead of pre-computed age.
        let now = Utc::now();
        let one_quarter_ago = now - Duration::days(90);
        let f = decay_for_entry(one_quarter_ago, now, 90.0);
        assert!(approx(f, 0.5), "90-day-old at 90-day half-life → 0.5; got {f}");
    }

    #[test]
    fn decay_for_entry_handles_subday_resolution() {
        // 12 hours old, 1 day half-life → 0.5^0.5 ≈ 0.707.
        // Ensures we don't truncate age to whole days.
        let now = Utc::now();
        let twelve_hours_ago = now - Duration::hours(12);
        let f = decay_for_entry(twelve_hours_ago, now, 1.0);
        // 12h of a 1-day half-life is exactly 0.5^0.5 = 1/sqrt(2). Use the
        // constant, not a 0.7071 literal (clippy::approx_constant).
        assert!(
            (f - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "12h/1day should produce ~0.707 (1/sqrt2); got {f}"
        );
    }

    #[test]
    fn decay_for_entry_with_future_timestamp_clamps_to_one() {
        // Clock skew defense via the wrapper.
        let now = Utc::now();
        let future = now + Duration::days(30);
        assert_eq!(decay_for_entry(future, now, 90.0), 1.0);
    }
}
