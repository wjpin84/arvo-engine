//! What kind of market a result was earned in.
//!
//! # The problem
//!
//! A rule that works in trends, judged across a window that was half trend and
//! half chop, gets one number describing neither. Strongly positive in one half
//! and strongly negative in the other nets to roughly zero, and `NotSupported`
//! is then a true statement about an average that nothing in the market ever
//! produced.
//!
//! Splitting the window by regime says which it was.
//!
//! # This is descriptive, and saying so is the whole safety argument
//!
//! The labels here are computed **after the fact, over the whole window**. That
//! is legitimate for *describing* a result and illegitimate for *taking* one.
//!
//! - Honest: "this rule made its money in the trending third and gave it back
//!   in the range — the pooled verdict is an average of two different answers."
//! - **Not** honest, and not something this module licenses: "so trade it in
//!   trends." Acting on that needs to know the regime *before* the bar, which
//!   is a real-time detector, a much harder thing, and one this codebase does
//!   not have. A backtest that filtered by a label computed from the whole
//!   window would be look-ahead of the most flattering kind.
//!
//! Every consumer is expected to carry that distinction. [`Breakdown::split`]
//! reports a disagreement; it never recommends acting on one.
//!
//! # Labelled from the benchmark curve
//!
//! Buy-and-hold's equity is cash plus shares times price, so its shape tracks
//! the instrument's own path — and it is already stored on every evaluation.
//! Labelling from it costs no new data dependency and no second notion of what
//! the market did.
//!
//! For a book the benchmark holds every member, so the regime is the *book's*,
//! not any one member's. That is the right answer for a result the book
//! produced, and it does mean a member that ranged while the book trended is
//! not visible here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::EquityPoint;

/// Above this efficiency ratio, the market is going somewhere.
///
/// The ratio is net movement over total movement across a lookback: a path that
/// goes straight up scores 1.0, and one that ends where it started scores 0.0.
///
/// 0.35 is a chosen line, not a discovered one. Kaufman's adaptive work uses
/// the same measure with thresholds in this region, and anything in 0.3–0.4
/// separates "went somewhere" from "wandered" on daily bars. It is a constant
/// here rather than a parameter because a threshold tuned per result is a
/// threshold that can be tuned until the answer is the one you wanted.
pub const TREND_THRESHOLD: f64 = 0.35;

/// How many periods the ratio looks back over.
///
/// Twenty — a trading month on daily bars. Short enough that a regime lasting a
/// season is not averaged away, long enough that a fortnight of noise does not
/// relabel the market twice a week.
pub const LOOKBACK: usize = 20;

/// What kind of market a period was.
///
/// Three, not five. Adding "high volatility" and "low volatility" doubles the
/// buckets and halves the observations in each, and the question this exists to
/// answer — did a directional rule earn its result directionally? — does not
/// need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    TrendingUp,
    TrendingDown,
    /// Went nowhere in particular. Not "flat" — a range can be violent.
    Ranging,
}

impl Regime {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::TrendingUp => "trending up",
            Self::TrendingDown => "trending down",
            Self::Ranging => "ranging",
        }
    }
}

/// How a run did in one kind of market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeOutcome {
    pub regime: Regime,
    /// Periods labelled this way.
    pub periods: usize,
    /// Share of the labelled window, as a fraction.
    pub share: f64,
    /// Compounded return over this regime's periods alone.
    pub strategy_return: f64,
    pub benchmark_return: f64,
    /// Strategy minus benchmark, over the same periods.
    pub excess_return: f64,
}

/// A result split by the kind of market that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breakdown {
    /// One per regime that actually occurred, in a stable order.
    pub outcomes: Vec<RegimeOutcome>,
    /// Periods before the lookback filled, which carry no label.
    ///
    /// Reported rather than folded into the first regime: they are part of the
    /// run's return and no part of any regime's, and hiding that would make the
    /// regime returns look as though they sum to the whole.
    pub unlabelled: usize,
}

