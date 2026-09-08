//! How confident a Sharpe ratio is, given how little of it there is.
//!
//! A Sharpe ratio is an estimate, and every estimate has a standard error. Two
//! runs reporting 1.2 are not the same finding when one has forty returns and
//! the other four thousand, and nothing in [`Metrics`](crate::Metrics) said so:
//! the number was printed to two decimals either way.
//!
//! The Probabilistic Sharpe Ratio (Bailey and López de Prado) answers the
//! question the point estimate cannot — *what is the probability the true
//! Sharpe exceeds a threshold, given this many observations of this shape?*
//!
//! # Why this is not the deflation Arvo already has
//!
//! They correct different errors and both are needed.
//!
//! [`expected_best_under_null`](crate::expected_best_under_null) corrects for
//! **selection**: the best of nine configurations is high partly because nine
//! were tried. It needs the whole search, and it therefore only runs where
//! there was one.
//!
//! This corrects for **sample length and shape**: a Sharpe computed from a
//! short, skewed, fat-tailed return series is less certain than the same
//! number from a long, well-behaved one. It needs only the curve, so it runs
//! on every result — including the single studies and the benchmark runs that
//! deflation never sees.
//!
//! QuantConnect ship this on every backtest and say why they chose it over
//! deflation: it "could be self contained with a single analysis, as opposed
//! to other alternatives that required analysis of multiple backtests". They
//! are right that it is cheaper, and wrong that it is a substitute. Arvo now
//! has both, which is the point.
//!
//! # The unit trap
//!
//! The formula takes a **per-period** Sharpe and the number of periods. Arvo's
//! [`Metrics::sharpe`](crate::Metrics::sharpe) is annualised. Feeding the
//! annualised figure in would multiply the test statistic by roughly √252 on
//! daily bars and √98,280 on five-minute ones, reporting near-certainty for
//! everything. So nothing here reads `Metrics`: it is computed from the
//! returns directly, and the threshold is converted *into* per-period units
//! rather than the Sharpe being converted out of them.

/// The probability that the true Sharpe ratio exceeds `annual_threshold`.
///
/// `returns` are per-period simple returns; `periods_per_year` is what the
/// experiment's interval annualises by. The threshold is given annualised
/// because that is how anyone states one — "is this better than a Sharpe of
/// 1?" — and converted down internally.
///
/// Returns `None` when there is nothing to be confident about: fewer than two
/// returns, a series that never moved, or a variance term the shape drives
/// non-positive. A `None` is an absence of evidence and reads as one; a 0.5
/// would read as a coin flip anyone had measured.
#[must_use]
pub fn probabilistic_sharpe(
    returns: &[f64],
    periods_per_year: f64,
    annual_threshold: f64,
) -> Option<f64> {
    if returns.len() < 2 || periods_per_year <= 0.0 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "return counts are small")]
    let n = returns.len() as f64;

    let mean = returns.iter().sum::<f64>() / n;
    // Population moments, which is what the estimator is stated in terms of.
    // The sample/population distinction moves the third decimal here and the
    // formula's own derivation uses these.
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let sd = variance.sqrt();
    if sd <= 0.0 {
        return None;
    }

    let sharpe = mean / sd;
    // Down into per-period units. A Sharpe annualises by √periods, so a
    // threshold de-annualises by the same factor.
    let threshold = annual_threshold / periods_per_year.sqrt();

    let skew = returns.iter().map(|r| ((r - mean) / sd).powi(3)).sum::<f64>() / n;
    // Non-excess: 3.0 for a normal distribution. The formula subtracts one
    // from it, so passing excess kurtosis instead would shift the variance
    // term by exactly 3 and quietly overstate confidence on fat tails — which
    // are the case this exists to be careful about.
    let kurtosis = returns.iter().map(|r| ((r - mean) / sd).powi(4)).sum::<f64>() / n;

    // The estimator's own variance: how uncertain this Sharpe is, given the
    // shape of what produced it. Negative skew and fat tails both inflate it,
    // which is why a strategy that makes small gains and occasional large
    // losses is held to a harder standard than its point estimate suggests.
    let variance_term =
        1.0 - skew * sharpe + (kurtosis - 1.0) / 4.0 * sharpe.powi(2);
    if variance_term <= 0.0 {
        return None;
    }

    let statistic = (sharpe - threshold) * (n - 1.0).sqrt() / variance_term.sqrt();
    Some(normal_cdf(statistic))
}

/// Period returns of an equity curve, as the Sharpe is computed from.
///
/// Shared with [`crate::Metrics`] rather than re-derived, so the two cannot
/// disagree about what a return is.
#[must_use]
pub fn period_returns(curve: &[crate::EquityPoint]) -> Vec<f64> {
    curve
        .windows(2)
        .map(|pair| {
            if pair[0].equity == 0.0 {
                0.0
            } else {
                (pair[1].equity - pair[0].equity) / pair[0].equity
            }
        })
        .collect()
}

