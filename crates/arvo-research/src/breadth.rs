//! Whether a panel's N results are N findings.
//!
//! The panel runs one configuration across several instruments and pools what
//! comes back: a mean excess return, a count of how many beat their benchmark.
//! Both of those read as evidence in proportion to the number of instruments —
//! three that agree feel like three times the confidence of one.
//!
//! They are not, if the three moved together. A rule that buys every US large
//! cap on the same signal, tested on three US large caps, has been tested
//! roughly once. The pooled average is then a single observation wearing a
//! sample size, which is the most flattering shape a result can take and the
//! one this crate exists to refuse.
//!
//! # What is measured, and what it is not
//!
//! The correlation is between the *strategy's* returns on each instrument, not
//! between the instruments' prices. That is the right quantity — what matters
//! is whether the results are independent evidence, not whether the underlying
//! assets are — and it is worth saying because the two can differ sharply. Two
//! genuinely unrelated instruments produce correlated strategy returns if the
//! rule only trades in one kind of market and they both had it.
//!
//! # The number this produces is a scale, not a threshold
//!
//! Effective breadth is `N / (1 + (N-1)ρ̄)`: the standard count of independent
//! bets behind a set of correlated ones. It is exactly N when they are
//! uncorrelated and exactly 1 when they move identically. It assumes equal
//! weights and a single average correlation, neither of which is quite true,
//! so it is reported to one decimal and never used as a pass/fail gate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::EquityPoint;

/// The fewest overlapping periods a correlation is worth computing from.
///
/// Two returns produce a correlation of exactly ±1 and mean nothing. Twenty is
/// still small; below it the number is noise wearing a decimal point.
const MIN_OVERLAP: usize = 20;

/// How much of a panel's apparent breadth is real.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breadth {
    /// The instruments, in the order the matrix rows and columns follow.
    pub instruments: Vec<String>,
    /// Pairwise correlation of strategy returns. Square, symmetric, ones on
    /// the diagonal, and `None` for a pair with too little overlap to say.
    pub correlations: Vec<Vec<Option<f64>>>,
    /// Mean of the off-diagonal entries that could be computed.
    pub mean_correlation: Option<f64>,
    /// `N / (1 + (N-1)ρ̄)` — how many independent instruments this panel
    /// behaves like. `None` when there is nothing to average.
    pub effective: Option<f64>,
}

impl Breadth {
    /// How much the pooled statistics overstate their own sample size.
    ///
    /// The standard error of a mean over `effective` independent observations
    /// is larger than over `N` by `sqrt(N / effective)`. Below about 1.2 this
    /// is not worth mentioning; above 1.5 the pooled average is meaningfully
    /// weaker than its instrument count suggests.
    #[must_use]
    pub fn overstatement(&self) -> Option<f64> {
        let effective = self.effective?;
        if effective <= 0.0 {
            return None;
        }
        #[expect(clippy::cast_precision_loss, reason = "panel sizes are small")]
        let count = self.instruments.len() as f64;
        Some((count / effective).sqrt())
    }
}