impl Breakdown {
    /// The regimes where the rule beat its benchmark and where it did not,
    /// when both happened and the gap is worth reporting.
    ///
    /// `None` when the result does not disagree with itself — one regime only,
    /// or the same sign everywhere, or a difference too small to describe. That
    /// is the ordinary case and it deserves silence rather than a line saying
    /// nothing happened.
    ///
    /// **A disagreement is a caveat on the verdict, never a trading rule.** See
    /// the module note: knowing which regime you are in requires knowing it in
    /// advance.
    #[must_use]
    pub fn split(&self) -> Option<(&RegimeOutcome, &RegimeOutcome)> {
        let best = self
            .outcomes
            .iter()
            .filter(|outcome| outcome.periods >= MIN_PERIODS_TO_DESCRIBE)
            .max_by(|a, b| a.excess_return.total_cmp(&b.excess_return))?;
        let worst = self
            .outcomes
            .iter()
            .filter(|outcome| outcome.periods >= MIN_PERIODS_TO_DESCRIBE)
            .min_by(|a, b| a.excess_return.total_cmp(&b.excess_return))?;

        // Both directions must actually have happened, and the gap has to be
        // big enough that a reader would act differently for knowing it.
        (best.regime != worst.regime
            && best.excess_return > 0.0
            && worst.excess_return < 0.0
            && best.excess_return - worst.excess_return >= MATERIAL_GAP)
            .then_some((best, worst))
    }
}

/// Below this many periods, a regime's return describes those few periods
/// rather than the regime.
///
/// Ten, matching the trade-count bar `advice` uses for the same reason: a
/// statistic over a handful of observations is a statistic about the handful.
pub const MIN_PERIODS_TO_DESCRIBE: usize = 10;

/// How far apart two regimes' excess returns must be before the difference is
/// worth a line. Five percentage points.
pub const MATERIAL_GAP: f64 = 0.05;

/// Labels each period of a curve, `None` until the lookback has filled.
///
/// The ratio at period *i* uses periods *i − lookback* through *i* and nothing
/// after, so a label never depends on the future **relative to its own point**.
/// That does not make the breakdown safe to select on — the whole series is
/// still computed after the run — and the module note says why.
#[must_use]
pub fn label(curve: &[EquityPoint], lookback: usize) -> Vec<Option<Regime>> {
    let lookback = lookback.max(2);
    let mut out = vec![None; curve.len()];

    for index in lookback..curve.len() {
        let window = &curve[index - lookback..=index];
        let net = window[window.len() - 1].equity - window[0].equity;
        let travelled: f64 = window
            .windows(2)
            .map(|pair| (pair[1].equity - pair[0].equity).abs())
            .sum();

        // A window that never moved is not trending, whatever the ratio would
        // divide by.
        if travelled <= 0.0 {
            out[index] = Some(Regime::Ranging);
            continue;
        }

        let efficiency = net.abs() / travelled;
        out[index] = Some(if efficiency < TREND_THRESHOLD {
            Regime::Ranging
        } else if net > 0.0 {
            Regime::TrendingUp
        } else {
            Regime::TrendingDown
        });
    }
    out
}

/// Splits a result by the kind of market each period was.
///
/// Returns `None` when the two curves cannot be compared period for period —
/// different lengths mean they describe different runs, and attributing one
/// curve's returns to the other's regimes would be silently wrong.
#[must_use]
pub fn attribute(strategy: &[EquityPoint], benchmark: &[EquityPoint]) -> Option<Breakdown> {
    if strategy.len() != benchmark.len() || strategy.len() < 2 {
        return None;
    }

    let labels = label(benchmark, LOOKBACK);
    // Compounded, not summed: a regime's return is what an account actually did
    // over those periods, and adding period returns overstates a rising path
    // and understates a falling one.
    let mut growth: BTreeMap<Regime, (f64, f64, usize)> = BTreeMap::new();
    let mut unlabelled = 0;

    for (index, label) in labels.iter().enumerate().skip(1) {
        let Some(regime) = *label else {
            unlabelled += 1;
            continue;
        };
        let entry = growth.entry(regime).or_insert((1.0, 1.0, 0));
        entry.0 *= 1.0 + period_return(strategy, index);
        entry.1 *= 1.0 + period_return(benchmark, index);
        entry.2 += 1;
    }

    let labelled: usize = growth.values().map(|(_, _, periods)| periods).sum();
    if labelled == 0 {
        return None;
    }

    #[expect(clippy::cast_precision_loss, reason = "period counts are small")]
    let total = labelled as f64;
    let outcomes = growth
        .into_iter()
        .map(|(regime, (strategy_growth, benchmark_growth, periods))| {
            let strategy_return = strategy_growth - 1.0;
            let benchmark_return = benchmark_growth - 1.0;
            RegimeOutcome {
                regime,
                periods,
                #[expect(clippy::cast_precision_loss, reason = "period counts are small")]
                share: periods as f64 / total,
                strategy_return,
                benchmark_return,
                excess_return: strategy_return - benchmark_return,
            }
        })
        .collect();

    Some(Breakdown {
        outcomes,
        unlabelled,
    })
}

