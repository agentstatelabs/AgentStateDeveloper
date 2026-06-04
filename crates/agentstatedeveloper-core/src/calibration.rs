//! Plan J t-015: confidence-bucket calibration helpers.
//!
//! Calibration asks: when our predictor (uncertainty.level,
//! recovery estimate, result_bucket) says "medium", how often is
//! the underlying result actually correct?
//!
//! A well-calibrated predictor's pass rate within a bucket should
//! roughly match the bucket's nominal confidence. If `medium` has
//! a 95% pass rate, it should probably be relabeled `high`. If
//! `high` has a 60% pass rate, the threshold is too generous.
//!
//! This module is pure (no I/O, no async, no clock). It takes a
//! flat list of `(predicted_bucket, passed)` observations and
//! computes per-bucket statistics. The probe harness collects the
//! observations and feeds them in; the kernel here is the same
//! regardless of where observations come from (probe-harness
//! results, golden-set scoring, A/B test outcomes).

use std::collections::BTreeMap;

/// Per-bucket calibration statistics.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BucketStats {
    /// Bucket label (e.g. "low", "medium", "high", "critical").
    pub bucket: String,
    /// Total observations assigned to this bucket.
    pub count: usize,
    /// Observations that passed (predictor was right).
    pub passed: usize,
    /// Pass rate as a fraction in [0.0, 1.0]. NaN when count == 0
    /// (guard before use). Rounded to 4 decimal places.
    pub pass_rate: f64,
    /// Plain-English advisory drawn from the gap between bucket
    /// label semantics and observed pass rate. Empty string when
    /// the bucket appears well-calibrated.
    pub advice: String,
    /// Why `advice` looks the way it does. Field-eval refinement
    /// (1.0.73 ExampleProj run): an empty `advice` string had three
    /// possible meanings — well-calibrated, sample-too-small,
    /// or unknown-label — and consumers couldn't tell which from
    /// the JSON alone. This explicit status lets downstream tools
    /// gate alerting / dashboards without re-deriving the
    /// classification from `count` + `bucket` semantics.
    pub advice_status: AdviceStatus,
}

/// Why a bucket's `advice` field has the value it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdviceStatus {
    /// Sample large enough AND observed pass rate within ±15pp of
    /// the bucket's expected midpoint. `advice` is empty.
    WellCalibrated,
    /// `count < 5`. Statistical noise too high to act on; `advice`
    /// is suppressed even if the rate looks off.
    SampleTooSmall,
    /// Bucket label is not in the known-semantics table (uncertainty
    /// or quality axis). Can't judge calibration; `advice` is empty.
    UnknownLabel,
    /// `count ≥ 5` AND observed rate diverges from expected by >15pp.
    /// `advice` contains the multi-cause analysis; this is the only
    /// status that means "you should look at the bucket label vs.
    /// the predictor's thresholds."
    Actionable,
}

/// Aggregate calibration across all buckets.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CalibrationReport {
    /// Per-bucket stats, sorted by bucket label for stable output.
    pub buckets: Vec<BucketStats>,
    /// Total observations across all buckets.
    pub total: usize,
    /// Overall pass rate (sum_passed / total). NaN when total == 0.
    pub overall_pass_rate: f64,
}

/// Compute calibration from a stream of `(bucket_label, passed)`
/// observations. Buckets are grouped by exact-string label match
/// (case-sensitive — caller should normalize first if needed).
pub fn compute_calibration<S, I>(observations: I) -> CalibrationReport
where
    S: AsRef<str>,
    I: IntoIterator<Item = (S, bool)>,
{
    let mut totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut grand_total = 0usize;
    let mut grand_passed = 0usize;

    for (bucket, passed) in observations {
        let entry = totals.entry(bucket.as_ref().to_string()).or_insert((0, 0));
        entry.0 += 1;
        if passed {
            entry.1 += 1;
            grand_passed += 1;
        }
        grand_total += 1;
    }

    let buckets: Vec<BucketStats> = totals
        .into_iter()
        .map(|(label, (count, passed))| {
            let rate = if count == 0 {
                f64::NAN
            } else {
                round4(passed as f64 / count as f64)
            };
            let (advice, advice_status) = bucket_advice_with_status(&label, count, rate);
            BucketStats {
                bucket: label,
                count,
                passed,
                pass_rate: rate,
                advice,
                advice_status,
            }
        })
        .collect();

    let overall = if grand_total == 0 {
        f64::NAN
    } else {
        round4(grand_passed as f64 / grand_total as f64)
    };

    CalibrationReport {
        buckets,
        total: grand_total,
        overall_pass_rate: overall,
    }
}

