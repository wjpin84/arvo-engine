//! Comparing one vendor's bars against another's.
//!
//! [`quality`](crate::quality) checks a series against *itself* — a low above
//! a high, a gap where a session should be, a range too wide to be real. Those
//! catch what is impossible. They cannot catch what is merely wrong: a close
//! that is off by forty cents is a perfectly well-formed bar, and six internal
//! checks will pass it every time.
//!
//! This is the same structural weakness the reconciliation invariants were
//! written for. A number checked against a restatement of itself catches
//! nothing; a number checked against an independently produced version of the
//! same fact catches a bad source. The only independent version of a price is
//! somebody else's.
//!
//! # Why the answer is a classification and not a count
//!
//! Two vendors disagreeing is the normal case, and most of the ways they
//! disagree are not faults:
//!
//! * **Different adjustment.** One series split-adjusted and the other raw
//!   differ on *every* bar by the same factor. That is not bad data on either
//!   side — it is a statement that they cannot be compared until one is
//!   restated, and it is the single most likely disagreement between any two
//!   equity sources.
//! * **Different session.** Extended hours included or not changes which bars
//!   exist and what the first and last of each day contain.
//! * **Different volume basis.** Consolidated tape against primary exchange
//!   routinely differs by a factor of three or more, on data whose prices
//!   agree exactly.
//!
//! Reporting "4,812 bars disagree" across any of those is true, useless, and
//! the kind of thing that gets a check switched off. So the disagreement is
//! classified first and counted second.

use crate::Bar;

/// How far two vendors' prices may differ and still be the same price.
///
/// Ten basis points. Vendors round to different precisions, take the last
/// print from different feeds, and settle ties differently; a tenth of a
/// percent absorbs all of that. It does not absorb a genuinely different
/// price, which is the point.
pub const PRICE_TOLERANCE_BPS: f64 = 10.0;

/// How close the per-bar ratios must sit to each other to call the difference
/// a rescaling rather than noise.
///
/// If one series is the other times a constant, every bar's ratio is that
/// constant. Real data never matches to the last bit, so the test is that the
/// spread of ratios is small relative to the ratio itself.
const RESCALE_SPREAD: f64 = 0.005;

/// A ratio this far from 1.0 is a rescaling worth reporting rather than
/// rounding.
const RESCALE_FLOOR: f64 = 0.001;

/// What comparing two sources found.
#[derive(Debug, Clone, PartialEq)]
pub enum Agreement {
    /// Nothing to compare: no instant appears in both.
    NoOverlap,
    /// Every shared bar agrees within tolerance.
    Aligned { compared: usize },
    /// One series is the other times a near-constant factor.
    ///
    /// Almost always an adjustment difference — one split-adjusted and one
    /// raw, or adjusted as of different dates. Neither source is wrong and
    /// they cannot be used together until one is restated, which is a
    /// different problem from bad data and has a different fix.
    Rescaled {
        /// What the second series must be multiplied by to match the first.
        factor: f64,
        compared: usize,
    },
    /// Shared bars that genuinely disagree.
    ///
    /// The finding this exists for: at least one source has prices nobody
    /// traded at, and no check against a single series could have said so.
    Diverged {
        /// Bars whose prices differ by more than the tolerance.
        disagreeing: usize,
        compared: usize,
        /// The largest single disagreement, as a fraction, and where.
        worst: f64,
        at: chrono::NaiveDateTime,
    },
}

/// Bars one source has and the other does not.
///
/// Reported beside [`Agreement`] rather than inside it, because a coverage
/// difference and a price difference are independent: two sources can cover
/// different sessions and agree perfectly wherever they overlap.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    pub only_first: usize,
    pub only_second: usize,
    pub shared: usize,
}