/// One period's return, guarding a zero or negative starting equity.
fn period_return(curve: &[EquityPoint], index: usize) -> f64 {
    let previous = curve[index - 1].equity;
    if previous <= 0.0 {
        return 0.0;
    }
    (curve[index].equity - previous) / previous
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};

    fn at(index: usize) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 1, 1)
            .expect("valid")
            .and_time(NaiveTime::MIN)
            + chrono::Duration::days(i64::try_from(index).expect("small"))
    }

    fn curve(values: &[f64]) -> Vec<EquityPoint> {
        values
            .iter()
            .enumerate()
            .map(|(index, equity)| EquityPoint {
                at: at(index),
                equity: *equity,
            })
            .collect()
    }

    /// A path that climbs steadily: every step the same direction.
    fn climbing(periods: usize) -> Vec<f64> {
        (0..periods)
            .map(|i| 100.0 + f64::from(u32::try_from(i).expect("small")))
            .collect()
    }

    /// A path that oscillates around one level and ends where it started.
    fn chopping(periods: usize) -> Vec<f64> {
        (0..periods)
            .map(|i| if i % 2 == 0 { 100.0 } else { 103.0 })
            .collect()
    }

    #[test]
    fn a_path_that_goes_somewhere_is_trending() {
        let labels = label(&curve(&climbing(40)), LOOKBACK);
        assert_eq!(labels[39], Some(Regime::TrendingUp));
    }

    #[test]
    fn a_path_that_ends_where_it_started_is_ranging() {
        // The distinction the threshold exists for: this one moves constantly
        // and arrives nowhere. "Ranging" is not "flat".
        let labels = label(&curve(&chopping(40)), LOOKBACK);
        assert_eq!(labels[39], Some(Regime::Ranging));
    }

    #[test]
    fn a_falling_trend_is_not_confused_with_a_range() {
        let falling: Vec<f64> = climbing(40).into_iter().rev().collect();
        let labels = label(&curve(&falling), LOOKBACK);
        assert_eq!(labels[39], Some(Regime::TrendingDown));
    }

    #[test]
    fn nothing_is_labelled_before_the_lookback_has_filled() {
        // A label from a partial window would describe less market than it
        // claims to, and the periods it covers are still part of the run.
        let labels = label(&curve(&climbing(40)), LOOKBACK);
        assert!(
            labels[..LOOKBACK].iter().all(Option::is_none),
            "the first {LOOKBACK} periods have no full window behind them"
        );
        assert!(labels[LOOKBACK].is_some());
    }

    #[test]
    fn a_curve_that_never_moved_is_ranging_rather_than_a_division_by_zero() {
        let labels = label(&curve(&[100.0; 40]), LOOKBACK);
        assert_eq!(labels[39], Some(Regime::Ranging));
    }

    /// A benchmark that trends for the first half and ranges for the second,
    /// with a strategy that tracks it in the trend and bleeds in the range.
    fn split_window() -> (Vec<EquityPoint>, Vec<EquityPoint>) {
        let mut benchmark = climbing(60);
        benchmark.extend(chopping(60).iter().map(|v| v + 59.0));

        let mut strategy = Vec::with_capacity(benchmark.len());
        let mut equity = 100.0;
        for (index, _) in benchmark.iter().enumerate() {
            if index < 60 {
                equity += 2.0; // outruns the benchmark in the trend
            } else {
                equity -= 0.4; // and bleeds in the chop, where it beats nothing
            }
            strategy.push(equity);
        }
        (curve(&strategy), curve(&benchmark))
    }

    #[test]
    fn a_result_earned_in_one_regime_and_lost_in_another_is_split() {
        // The whole point. One pooled number would average these into a
        // statement describing neither half.
        let (strategy, benchmark) = split_window();
        let breakdown = attribute(&strategy, &benchmark).expect("same length");

        assert!(
            breakdown.outcomes.len() >= 2,
            "the window has more than one regime in it: {:?}",
            breakdown.outcomes
        );
        let trending = breakdown
            .outcomes
            .iter()
            .find(|o| o.regime == Regime::TrendingUp)
            .expect("the first half trends");
        let ranging = breakdown
            .outcomes
            .iter()
            .find(|o| o.regime == Regime::Ranging)
            .expect("the second half ranges");
        assert!(
            trending.strategy_return > ranging.strategy_return,
            "it made money in the trend and lost it in the chop: {trending:?} vs {ranging:?}"
        );
    }

    #[test]
    fn a_disagreement_between_regimes_is_reported() {
        let (strategy, benchmark) = split_window();
        let breakdown = attribute(&strategy, &benchmark).expect("same length");
        let (best, worst) = breakdown.split().expect("it beat one and lost the other");
        assert_ne!(best.regime, worst.regime);
        assert!(best.excess_return > 0.0 && worst.excess_return < 0.0);
    }

    #[test]
    fn a_result_that_did_the_same_thing_everywhere_is_not_split() {
        // Silence is the right answer when nothing disagrees. A line saying
        // "this behaved consistently" on every result is a line that teaches a
        // reader to skip the section.
        let benchmark = curve(&climbing(80));
        // Tracks the benchmark exactly, so excess is zero in every regime.
        let breakdown = attribute(&benchmark, &benchmark).expect("same length");
        assert!(breakdown.split().is_none());
    }

    #[test]
    fn a_regime_with_too_few_periods_does_not_carry_the_split() {
        // A statistic over a handful of observations is a statistic about the
        // handful, and letting it name the best or worst regime would be
        // reporting noise as a finding.
        let breakdown = Breakdown {
            outcomes: vec![
                RegimeOutcome {
                    regime: Regime::TrendingUp,
                    periods: 3,
                    share: 0.05,
                    strategy_return: 0.9,
                    benchmark_return: 0.0,
                    excess_return: 0.9,
                },
                RegimeOutcome {
                    regime: Regime::Ranging,
                    periods: 50,
                    share: 0.95,
                    strategy_return: -0.2,
                    benchmark_return: 0.0,
                    excess_return: -0.2,
                },
            ],
            unlabelled: 0,
        };
        assert!(
            breakdown.split().is_none(),
            "three periods cannot be the regime this rule works in"
        );
    }

    #[test]
    fn a_gap_too_small_to_change_a_reading_is_not_reported() {
        let breakdown = Breakdown {
            outcomes: vec![
                RegimeOutcome {
                    regime: Regime::TrendingUp,
                    periods: 40,
                    share: 0.5,
                    strategy_return: 0.01,
                    benchmark_return: 0.0,
                    excess_return: 0.01,
                },
                RegimeOutcome {
                    regime: Regime::Ranging,
                    periods: 40,
                    share: 0.5,
                    strategy_return: -0.01,
                    benchmark_return: 0.0,
                    excess_return: -0.01,
                },
            ],
            unlabelled: 0,
        };
        assert!(breakdown.split().is_none(), "two points is not a finding");
    }

    #[test]
    fn unlabelled_periods_are_counted_rather_than_folded_into_a_regime() {
        // They are part of the run's return and no part of any regime's.
        // Hiding them would make the regime returns look as though they sum to
        // the whole.
        let benchmark = curve(&climbing(40));
        let breakdown = attribute(&benchmark, &benchmark).expect("same length");
        assert_eq!(
            breakdown.unlabelled,
            LOOKBACK - 1,
            "every period before the lookback filled, less the first which has \
             no return of its own"
        );
    }

    #[test]
    fn two_curves_of_different_lengths_are_not_attributed() {
        // Different lengths mean different runs, and attributing one curve's
        // returns to the other's regimes would be silently wrong.
        assert!(attribute(&curve(&climbing(40)), &curve(&climbing(30))).is_none());
    }

    #[test]
    fn shares_add_up_to_the_labelled_window() {
        let (strategy, benchmark) = split_window();
        let breakdown = attribute(&strategy, &benchmark).expect("same length");
        let total: f64 = breakdown.outcomes.iter().map(|o| o.share).sum();
        assert!((total - 1.0).abs() < 1e-9, "shares summed to {total}");
    }
}