/// Bucket-semantics advisory. ASD uses TWO bucket axes with
/// OPPOSITE directions — getting this wrong was the original
/// 1.0.59-through-1.0.66 calibration bug (ExampleProj's `low`
/// bucket at 100% pass rate looked miscalibrated only because
/// this table had the uncertainty axis inverted).
///
/// **Uncertainty axis** (from `compute_uncertainty` →
/// `uncertainty.level`): `low` means "low uncertainty / HIGH
/// confidence", `critical` means "highest uncertainty / LOW
/// confidence". A `low` bucket at 95% pass rate is correctly
/// calibrated; a `critical` bucket at 95% would be miscalibrated
/// (results are better than the predictor warned).
///
/// **Quality axis** (from `result_bucket` and recovery
/// estimation): `core` / `strong` mean "high-quality result",
/// `noisy` / `weak` mean "low-quality result". Direction
/// matches intuition: high-quality buckets should pass at high
/// rates, low-quality at low rates.
///
/// Reading the table below: `expected` is the OBSERVED pass
/// rate we'd expect if the bucket label accurately describes
/// the underlying signal. Advice fires when observed deviates
/// from that by >15pp and sample size is ≥5.
fn bucket_advice_with_status(
    label: &str,
    count: usize,
    rate: f64,
) -> (String, AdviceStatus) {
    if count < 5 {
        return (String::new(), AdviceStatus::SampleTooSmall);
    }
    if rate.is_nan() {
        return (String::new(), AdviceStatus::SampleTooSmall);
    }
    let expected = match label {
        // Uncertainty axis — low uncertainty = high confidence
        // = expect high pass rate.
        "low" => 0.95,
        "medium" => 0.70,
        "high" => 0.45,
        "critical" => 0.20,

        // Quality axis — high quality = expect high pass rate.
        "core" | "strong" | "relevant" => 0.90,
        "partial" | "peripheral" => 0.65,
        "weak" | "noisy" => 0.25,

        _ => return (String::new(), AdviceStatus::UnknownLabel),
    };
    let gap = rate - expected;
    if gap > 0.15 {
        // Field-eval (2026-06-04, ExampleProj 1.0.65): first real run
        // showed 7 `low` observations at 100% pass rate. The original
        // wording asserted "bucket threshold is too strict" — but
        // that's only one of three possible explanations. The probes
        // themselves may be too lenient (e.g. `qname_rank_lte
        // max_rank=5` passes if the right symbol is anywhere in the
        // top 5, not just at rank 1), or the predictor may have
        // correctly classified queries that any reasonable retrieval
        // would handle — `low` doesn't necessarily mean "expected to
        // fail," it means "model has uncertainty about which exact
        // candidate." Don't assert which one until precision-mode
        // probes (Plan J t-019) are in place to distinguish.
        let msg = format!(
            "observed pass rate {:.0}% exceeds expected midpoint {:.0}% by {:.0}pp — possible causes: (a) bucket threshold too strict, (b) probes too lenient to differentiate within-bucket precision, (c) bucket label genuinely describes uncertainty rather than expected failure rate. Tighten probes (rank_eq vs rank_lte) before retuning thresholds.",
            rate * 100.0,
            expected * 100.0,
            gap.abs() * 100.0,
        );
        (msg, AdviceStatus::Actionable)
    } else if gap < -0.15 {
        // The under-performing case is the clearer signal: probes
        // are failing, predictor said the result was high-confidence.
        // Leniency doesn't explain failures, so this advice can stay
        // direct.
        let msg = format!(
            "observed pass rate {:.0}% trails expected midpoint {:.0}% by {:.0}pp — bucket threshold may be too generous (results are worse than the label promises)",
            rate * 100.0,
            expected * 100.0,
            gap.abs() * 100.0,
        );
        (msg, AdviceStatus::Actionable)
    } else {
        (String::new(), AdviceStatus::WellCalibrated)
    }
}

fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_observations_yield_empty_report() {
        let report = compute_calibration::<&str, _>(Vec::new());
        assert_eq!(report.total, 0);
        assert!(report.overall_pass_rate.is_nan());
        assert!(report.buckets.is_empty());
    }

    #[test]
    fn single_bucket_all_pass_reports_1_0() {
        let obs = vec![("high", true), ("high", true), ("high", true)];
        let r = compute_calibration(obs);
        assert_eq!(r.total, 3);
        assert_eq!(r.buckets.len(), 1);
        assert_eq!(r.buckets[0].count, 3);
        assert_eq!(r.buckets[0].passed, 3);
        assert!((r.buckets[0].pass_rate - 1.0).abs() < 1e-9);
        assert!((r.overall_pass_rate - 1.0).abs() < 1e-9);
    }

    #[test]
    fn multi_bucket_groups_correctly() {
        let obs = vec![
            ("high", true), ("high", true), ("high", false),
            ("low", false), ("low", false),
            ("medium", true),
        ];
        let r = compute_calibration(obs);
        assert_eq!(r.total, 6);
        // BTreeMap sorts alphabetically: high, low, medium
        assert_eq!(r.buckets[0].bucket, "high");
        assert_eq!(r.buckets[0].count, 3);
        assert_eq!(r.buckets[0].passed, 2);
        assert!((r.buckets[0].pass_rate - 0.6667).abs() < 0.001);
        assert_eq!(r.buckets[1].bucket, "low");
        assert_eq!(r.buckets[1].count, 2);
        assert_eq!(r.buckets[1].passed, 0);
        assert_eq!(r.buckets[1].pass_rate, 0.0);
        assert_eq!(r.buckets[2].bucket, "medium");
    }

    #[test]
    fn small_sample_suppresses_advice() {
        // 4 obs is below the n=5 threshold; advice empty even if
        // the rate looks miscalibrated.
        let obs = vec![("high", false); 4];
        let r = compute_calibration(obs);
        assert_eq!(r.buckets[0].advice, "");
    }

    #[test]
    fn low_bucket_underperforming_gets_too_generous_advice() {
        // Uncertainty axis: `low` means LOW UNCERTAINTY = high
        // confidence → expect ~95% pass rate. 10 obs at 30% pass
        // rate is 65pp below — predictor said "high confidence,
        // results trustworthy" but probes failed → bucket
        // threshold was too generous (predictor admitted
        // high-confidence too easily).
        let mut obs: Vec<(&str, bool)> = Vec::new();
        for i in 0..10 {
            obs.push(("low", i < 3)); // 3 pass, 7 fail
        }
        let r = compute_calibration(obs);
        let advice = &r.buckets[0].advice;
        assert!(
            advice.contains("too generous"),
            "expected 'too generous' advice for low@30% (uncertainty axis); got: {advice:?}"
        );
        assert!(advice.contains("trails"), "got: {advice}");
    }

    #[test]
    fn high_bucket_overperforming_advice_lists_competing_causes() {
        // Uncertainty axis: `high` means HIGH UNCERTAINTY → expect
        // ~45% pass rate. 10 obs at 95% pass rate is 50pp above —
        // predictor warned low-confidence but probes passed →
        // either bucket is too strict, probes too lenient, or
        // "high uncertainty" describes scope-of-doubt rather than
        // expected-failure-rate. The multi-cause advisory shape
        // (added in 1.0.66, semantics direction corrected in
        // 1.0.68) is pinned by checking for "(a)/(b)/(c)" and
        // "Tighten probes" anchors.
        let mut obs: Vec<(&str, bool)> = Vec::new();
        for i in 0..10 {
            obs.push(("high", i != 0)); // 9 pass, 1 fail
        }
        let r = compute_calibration(obs);
        let advice = &r.buckets[0].advice;
        assert!(
            advice.contains("exceeds"),
            "must report the over-performance; got: {advice:?}"
        );
        assert!(
            advice.contains("(a)") && advice.contains("(b)") && advice.contains("(c)"),
            "must enumerate three competing causes; got: {advice:?}"
        );
        assert!(
            advice.contains("Tighten probes"),
            "must suggest the precision-mode remediation before threshold tuning; got: {advice:?}"
        );
    }

    #[test]
    fn exampleproj_field_signal_now_well_calibrated() {
        // Regression test for the ExampleProj 1.0.65/66/67 case: 10
        // observations in the `low` bucket all passing. With the
        // semantics-inverted advice table (1.0.59-1.0.67), this
        // produced a confidently-wrong "threshold too strict"
        // advisory. With the corrected semantics (1.0.68), `low`
        // expects ~95% pass rate, so 100% observed is +5pp —
        // well within the ±15pp band → advice empty (correctly
        // calibrated).
        let obs: Vec<(&str, bool)> = (0..10).map(|_| ("low", true)).collect();
        let r = compute_calibration(obs);
        assert_eq!(
            r.buckets[0].advice, "",
            "low@100% must be well-calibrated under uncertainty-axis semantics"
        );
    }

    #[test]
    fn well_calibrated_bucket_emits_no_advice() {
        // 10 obs of "medium" at ~70% pass — within the ±15pp band
        // around the 65% midpoint. Advice must be empty.
        let mut obs: Vec<(&str, bool)> = Vec::new();
        for i in 0..10 {
            obs.push(("medium", i < 7));
        }
        let r = compute_calibration(obs);
        assert_eq!(
            r.buckets[0].advice, "",
            "well-calibrated bucket should have empty advice"
        );
        assert_eq!(
            r.buckets[0].advice_status,
            AdviceStatus::WellCalibrated,
            "well-calibrated bucket must report status explicitly"
        );
    }

    #[test]
    fn advice_status_distinguishes_three_empty_causes() {
        // Refinement (1.0.73): an empty `advice` string had three
        // possible meanings — well-calibrated, sample-too-small,
        // unknown-label. The advice_status field disambiguates so
        // consumers don't have to re-derive.

        // SampleTooSmall: low bucket, only 4 observations (below
        // n=5 threshold), all passing.
        let small: Vec<(&str, bool)> = (0..4).map(|_| ("low", true)).collect();
        let r1 = compute_calibration(small);
        assert_eq!(r1.buckets[0].advice, "");
        assert_eq!(r1.buckets[0].advice_status, AdviceStatus::SampleTooSmall);

        // UnknownLabel: bucket name not in the semantics table.
        let unknown: Vec<(&str, bool)> = (0..10).map(|_| ("freshly_invented", true)).collect();
        let r2 = compute_calibration(unknown);
        assert_eq!(r2.buckets[0].advice, "");
        assert_eq!(r2.buckets[0].advice_status, AdviceStatus::UnknownLabel);

        // WellCalibrated: low bucket at 100% pass (expected ~95%,
        // gap +5pp within ±15pp band).
        let well: Vec<(&str, bool)> = (0..10).map(|_| ("low", true)).collect();
        let r3 = compute_calibration(well);
        assert_eq!(r3.buckets[0].advice, "");
        assert_eq!(r3.buckets[0].advice_status, AdviceStatus::WellCalibrated);
    }

    #[test]
    fn actionable_advice_reports_actionable_status() {
        // Mirror: when advice IS populated, status is Actionable.
        // 10 obs of "high" at 95% pass — expected ~45%, +50pp gap.
        let obs: Vec<(&str, bool)> = (0..10).map(|i| ("high", i != 0)).collect();
        let r = compute_calibration(obs);
        assert!(!r.buckets[0].advice.is_empty());
        assert_eq!(r.buckets[0].advice_status, AdviceStatus::Actionable);
    }

    #[test]
    fn unknown_bucket_label_emits_no_advice() {
        // A label not in the bucket-semantics table → can't judge
        // calibration; advice empty regardless of pass rate.
        let obs = vec![("freshly_invented_label", false); 10];
        let r = compute_calibration(obs);
        assert_eq!(r.buckets[0].advice, "");
    }

    #[test]
    fn recovery_estimate_labels_work() {
        // Smoke test: strong/partial/weak (from
        // recovery_suggestions) classify under the same semantics
        // as low/medium/high.
        let obs = vec![("strong", true); 8];
        let r = compute_calibration(obs);
        // 100% pass for "strong" (expected ~87.5%) → +12.5pp,
        // within the 15pp band → no advice (well-calibrated).
        assert_eq!(r.buckets[0].advice, "");
    }

    #[test]
    fn overall_rate_aggregates_across_buckets() {
        let obs = vec![
            ("high", true), ("high", true),
            ("low", false), ("low", false),
        ];
        let r = compute_calibration(obs);
        // 2 pass / 4 total = 0.5
        assert!((r.overall_pass_rate - 0.5).abs() < 1e-9);
    }
}