/// Measures how much the members of a panel moved together.
///
/// Takes each instrument's out-of-sample equity curve. Returns are derived
/// here rather than passed in, so there is one definition of what a period
/// return is and it is the same one the metrics use.
#[must_use]
pub fn measure(curves: &[(String, Vec<EquityPoint>)]) -> Breadth {
    let series: Vec<BTreeMap<chrono::NaiveDateTime, f64>> =
        curves.iter().map(|(_, curve)| returns(curve)).collect();
    let instruments: Vec<String> = curves.iter().map(|(name, _)| name.clone()).collect();

    let mut correlations = vec![vec![None; instruments.len()]; instruments.len()];
    let mut off_diagonal = Vec::new();

    for (row, left) in series.iter().enumerate() {
        for (column, right) in series.iter().enumerate() {
            if row == column {
                correlations[row][column] = Some(1.0);
                continue;
            }
            // Only where both were measured. Instruments can cover different
            // spans even inside one window — a listing that starts late, a
            // file that ends early — and pairing a return against a gap would
            // be correlating one series with an assumption.
            let paired: Vec<(f64, f64)> = left
                .iter()
                .filter_map(|(at, value)| right.get(at).map(|other| (*value, *other)))
                .collect();
            correlations[row][column] = correlate(&paired);
            if row < column {
                if let Some(value) = correlations[row][column] {
                    off_diagonal.push(value);
                }
            }
        }
    }

    #[expect(clippy::cast_precision_loss, reason = "panel sizes are small")]
    let count = instruments.len() as f64;
    let mean = if off_diagonal.is_empty() {
        None
    } else {
        #[expect(clippy::cast_precision_loss, reason = "pair counts are small")]
        Some(off_diagonal.iter().sum::<f64>() / off_diagonal.len() as f64)
    };

    let effective = mean.and_then(|mean| {
        if count < 1.0 {
            return None;
        }
        // A strongly *negative* average correlation drives the denominator to
        // zero and the effective count to infinity, which is arithmetically
        // true of the formula and nonsense as a claim about evidence. Held at
        // the number of instruments: they cannot be more independent than they
        // are numerous.
        let denominator = 1.0 + (count - 1.0) * mean;
        if denominator <= 0.0 {
            return Some(count);
        }
        Some((count / denominator).min(count))
    });

    Breadth {
        instruments,
        correlations,
        mean_correlation: mean,
        effective,
    }
}

/// Period returns of an equity curve, by the instant each covers.
fn returns(curve: &[EquityPoint]) -> BTreeMap<chrono::NaiveDateTime, f64> {
    curve
        .windows(2)
        .filter(|pair| pair[0].equity != 0.0)
        .map(|pair| (pair[1].at, pair[1].equity / pair[0].equity - 1.0))
        .collect()
}