/// Compares two series of the same instrument over the same window.
///
/// Volume is deliberately not compared. Consolidated tape and primary-exchange
/// volume differ by a factor of three on data whose prices are identical, so a
/// volume check would fire on every honest pair and say nothing about whether
/// the prices can be trusted.
#[must_use]
pub fn compare(first: &[Bar], second: &[Bar]) -> (Agreement, Coverage) {
    let shared = shared_bars(first, second);
    let coverage = Coverage {
        only_first: first.len() - shared.len(),
        only_second: second.len() - shared.len(),
        shared: shared.len(),
    };

    if shared.is_empty() {
        return (Agreement::NoOverlap, coverage);
    }

    // Closes only, for the rescaling test. A split factor applies to every
    // price in the bar equally, so one field settles it and four would just be
    // the same evidence four times.
    let ratios: Vec<f64> = shared
        .iter()
        .filter(|(_, b)| b.close != 0.0)
        .map(|(a, b)| a.close / b.close)
        .collect();

    if let Some(factor) = constant_factor(&ratios) {
        return (
            Agreement::Rescaled {
                factor,
                compared: shared.len(),
            },
            coverage,
        );
    }

    let tolerance = PRICE_TOLERANCE_BPS / 10_000.0;
    let mut disagreeing = 0;
    let mut worst = 0.0;
    let mut worst_at = shared[0].0.at;

    for (a, b) in &shared {
        // The worst of the four, so a bar whose close matches but whose low is
        // wrong is still caught — a stop is triggered by the low, and a
        // strategy is as exposed to a bad extreme as to a bad close.
        let gap = [
            relative(a.open, b.open),
            relative(a.high, b.high),
            relative(a.low, b.low),
            relative(a.close, b.close),
        ]
        .into_iter()
        .fold(0.0_f64, f64::max);

        if gap > tolerance {
            disagreeing += 1;
        }
        if gap > worst {
            worst = gap;
            worst_at = a.at;
        }
    }

    let agreement = if disagreeing == 0 {
        Agreement::Aligned {
            compared: shared.len(),
        }
    } else {
        Agreement::Diverged {
            disagreeing,
            compared: shared.len(),
            worst,
            at: worst_at,
        }
    };
    (agreement, coverage)
}

/// Bars present in both series, paired by their opening instant.
fn shared_bars<'a>(first: &'a [Bar], second: &'a [Bar]) -> Vec<(&'a Bar, &'a Bar)> {
    let index: std::collections::HashMap<chrono::NaiveDateTime, &Bar> =
        second.iter().map(|bar| (bar.at, bar)).collect();
    first
        .iter()
        .filter_map(|bar| index.get(&bar.at).map(|other| (bar, *other)))
        .collect()
}

/// The constant the ratios sit on, if they sit on one.
///
/// `None` when they scatter — which is the interesting case, because scattered
/// ratios mean the two sources disagree about individual prices rather than
/// about how the whole series is scaled.
fn constant_factor(ratios: &[f64]) -> Option<f64> {
    if ratios.len() < 2 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "bar counts are small")]
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    if mean <= 0.0 {
        return None;
    }
    // Within rounding of 1.0 is not a rescaling, it is agreement.
    if (mean - 1.0).abs() < RESCALE_FLOOR {
        return None;
    }
    let spread = ratios
        .iter()
        .map(|ratio| (ratio - mean).abs())
        .fold(0.0_f64, f64::max);
    (spread / mean < RESCALE_SPREAD).then_some(mean)
}

