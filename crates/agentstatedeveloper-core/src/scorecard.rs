//! The five-dimension benchmark scorecard — truth, feedback, change,
//! uncertainty, workflow — plus the data-quality and token-economy blocks
//! that qualify them.
//!
//! **This is the only implementation of the arithmetic.** It previously
//! existed three times over: `asd scorecard`, the `scorecard` MCP tool, and
//! the `/api/v1/scorecard` handler each carried their own copy of the same
//! loop and the same formulas. That is the shape of defect this codebase has
//! already paid for once — see the calibration-table axis inversion in
//! CLAUDE.md, where every unit test passed because each test encoded the same
//! wrong assumption as the code it covered. Three copies of a formula are
//! three places for it to drift, each with its own tests agreeing with it.
//!
//! What stays with the callers is *phrasing and envelope*, not computation:
//! the CLI renders a table and keeps a trend snapshot, the HTTP handler wraps
//! the payload in a timestamp, and each composes its own advice string for
//! the "nothing matched" case, because "try broadening --scope/--paths" is
//! good counsel in a terminal and meaningless over HTTP. Everything that
//! produces a number lives here.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use crate::candidates::{glob_match, resolve_scope};
use crate::effects::{AsgEffectStore, EffectStore};
use crate::engine::Engine;
use crate::feedback::{AsgFeedbackStore, FeedbackStore};
use crate::schema::{LedgerEntry, LedgerKind, Symbol, VerificationStatus};
use crate::search_fts::estimate_tokens;

/// Rough source-token density used for the token-economy baseline: the cost
/// of *reading the file* a symbol lives in, against ASD's structured index
/// entry for it. An estimate, and labelled as one wherever it surfaces.
const TOKENS_PER_LINE: usize = 9;

/// Below this many ledger entries per symbol the scores describe how little
/// has been recorded, not how well the workflow is going — so every caller
/// surfaces the caveat rather than the bare number.
const SPARSE_LEDGER_DENSITY: f64 = 0.5;

/// Feedback entries that read as a full-marks corpus.
const FEEDBACK_TARGET: f64 = 50.0;

/// Symbol count past which the uncertainty dimension stops rewarding volume.
const VOLUME_TARGET: f64 = 500.0;

#[derive(Debug, Clone, Default)]
pub struct ScorecardOptions<'a> {
    /// Named scope alias from `.asd/scopes.toml`.
    pub scope: Option<&'a str>,
    /// Comma-separated glob patterns restricting which files are scored.
    pub paths: Option<&'a str>,
    /// Per-symbol gap listing for one dimension: truth / change / workflow /
    /// uncertainty. Anything else yields no rows.
    pub drill_down: Option<&'a str>,
    /// Cap on returned drill-down rows. `total_gaps` still reports the full
    /// count, so a truncated list never reads as a complete one.
    pub drill_limit: usize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Scores {
    pub truth: u64,
    pub feedback: u64,
    pub change: u64,
    pub uncertainty: u64,
    pub workflow: u64,
    pub overall: u64,
}