/// Pearson correlation, or `None` when the answer would not mean anything.
fn correlate(paired: &[(f64, f64)]) -> Option<f64> {
    if paired.len() < MIN_OVERLAP {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "series lengths are bounded")]
    let count = paired.len() as f64;

    let mean_left = paired.iter().map(|(left, _)| left).sum::<f64>() / count;
    let mean_right = paired.iter().map(|(_, right)| right).sum::<f64>() / count;

    let mut covariance = 0.0;
    let mut variance_left = 0.0;
    let mut variance_right = 0.0;
    for (left, right) in paired {
        let (left, right) = (left - mean_left, right - mean_right);
        covariance += left * right;
        variance_left += left * left;
        variance_right += right * right;
    }

    // A series that never moved has no correlation with anything. Reporting
    // zero would read as "independent", which is a claim; there is no claim to
    // make about a constant.
    let spread = (variance_left * variance_right).sqrt();
    if spread <= 0.0 {
        return None;
    }
    Some((covariance / spread).clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(name: &str, values: &[f64]) -> (String, Vec<EquityPoint>) {
        let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN);
        (
            name.to_owned(),
            values
                .iter()
                .enumerate()
                .map(|(index, equity)| EquityPoint {
                    at: start + chrono::Duration::days(index as i64),
                    equity: *equity,
                })
                .collect(),
        )
    }

    /// A curve whose returns follow a pattern, so two of them can be made to
    /// agree or disagree exactly.
    fn shaped(name: &str, sign: f64, length: usize) -> (String, Vec<EquityPoint>) {
        let mut equity = 100.0;
        let mut values = vec![equity];
        for index in 0..length {
            let step = if index % 3 == 0 { 0.02 } else { -0.01 };
            equity *= 1.0 + step * sign;
            values.push(equity);
        }
        curve(name, &values)
    }

    #[test]
    fn instruments_that_move_together_are_not_three_findings() {
        // The claim this module exists to make. A rule tested on three things
        // that behave identically has been tested roughly once, and the pooled
        // average is a single observation wearing a sample size.
        let panel = [shaped("A", 1.0, 60), shaped("B", 1.0, 60), shaped("C", 1.0, 60)];
        let breadth = measure(&panel);

        let mean = breadth.mean_correlation.expect("enough overlap");
        assert!((mean - 1.0).abs() < 1e-9, "identical series: {mean}");
        let effective = breadth.effective.expect("a mean to work from");
        assert!(
            (effective - 1.0).abs() < 1e-9,
            "three identical instruments behave like one: {effective}"
        );
    }

    #[test]
    fn uncorrelated_instruments_keep_their_full_breadth() {
        // The check has to be passable, or it says nothing. Two series whose
        // returns are unrelated should count as two.
        let a = curve(
            "A",
            &(0..80)
                .scan(100.0, |equity, index| {
                    *equity *= if index % 2 == 0 { 1.01 } else { 0.995 };
                    Some(*equity)
                })
                .collect::<Vec<_>>(),
        );
        let b = curve(
            "B",
            &(0..80)
                .scan(100.0, |equity, index| {
                    *equity *= if index % 7 < 3 { 1.008 } else { 0.997 };
                    Some(*equity)
                })
                .collect::<Vec<_>>(),
        );
        let breadth = measure(&[a, b]);
        let effective = breadth.effective.expect("a mean");
        assert!(
            effective > 1.5,
            "loosely related series keep most of their breadth: {effective} \
             (mean correlation {:?})",
            breadth.mean_correlation
        );
    }

    #[test]
    fn a_pair_with_too_little_overlap_makes_no_claim() {
        // Two returns produce a correlation of exactly ±1 and mean nothing.
        let panel = [shaped("A", 1.0, 5), shaped("B", -1.0, 5)];
        let breadth = measure(&panel);
        assert_eq!(breadth.correlations[0][1], None);
        assert_eq!(breadth.mean_correlation, None);
        assert_eq!(breadth.effective, None);
    }

    #[test]
    fn a_flat_curve_has_no_correlation_rather_than_a_zero_one() {
        // Reporting zero would read as "independent", which is a claim. There
        // is no claim to make about a constant.
        let flat = curve("FLAT", &vec![100.0; 60]);
        let moving = shaped("B", 1.0, 60);
        let breadth = measure(&[flat, moving]);
        assert_eq!(breadth.correlations[0][1], None);
    }

    #[test]
    fn the_diagonal_is_one_and_the_matrix_is_symmetric() {
        let panel = [shaped("A", 1.0, 60), shaped("B", -1.0, 60)];
        let breadth = measure(&panel);
        assert_eq!(breadth.correlations[0][0], Some(1.0));
        assert_eq!(breadth.correlations[1][1], Some(1.0));
        assert_eq!(breadth.correlations[0][1], breadth.correlations[1][0]);
    }

    #[test]
    fn perfectly_opposed_instruments_are_not_more_numerous_than_they_are() {
        // A strongly negative average drives the formula's denominator toward
        // zero and the effective count toward infinity — arithmetically true
        // and nonsense as a claim about evidence.
        let panel = [shaped("A", 1.0, 60), shaped("B", -1.0, 60)];
        let breadth = measure(&panel);
        let effective = breadth.effective.expect("a mean");
        assert!(
            effective <= 2.0 + 1e-9,
            "two instruments cannot be more than two: {effective}"
        );
    }

    #[test]
    fn overstatement_says_how_much_the_pooled_average_is_flattered() {
        // The standard error of a mean over `effective` observations is larger
        // than over N by sqrt(N / effective).
        let panel = [shaped("A", 1.0, 60), shaped("B", 1.0, 60), shaped("C", 1.0, 60)];
        let overstatement = measure(&panel).overstatement().expect("measurable");
        assert!(
            (overstatement - 3f64.sqrt()).abs() < 1e-9,
            "three identical instruments overstate by sqrt(3): {overstatement}"
        );
    }

    #[test]
    fn instruments_covering_different_spans_are_paired_only_where_both_ran() {
        // Pairing a return against a gap would be correlating one series with
        // an assumption.
        let long = shaped("LONG", 1.0, 80);
        let short = shaped("SHORT", 1.0, 30);
        let breadth = measure(&[long, short]);
        assert!(
            breadth.correlations[0][1].is_some(),
            "thirty overlapping periods is enough to measure"
        );
    }

    #[test]
    fn a_panel_of_one_has_no_breadth_to_measure() {
        let breadth = measure(&[shaped("A", 1.0, 60)]);
        assert_eq!(breadth.mean_correlation, None);
        assert_eq!(breadth.effective, None);
        assert_eq!(breadth.overstatement(), None);
    }
}