/// Difference between two prices as a fraction of the first.
fn relative(a: f64, b: f64) -> f64 {
    let scale = a.abs().max(b.abs());
    if scale == 0.0 {
        return 0.0;
    }
    (a - b).abs() / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn bar(day: u32, close: f64) -> Bar {
        Bar {
            at: at(day),
            open: close,
            high: close * 1.01,
            low: close * 0.99,
            close,
            volume: 1_000.0,
        }
    }

    fn series(closes: &[(u32, f64)]) -> Vec<Bar> {
        closes.iter().map(|(day, close)| bar(*day, *close)).collect()
    }

    #[test]
    fn two_sources_that_agree_say_so_without_qualification() {
        let a = series(&[(1, 100.0), (2, 101.0), (3, 102.0)]);
        let (agreement, coverage) = compare(&a, &a.clone());
        assert_eq!(agreement, Agreement::Aligned { compared: 3 });
        assert_eq!(coverage.only_first, 0);
        assert_eq!(coverage.only_second, 0);
    }

    #[test]
    fn rounding_to_different_precisions_is_still_agreement() {
        // Vendors round differently and take the last print from different
        // feeds. A check that fires on a cent is a check nobody leaves on.
        let a = series(&[(1, 100.00), (2, 101.00), (3, 102.00)]);
        let b = series(&[(1, 100.01), (2, 100.99), (3, 102.01)]);
        let (agreement, _) = compare(&a, &b);
        assert!(
            matches!(agreement, Agreement::Aligned { .. }),
            "{agreement:?}"
        );
    }

    #[test]
    fn one_series_split_adjusted_and_one_raw_is_a_rescaling_not_a_fault() {
        // The single most likely disagreement between two equity sources, and
        // the one it would be most wrong to report as bad data. Neither side
        // has a price nobody traded at.
        let raw = series(&[(1, 400.0), (2, 404.0), (3, 396.0)]);
        let adjusted = series(&[(1, 100.0), (2, 101.0), (3, 99.0)]);
        let (agreement, _) = compare(&raw, &adjusted);
        let Agreement::Rescaled { factor, compared } = agreement else {
            panic!("a 4:1 split is a rescaling: {agreement:?}");
        };
        assert!((factor - 4.0).abs() < 1e-9, "{factor}");
        assert_eq!(compared, 3);
    }

    #[test]
    fn one_bad_price_among_good_ones_is_a_divergence_not_a_rescaling() {
        // The finding this exists for. The ratios scatter rather than sitting
        // on a constant, which is what separates a bad print from an
        // adjustment difference.
        let a = series(&[(1, 100.0), (2, 101.0), (3, 102.0), (4, 103.0)]);
        let b = series(&[(1, 100.0), (2, 101.0), (3, 89.0), (4, 103.0)]);
        let (agreement, _) = compare(&a, &b);
        let Agreement::Diverged {
            disagreeing,
            compared,
            worst,
            at: where_,
        } = agreement
        else {
            panic!("a single bad close is a divergence: {agreement:?}");
        };
        assert_eq!(disagreeing, 1);
        assert_eq!(compared, 4);
        assert_eq!(where_, at(3));
        assert!(worst > 0.1, "{worst}");
    }

    #[test]
    fn a_wrong_extreme_is_caught_even_when_the_close_agrees() {
        // A stop is triggered by the low, not the close. A source with good
        // closes and bad extremes would pass any close-only comparison and
        // would still stop a strategy out of positions it never lost.
        let mut a = series(&[(1, 100.0), (2, 100.0), (3, 100.0)]);
        let b = a.clone();
        a[1].low = 80.0;

        let (agreement, _) = compare(&a, &b);
        assert!(
            matches!(agreement, Agreement::Diverged { .. }),
            "{agreement:?}"
        );
    }

    #[test]
    fn different_volume_bases_are_not_a_disagreement_about_price() {
        // Consolidated tape against primary exchange differs by a factor of
        // three on data whose prices are identical.
        let a = series(&[(1, 100.0), (2, 101.0)]);
        let mut b = a.clone();
        for bar in &mut b {
            bar.volume *= 3.4;
        }
        let (agreement, _) = compare(&a, &b);
        assert!(
            matches!(agreement, Agreement::Aligned { .. }),
            "{agreement:?}"
        );
    }

    #[test]
    fn covering_different_days_is_reported_apart_from_the_prices() {
        // Two sources can cover different sessions and agree perfectly
        // wherever they overlap. Those are independent facts and collapsing
        // them would hide one behind the other.
        let a = series(&[(1, 100.0), (2, 101.0), (3, 102.0)]);
        let b = series(&[(2, 101.0), (3, 102.0), (4, 103.0)]);
        let (agreement, coverage) = compare(&a, &b);
        assert_eq!(agreement, Agreement::Aligned { compared: 2 });
        assert_eq!(coverage.shared, 2);
        assert_eq!(coverage.only_first, 1);
        assert_eq!(coverage.only_second, 1);
    }

    #[test]
    fn no_shared_instant_is_no_comparison_rather_than_a_clean_bill() {
        let a = series(&[(1, 100.0), (2, 101.0)]);
        let b = series(&[(5, 100.0), (6, 101.0)]);
        let (agreement, coverage) = compare(&a, &b);
        assert_eq!(agreement, Agreement::NoOverlap);
        assert_eq!(coverage.shared, 0);
    }

    #[test]
    fn a_single_shared_bar_cannot_establish_a_rescaling() {
        // One ratio is always exactly constant. Calling that a rescaling would
        // report every one-bar overlap as an adjustment difference.
        let a = series(&[(1, 400.0)]);
        let b = series(&[(1, 100.0)]);
        let (agreement, _) = compare(&a, &b);
        assert!(
            matches!(agreement, Agreement::Diverged { .. }),
            "one bar is not evidence of a factor: {agreement:?}"
        );
    }

    #[test]
    fn a_rescaling_that_does_not_hold_across_the_series_is_a_divergence() {
        // Half the series scaled and half not is not an adjustment — it is a
        // source that changed its mind, which is worse than either.
        let a = series(&[(1, 400.0), (2, 404.0), (3, 102.0), (4, 103.0)]);
        let b = series(&[(1, 100.0), (2, 101.0), (3, 102.0), (4, 103.0)]);
        let (agreement, _) = compare(&a, &b);
        assert!(
            matches!(agreement, Agreement::Diverged { .. }),
            "{agreement:?}"
        );
    }
}