/// Standard normal cumulative distribution.
///
/// Via `libm::erf`, which is already in the build, rather than a hand-rolled
/// rational approximation — one fewer piece of numerics to be wrong about.
fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + libm::erf(x / std::f64::consts::SQRT_2))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A return series with a given per-period mean and spread, alternating so
    /// the mean and standard deviation are exactly what was asked for.
    fn returns(count: usize, mean: f64, spread: f64) -> Vec<f64> {
        (0..count)
            .map(|i| if i % 2 == 0 { mean + spread } else { mean - spread })
            .collect()
    }

    #[test]
    fn more_of_the_same_edge_is_more_confidence() {
        // The whole point. Two runs reporting the same Sharpe are not the same
        // finding when one has ten times the observations.
        let short = probabilistic_sharpe(&returns(20, 0.001, 0.01), 252.0, 0.0)
            .expect("a moving series has a Sharpe");
        let long = probabilistic_sharpe(&returns(200, 0.001, 0.01), 252.0, 0.0)
            .expect("a moving series has a Sharpe");
        assert!(long > short, "short {short:.4} long {long:.4}");
    }

    #[test]
    fn the_threshold_is_annualised_going_in() {
        // The unit trap. A threshold stated annually must be compared against
        // an annualised Sharpe, and this series has one of about 1.6 — so a
        // bar of 1.0 should still be probably cleared and a bar of 5.0 should
        // not.
        let series = returns(500, 0.001, 0.01);
        let over_one = probabilistic_sharpe(&series, 252.0, 1.0).expect("moves");
        let over_five = probabilistic_sharpe(&series, 252.0, 5.0).expect("moves");
        assert!(over_one > 0.5, "{over_one:.4}");
        assert!(over_five < 0.5, "{over_five:.4}");
        assert!(over_one > over_five);
    }

    #[test]
    fn a_higher_bar_is_never_easier_to_clear() {
        let series = returns(300, 0.001, 0.01);
        let mut previous = 1.1;
        for bar in [0.0, 0.5, 1.0, 1.5, 2.0] {
            let p = probabilistic_sharpe(&series, 252.0, bar).expect("moves");
            assert!(p <= previous, "bar {bar} gave {p:.4} against {previous:.4}");
            previous = p;
        }
    }

    #[test]
    fn negative_skew_costs_confidence() {
        // Many small gains and one large loss is the shape that flatters a
        // point estimate most, so it is the shape this has to be hardest on.
        let mut steady = vec![0.001_f64; 100];
        let mut skewed = steady.clone();
        // Same mean, same variance, opposite third moment.
        steady[0] = 0.05;
        steady[1] = -0.048;
        skewed[0] = -0.05;
        skewed[1] = 0.048;

        let a = probabilistic_sharpe(&steady, 252.0, 0.0).expect("moves");
        let b = probabilistic_sharpe(&skewed, 252.0, 0.0).expect("moves");
        assert!(b < a, "positively skewed {a:.6} should beat negative {b:.6}");
    }

    #[test]
    fn a_curve_that_never_moved_reports_absence_rather_than_a_coin_flip() {
        // 0.5 would read as "measured, and it is even odds". There is no
        // measurement here at all.
        assert_eq!(probabilistic_sharpe(&[0.0; 50], 252.0, 0.0), None);
        assert_eq!(probabilistic_sharpe(&[], 252.0, 0.0), None);
        assert_eq!(probabilistic_sharpe(&[0.01], 252.0, 0.0), None);
    }

    #[test]
    fn a_probability_stays_a_probability() {
        for count in [3usize, 10, 100, 1000] {
            for mean in [-0.01, 0.0, 0.001, 0.05] {
                let p = probabilistic_sharpe(&returns(count, mean, 0.01), 252.0, 0.0)
                    .expect("moves");
                assert!((0.0..=1.0).contains(&p), "{count} {mean} gave {p}");
            }
        }
    }

    #[test]
    fn the_normal_cdf_agrees_with_the_table() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-12);
        assert!((normal_cdf(1.0) - 0.841_344_746).abs() < 1e-9);
        assert!((normal_cdf(-1.96) - 0.025).abs() < 1e-4);
        assert!((normal_cdf(2.326) - 0.99).abs() < 1e-4);
    }

    #[test]
    fn returns_come_from_the_curve_the_same_way_metrics_reads_it() {
        let at = |d: u32| {
            chrono::NaiveDate::from_ymd_opt(2024, 1, d)
                .expect("valid")
                .and_time(chrono::NaiveTime::MIN)
        };
        let curve: Vec<crate::EquityPoint> = [100.0, 110.0, 99.0]
            .iter()
            .enumerate()
            .map(|(i, equity)| crate::EquityPoint {
                at: at(u32::try_from(i).expect("small") + 1),
                equity: *equity,
            })
            .collect();

        let got = period_returns(&curve);
        assert!((got[0] - 0.1).abs() < 1e-12, "{got:?}");
        assert!((got[1] + 0.1).abs() < 1e-12, "{got:?}");
    }
}