impl Scores {
    pub fn get(&self, dimension: &str) -> Option<u64> {
        Some(match dimension {
            "truth" => self.truth,
            "feedback" => self.feedback,
            "change" => self.change,
            "uncertainty" => self.uncertainty,
            "workflow" => self.workflow,
            "overall" => self.overall,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DataQuality {
    pub ledger_density: f64,
    pub symbols_scored: usize,
    pub symbols_with_any_ledger: usize,
    pub coverage_pct: f64,
    pub sparse_db: bool,
    pub note: String,
    /// The glob patterns in force, or `None` when unscoped.
    pub scope: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Details {
    pub total_symbols: usize,
    pub verified_effects: usize,
    pub owned_symbols: usize,
    pub invariant_symbols: usize,
    pub validation_symbols: usize,
    pub feedback_entries: usize,
    pub total_ledger_entries: usize,
    pub ctx_tagged_ledger_entries: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TokenEconomy {
    pub structured_tokens: usize,
    pub source_read_tokens_est: usize,
    pub reduction_pct: f64,
    pub ratio_x: f64,
}

/// The disclaimer that must travel with [`TokenEconomy`] wherever it is
/// rendered. Kept as a constant so no caller can quietly ship the ratio
/// without the caveat that it is an internal estimate.
pub const TOKEN_ECONOMY_NOTE: &str = "Internal estimate — NOT a published benchmark and NOT measured per query. \
     Compares ASD's structured per-symbol index cost (qname + signature + first doc \
     line) against reading the source files those symbols live in (file length \
     estimated from symbol line spans).";

#[derive(Debug, Clone, Serialize)]
pub struct GapSymbol {
    pub qname: String,
    pub file: String,
    pub has_verified_effects: bool,
    pub has_ownership: bool,
    pub has_invariant: bool,
    pub has_validation_scenario: bool,
    pub ledger_entries: usize,
    pub ctx_tagged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DrillDown {
    pub dimension: String,
    /// Every gap found, not just the returned ones.
    pub total_gaps: usize,
    pub shown: usize,
    pub omitted: usize,
    pub gap_symbols: Vec<GapSymbol>,
}

#[derive(Debug, Clone)]
pub struct Scorecard {
    pub scores: Scores,
    pub data_quality: DataQuality,
    pub details: Details,
    pub token_economy: TokenEconomy,
    pub drill_down: Option<DrillDown>,
    /// True when no symbol matched — every score is zero because there was
    /// nothing to score, which is a different claim from "scored zero".
    /// Callers phrase their own advice for this case.
    pub matched_nothing: bool,
    /// True when a scope/paths filter was applied at all. With
    /// `matched_nothing`, distinguishes "your filter matched nothing" from
    /// "this repo has never been indexed".
    pub scoped: bool,
}

impl Scorecard {
    /// The shared JSON envelope: `capability_scores` + the legacy `scores`
    /// alias, the qualifying blocks, and the drill-down when one was asked
    /// for.
    ///
    /// `scores` duplicates `capability_scores` because the CLI's trend
    /// snapshots on disk are keyed on `scores` — dropping it would orphan
    /// every stored history file. Callers add their own `timestamp` / `trend`
    /// / `note` around this.
    pub fn to_json(&self) -> Value {
        let scores = json!(self.scores);
        let mut out = json!({
            "capability_scores": scores,
            "scores": scores,
            "data_quality": self.data_quality,
            "details": self.details,
            "token_economy": {
                "note": TOKEN_ECONOMY_NOTE,
                "structured_tokens": self.token_economy.structured_tokens,
                "source_read_tokens_est": self.token_economy.source_read_tokens_est,
                "reduction_pct": self.token_economy.reduction_pct,
                "ratio_x": self.token_economy.ratio_x,
            },
        });
        if let Some(drill) = &self.drill_down {
            out.as_object_mut()
                .expect("json! built an object")
                .insert("drill_down".into(), json!(drill));
        }
        out
    }
}

/// Score the indexed symbol set.
///
/// One bulk read of the by-qname index and one of the ledger tree, then a
/// single pass — not N per-symbol git reads. `db_path` is needed only to
/// resolve a named `scope` alias against `.asd/scopes.toml`.
pub fn compute(engine: &Engine, db_path: &Path, opts: &ScorecardOptions<'_>) -> Scorecard {
    let ref_name = engine.ref_name.clone();
    let effect_store = AsgEffectStore::from_engine(engine);
    let feedback_store = AsgFeedbackStore::from_engine(engine);

    let mut paths_filter: Vec<String> = Vec::new();
    if let Some(s) = opts.scope {
        paths_filter.extend(resolve_scope(s, db_path));
    }
    if let Some(p) = opts.paths {
        paths_filter.extend(
            p.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty()),
        );
    }
    let scoped = !paths_filter.is_empty();

    let all_syms: Vec<Symbol> = {
        let tree = engine
            .repo
            .get_tree(&ref_name, "/asd/v1/index/by-qname")
            .unwrap_or(Value::Object(Default::default()));
        tree.as_object()
            .map(|m| {
                m.values()
                    .filter_map(|v| serde_json::from_value::<Symbol>(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    };

    let scored_syms: Vec<&Symbol> = if scoped {
        all_syms
            .iter()
            .filter(|s| paths_filter.iter().any(|p| glob_match(p, &s.file)))
            .collect()
    } else {
        all_syms.iter().collect()
    };
    let total_symbols = scored_syms.len();

    if total_symbols == 0 {
        return empty(scoped, paths_filter);
    }

    let ledger_by_sym = load_ledger(engine, &ref_name);

    let drill = opts.drill_down.unwrap_or("").to_lowercase();
    let need_drill = !drill.is_empty();
    let mut drill_rows: Vec<GapSymbol> = Vec::new();

    let mut verified_count = 0usize;
    let mut owned_count = 0usize;
    let mut has_invariant = 0usize;
    let mut has_validation = 0usize;
    let mut total_ledger_entries = 0usize;
    let mut ctx_tagged_entries = 0usize;

    let mut structured_tokens = 0usize;
    // Longest symbol span per file stands in for the file's length — the
    // index knows line spans without reading anything off disk.
    let mut file_max_line: HashMap<&str, u32> = HashMap::new();

    for sym in &scored_syms {
        let has_verified =
            if let Ok(Some(decl)) = effect_store.get_effects(&ref_name, &sym.symbol_id) {
                decl.verification
                    .as_ref()
                    .map(|v| matches!(v.status, VerificationStatus::Ok))
                    .unwrap_or(false)
            } else {
                false
            };
        if has_verified {
            verified_count += 1;
        }

        let record = format!(
            "{} {} {}",
            sym.qname,
            sym.signature.as_deref().unwrap_or(""),
            sym.doc
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        );
        structured_tokens += estimate_tokens(&record);
        let f = file_max_line.entry(sym.file.as_str()).or_insert(0);
        *f = (*f).max(sym.end.line);

        let entries = ledger_by_sym
            .get(&sym.symbol_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        total_ledger_entries += entries.len();

        let mut sym_owned = false;
        let mut sym_inv = false;
        let mut sym_vs = false;
        let mut sym_ctx = false;
        for entry in entries {
            match entry.kind {
                LedgerKind::Invariant => sym_inv = true,
                LedgerKind::ValidationScenario => sym_vs = true,
                LedgerKind::Ownership => sym_owned = true,
                _ => {}
            }
            if entry.tags.iter().any(|t| t.starts_with("ctx:")) {
                sym_ctx = true;
                ctx_tagged_entries += 1;
            }
        }
        if sym_owned {
            owned_count += 1;
        }
        if sym_inv {
            has_invariant += 1;
        }
        if sym_vs {
            has_validation += 1;
        }

        if need_drill {
            let include = match drill.as_str() {
                "truth" => !has_verified || !sym_owned,
                "change" => !sym_inv || !sym_vs,
                "workflow" => entries.is_empty() || !sym_ctx,
                "uncertainty" => !has_verified,
                _ => false,
            };
            if include {
                drill_rows.push(GapSymbol {
                    qname: sym.qname.clone(),
                    file: sym.file.clone(),
                    has_verified_effects: has_verified,
                    has_ownership: sym_owned,
                    has_invariant: sym_inv,
                    has_validation_scenario: sym_vs,
                    ledger_entries: entries.len(),
                    ctx_tagged: sym_ctx,
                });
            }
        }
    }

    let feedback_count = feedback_store
        .list_all(&ref_name)
        .map(|v| v.len())
        .unwrap_or(0);

    let details = Details {
        total_symbols,
        verified_effects: verified_count,
        owned_symbols: owned_count,
        invariant_symbols: has_invariant,
        validation_symbols: has_validation,
        feedback_entries: feedback_count,
        total_ledger_entries,
        ctx_tagged_ledger_entries: ctx_tagged_entries,
    };
    let scores = score(&details);

    let total = total_symbols as f64;
    let ledger_density = total_ledger_entries as f64 / total;
    let sparse_db = ledger_density < SPARSE_LEDGER_DENSITY;
    let with_ledger = scored_syms
        .iter()
        .filter(|s| ledger_by_sym.contains_key(&s.symbol_id))
        .count();

    let source_read_tokens: usize = file_max_line
        .values()
        .map(|&l| l as usize * TOKENS_PER_LINE)
        .sum();
    let reduction_pct = if source_read_tokens > 0 {
        (1.0 - structured_tokens as f64 / source_read_tokens as f64) * 100.0
    } else {
        0.0
    };
    let ratio_x = if structured_tokens > 0 {
        source_read_tokens as f64 / structured_tokens as f64
    } else {
        0.0
    };

    Scorecard {
        scores,
        data_quality: DataQuality {
            ledger_density,
            symbols_scored: total_symbols,
            symbols_with_any_ledger: with_ledger,
            coverage_pct: (with_ledger as f64 / total * 100.0).round(),
            sparse_db,
            note: if sparse_db {
                sparse_note(total_ledger_entries, total_symbols, ledger_density)
            } else {
                "ledger density is adequate".to_string()
            },
            scope: scoped.then_some(paths_filter),
        },
        details,
        token_economy: TokenEconomy {
            structured_tokens,
            source_read_tokens_est: source_read_tokens,
            reduction_pct: (reduction_pct * 10.0).round() / 10.0,
            ratio_x: (ratio_x * 10.0).round() / 10.0,
        },
        drill_down: need_drill.then(|| {
            let total_gaps = drill_rows.len();
            let shown: Vec<GapSymbol> = drill_rows.into_iter().take(opts.drill_limit).collect();
            DrillDown {
                dimension: drill,
                total_gaps,
                shown: shown.len(),
                omitted: total_gaps.saturating_sub(shown.len()),
                gap_symbols: shown,
            }
        }),
        matched_nothing: false,
        scoped,
    }
}

/// The sparse-ledger caveat. Identical wording across every surface, so it
/// lives with the threshold that triggers it rather than being retyped
/// alongside each renderer.
pub fn sparse_note(entries: usize, symbols: usize, density: f64) -> String {
    format!(
        "sparse ledger ({entries} entries across {symbols} symbols, \
         {density:.2} avg) — run 'asd sync' + 'asd hydrate' to populate; \
         scores reflect data density, not workflow quality"
    )
}

/// The formulas, isolated from the gathering so they can be tested against
/// hand-built metrics without an engine.
///
/// Directionality, since it is the thing most easily got backwards: every
/// dimension here runs *good-is-high*. That is the opposite of
/// `uncertainty.level` in `candidates`, where `low` means low uncertainty and
/// therefore high confidence. The `uncertainty` score below is an
/// index-health proxy — more verified effects and more symbols score higher —
/// not a measure of how uncertain anything is.
pub fn score(m: &Details) -> Scores {
    let total = m.total_symbols as f64;
    if m.total_symbols == 0 {
        return Scores {
            truth: 0,
            feedback: 0,
            change: 0,
            uncertainty: 0,
            workflow: 0,
            overall: 0,
        };
    }

    let truth = ((m.verified_effects as f64 / total + m.owned_symbols as f64 / total) / 2.0
        * 100.0)
        .min(100.0);
    let feedback = (m.feedback_entries as f64 / FEEDBACK_TARGET * 100.0).min(100.0);
    let change = ((m.invariant_symbols as f64 / total + m.validation_symbols as f64 / total) / 2.0
        * 100.0)
        .min(100.0);
    let uncertainty = {
        let effect_rate = m.verified_effects as f64 / total;
        let volume_score = (total / VOLUME_TARGET).min(1.0);
        ((effect_rate + volume_score) / 2.0 * 100.0).min(100.0)
    };
    let workflow = {
        let density = (m.total_ledger_entries as f64 / total / 2.0).min(1.0);
        let ctx_adoption = if m.total_ledger_entries == 0 {
            0.0
        } else {
            (m.ctx_tagged_ledger_entries as f64 / m.total_ledger_entries as f64).min(1.0)
        };
        ((density * 0.6 + ctx_adoption * 0.4) * 100.0).min(100.0)
    };
    let overall = (truth + feedback + change + uncertainty + workflow) / 5.0;

    Scores {
        truth: truth.round() as u64,
        feedback: feedback.round() as u64,
        change: change.round() as u64,
        uncertainty: uncertainty.round() as u64,
        workflow: workflow.round() as u64,
        overall: overall.round() as u64,
    }
}

/// One tree read for the whole ledger, superseded entries dropped, keyed by
/// symbol id.
fn load_ledger(engine: &Engine, ref_name: &str) -> HashMap<String, Vec<LedgerEntry>> {
    let tree = engine
        .repo
        .get_tree(ref_name, "/asd/v1/ledger")
        .unwrap_or(Value::Object(Default::default()));
    let mut map: HashMap<String, Vec<LedgerEntry>> = HashMap::new();
    if let Value::Object(by_symbol) = tree {
        for (sym_id, per_symbol) in by_symbol {
            if let Value::Object(entries_map) = per_symbol {
                let mut entries: Vec<LedgerEntry> = entries_map
                    .values()
                    .filter_map(|v| serde_json::from_value::<LedgerEntry>(v.clone()).ok())
                    .collect();
                entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                let superseded: HashSet<String> = entries
                    .iter()
                    .flat_map(|e| e.supersedes.iter().cloned())
                    .collect();
                entries.retain(|e| !superseded.contains(&e.entry_id));
                map.insert(sym_id, entries);
            }
        }
    }
    map
}

fn empty(scoped: bool, paths_filter: Vec<String>) -> Scorecard {
    let details = Details {
        total_symbols: 0,
        verified_effects: 0,
        owned_symbols: 0,
        invariant_symbols: 0,
        validation_symbols: 0,
        feedback_entries: 0,
        total_ledger_entries: 0,
        ctx_tagged_ledger_entries: 0,
    };
    Scorecard {
        scores: score(&details),
        data_quality: DataQuality {
            ledger_density: 0.0,
            symbols_scored: 0,
            symbols_with_any_ledger: 0,
            coverage_pct: 0.0,
            sparse_db: false,
            note: "no symbols to score".to_string(),
            scope: scoped.then_some(paths_filter),
        },
        details,
        token_economy: TokenEconomy {
            structured_tokens: 0,
            source_read_tokens_est: 0,
            reduction_pct: 0.0,
            ratio_x: 0.0,
        },
        drill_down: None,
        matched_nothing: true,
        scoped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(total: usize) -> Details {
        Details {
            total_symbols: total,
            verified_effects: 0,
            owned_symbols: 0,
            invariant_symbols: 0,
            validation_symbols: 0,
            feedback_entries: 0,
            total_ledger_entries: 0,
            ctx_tagged_ledger_entries: 0,
        }
    }

    #[test]
    fn an_empty_index_scores_zero_rather_than_dividing_by_it() {
        let s = score(&metrics(0));
        assert_eq!(s.overall, 0);
        assert_eq!(s.uncertainty, 0);
    }

    #[test]
    fn every_dimension_is_good_is_high() {
        // Guards the axis. `uncertainty` here is an index-health proxy, not a
        // measure of doubt: a fully verified, high-volume index must score
        // HIGH, the opposite direction from `uncertainty.level` elsewhere in
        // the codebase. See the note on `score`.
        let mut m = metrics(1000);
        m.verified_effects = 1000;
        m.owned_symbols = 1000;
        m.invariant_symbols = 1000;
        m.validation_symbols = 1000;
        m.feedback_entries = 50;
        m.total_ledger_entries = 4000;
        m.ctx_tagged_ledger_entries = 4000;

        let s = score(&m);
        assert_eq!(s.truth, 100);
        assert_eq!(s.change, 100);
        assert_eq!(s.feedback, 100);
        assert_eq!(s.uncertainty, 100, "a fully verified index is the GOOD end");
        assert_eq!(s.workflow, 100);
        assert_eq!(s.overall, 100);

        let bare = score(&metrics(1000));
        assert_eq!(bare.truth, 0);
        assert_eq!(bare.change, 0);
        assert_eq!(bare.workflow, 0);
        assert!(
            bare.uncertainty < s.uncertainty,
            "an unverified index must score LOWER, not higher"
        );
    }

    #[test]
    fn no_dimension_exceeds_one_hundred() {
        // Every formula is capped; a store with far more feedback or ledger
        // entries than the targets must not overflow the scale.
        let mut m = metrics(100_000);
        m.verified_effects = 100_000;
        m.owned_symbols = 100_000;
        m.feedback_entries = 100_000;
        m.total_ledger_entries = 1_000_000;
        m.ctx_tagged_ledger_entries = 1_000_000;
        m.invariant_symbols = 100_000;
        m.validation_symbols = 100_000;

        let s = score(&m);
        for (name, v) in [
            ("truth", s.truth),
            ("feedback", s.feedback),
            ("change", s.change),
            ("uncertainty", s.uncertainty),
            ("workflow", s.workflow),
            ("overall", s.overall),
        ] {
            assert!(v <= 100, "{name} = {v}");
        }
    }

    #[test]
    fn workflow_weights_density_above_ctx_adoption() {
        // 60/40 split: full density with no CTX tagging must beat full CTX
        // tagging on a thin ledger.
        let mut dense = metrics(100);
        dense.total_ledger_entries = 200;
        let mut tagged = metrics(100);
        tagged.total_ledger_entries = 1;
        tagged.ctx_tagged_ledger_entries = 1;

        assert!(score(&dense).workflow > score(&tagged).workflow);
    }

    #[test]
    fn scores_get_reaches_every_dimension() {
        let s = score(&metrics(0));
        for dim in [
            "truth",
            "feedback",
            "change",
            "uncertainty",
            "workflow",
            "overall",
        ] {
            assert!(s.get(dim).is_some(), "{dim} unreachable");
        }
        assert!(s.get("nonsense").is_none());
    }

    #[test]
    fn json_keeps_the_legacy_scores_alias() {
        // CLI trend snapshots on disk are keyed on `scores`; dropping the
        // alias would orphan every stored history file.
        let card = empty(false, vec![]);
        let v = card.to_json();
        assert_eq!(v["scores"], v["capability_scores"]);
        assert!(
            v["token_economy"]["note"]
                .as_str()
                .unwrap()
                .contains("estimate")
        );
    }
}
