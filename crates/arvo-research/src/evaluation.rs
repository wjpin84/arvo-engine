//! Deciding whether a result means anything.
//!
//! This is the part of Arvo that no trading engine provides, and the part
//! worth building carefully. An engine tells you what a strategy *did*.
//! Evaluation decides whether that is evidence of anything.
//!
//! Two commitments shape everything here:
//!
//! * **Nothing is judged in isolation.** Every experiment is scored against a
//!   benchmark run through the same engine, over the same window, paying the
//!   same costs. A strategy that made money while buy-and-hold made more has
//!   demonstrated nothing, and the most common way a backtest flatters itself
//!   is by never being asked.
//! * **[`Verdict::Inconclusive`] is a real answer.** Most experiments should
//!   return it. A system that only ever says supported or refuted, on runs
//!   with four trades and no out-of-sample data, is manufacturing confidence.

use serde::{Deserialize, Serialize};

use crate::{
    EquityPoint, Experiment, ExperimentId, HypothesisId, SimulationError, SimulationProvider,
    SimulationResult, StrategySpec,
};

/// Trading days in a year, for annualising daily-bar results.
///
/// The slice runs on daily bars. When a second bar interval appears this
/// becomes a property of the dataset rather than a constant.
pub const TRADING_DAYS_PER_YEAR: f64 = 252.0;

/// The benchmark every experiment is scored against: buy on the first bar,
/// hold to the end, pay the same costs.
pub const BUY_AND_HOLD: &str = "buy_and_hold";

/// What an equity curve says, once.
///
/// Deliberately small. These are the statistics that need no assumption
/// beyond the curve itself; anything requiring a model of returns belongs
/// somewhere it can be argued with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Fraction, not percent. `0.1` is a 10% gain.
    pub total_return: f64,
    /// Compound annual growth rate, from the number of periods observed.
    pub cagr: f64,
    /// Largest peak-to-trough fall, as a positive fraction.
    pub max_drawdown: f64,
    /// Annualised standard deviation of period returns.
    pub volatility: f64,
    /// Annualised return over volatility, at a zero risk-free rate.
    ///
    /// `None` when the curve never moves — a Sharpe ratio of a constant is a
    /// division by zero, and reporting it as `0.0` would read like a real
    /// measurement of a flat strategy.
    pub sharpe: Option<f64>,
    pub trades: u32,
}

impl Metrics {
    /// Derives every statistic from an equity curve.
    ///
    /// Returns `None` for a curve with fewer than two points or one that
    /// starts at zero — there is no return to speak of, and inventing one
    /// would put a number where there is no measurement.
    #[must_use]
    pub fn from_curve(curve: &[EquityPoint], trades: u32, periods_per_year: f64) -> Option<Self> {
        if curve.len() < 2 || periods_per_year <= 0.0 {
            return None;
        }
        let first = curve.first()?.equity;
        let last = curve.last()?.equity;
        if first <= 0.0 {
            return None;
        }

        let returns: Vec<f64> = curve
            .windows(2)
            .map(|pair| {
                if pair[0].equity == 0.0 {
                    0.0
                } else {
                    (pair[1].equity - pair[0].equity) / pair[0].equity
                }
            })
            .collect();

        let total_return = (last - first) / first;
        let years = returns.len() as f64 / periods_per_year;
        let cagr = if years > 0.0 && last > 0.0 {
            (last / first).powf(1.0 / years) - 1.0
        } else {
            0.0
        };

        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        // Sample standard deviation: these returns are a sample of the
        // strategy's behaviour, not the whole population of it.
        let variance = if returns.len() < 2 {
            0.0
        } else {
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() as f64 - 1.0)
        };
        let volatility = variance.sqrt() * periods_per_year.sqrt();
        let sharpe = if volatility > 0.0 {
            Some(mean * periods_per_year / volatility)
        } else {
            None
        };

        Some(Self {
            total_return,
            cagr,
            max_drawdown: max_drawdown(curve),
            volatility,
            sharpe,
            trades,
        })
    }
}

fn max_drawdown(curve: &[EquityPoint]) -> f64 {
    let mut peak = f64::MIN;
    let mut worst: f64 = 0.0;
    for point in curve {
        if point.equity > peak {
            peak = point.equity;
        }
        if peak > 0.0 {
            worst = worst.max((peak - point.equity) / peak);
        }
    }
    worst
}

/// The bar a result must clear, stated before the result is known.
///
/// Recorded into [`Evidence`] alongside the verdict, because a threshold
/// chosen after seeing the number is not a threshold. Moving these is allowed;
/// moving them silently is not.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EvaluationCriteria {
    /// Below this, the run is [`Verdict::Inconclusive`] whatever it returned.
    /// A handful of trades is a handful of coin flips.
    pub min_trades: u32,
    /// How far the strategy must beat its benchmark, as a fraction.
    pub min_excess_return: f64,
    /// A drawdown ceiling. A strategy that beat the benchmark on the way
    /// through a 60% drawdown is not one anybody would have held.
    pub max_drawdown: f64,
}

impl Default for EvaluationCriteria {
    /// Deliberately hard to clear. The default should refuse more than it
    /// accepts; a permissive default silently becomes the standard.
    fn default() -> Self {
        Self {
            min_trades: 30,
            min_excess_return: 0.0,
            max_drawdown: 0.30,
        }
    }
}

/// What an experiment showed about its hypothesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Beat the benchmark by the required margin, within the risk ceiling,
    /// on enough trades to be worth reading.
    Supported,
    /// Ran cleanly and did not clear the bar.
    NotSupported,
    /// The run cannot answer the question either way. The expected outcome,
    /// and not a failure — an inconclusive result that says so is worth more
    /// than a confident one that should not be.
    Inconclusive,
}

/// A result, its benchmark, and what the comparison supports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub strategy: Metrics,
    pub benchmark: Metrics,
    /// The curves behind the numbers, kept so a finding can be *drawn* and not
    /// only summarised — including one read back out of research memory long
    /// after the run. Metrics are a lossy projection of these; a chart of the
    /// two side by side says things no table does, like whether an edge was
    /// steady or one lucky month.
    ///
    /// `default` because this is a persisted format: a finding recorded before
    /// curves were kept should still load, as a finding with no chart, rather
    /// than becoming unreadable. Every field added here from now on needs the
    /// same courtesy.
    #[serde(default)]
    pub strategy_curve: Vec<EquityPoint>,
    #[serde(default)]
    pub benchmark_curve: Vec<EquityPoint>,
    /// Strategy return minus benchmark return. The number that matters:
    /// absolute return mostly measures whether the market went up.
    pub excess_return: f64,
    pub verdict: Verdict,
    /// Why the verdict came out that way, in the order the checks ran.
    pub reasons: Vec<String>,
}

impl Evaluation {
    /// Scores a strategy against its benchmark under stated criteria.
    #[must_use]
    pub fn new(
        strategy: Metrics,
        benchmark: Metrics,
        strategy_curve: Vec<EquityPoint>,
        benchmark_curve: Vec<EquityPoint>,
        criteria: &EvaluationCriteria,
    ) -> Self {
        let excess_return = strategy.total_return - benchmark.total_return;
        let mut reasons = Vec::new();

        let verdict = if strategy.trades < criteria.min_trades {
            reasons.push(format!(
                "{} trades is below the {} needed to read anything into the result",
                strategy.trades, criteria.min_trades
            ));
            Verdict::Inconclusive
        } else if excess_return < criteria.min_excess_return {
            reasons.push(format!(
                "excess return {excess_return:.4} did not clear {:.4} over buy-and-hold",
                criteria.min_excess_return
            ));
            Verdict::NotSupported
        } else if strategy.max_drawdown > criteria.max_drawdown {
            reasons.push(format!(
                "drawdown {:.4} exceeded the {:.4} ceiling, so the excess return was not \
                 obtainable in practice",
                strategy.max_drawdown, criteria.max_drawdown
            ));
            Verdict::NotSupported
        } else {
            reasons.push(format!(
                "beat buy-and-hold by {excess_return:.4} over {} trades, worst drawdown {:.4}",
                strategy.trades, strategy.max_drawdown
            ));
            Verdict::Supported
        };

        Self {
            strategy,
            benchmark,
            strategy_curve,
            benchmark_curve,
            excess_return,
            verdict,
            reasons,
        }
    }
}

/// The durable record: what was asked, what was run, and what it showed.
///
/// Carries the whole [`Experiment`] rather than its id, because evidence
/// outlives whatever produced it. A record that points at an experiment
/// defined elsewhere is only reproducible while that elsewhere still exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub hypothesis: HypothesisId,
    pub experiment: Experiment,
    /// The benchmark run this was scored against.
    pub benchmark: ExperimentId,
    /// The engine that produced both results, e.g. `"nautilus 0.63.0"`.
    /// Results from different engine versions are not comparable.
    pub engine: String,
    pub criteria: EvaluationCriteria,
    pub evaluation: Evaluation,
}

/// Runs an experiment and its benchmark, and returns what the pair supports.
///
/// This is the research loop's spine, and it lives here rather than in the
/// engine crate on purpose: it is stated entirely in terms of
/// [`SimulationProvider`], so the research domain drives the engine without
/// naming it.
///
/// The benchmark is derived from the experiment rather than supplied, so the
/// two cannot drift apart: same instrument, same window, same dataset, same
/// costs, same capital, same trade size. Only the strategy differs.
///
/// # Errors
///
/// Returns [`SimulationError`] if either run fails, or if a completed run
/// produced a curve too short to evaluate.
pub fn evaluate_against_benchmark(
    provider: &dyn SimulationProvider,
    experiment: &Experiment,
    criteria: &EvaluationCriteria,
) -> Result<Evidence, SimulationError> {
    let benchmark_experiment = benchmark_for(experiment);

    let strategy_result = provider.run(experiment)?;
    let benchmark_result = provider.run(&benchmark_experiment)?;

    let metrics = |result: &SimulationResult, what: &str| {
        Metrics::from_curve(&result.equity_curve, result.trades, TRADING_DAYS_PER_YEAR).ok_or_else(
            || {
                SimulationError::Rejected(format!(
                    "the {what} run produced {} equity points, too few to evaluate",
                    result.equity_curve.len()
                ))
            },
        )
    };

    let engine = strategy_result.engine.clone();
    let evaluation = Evaluation::new(
        metrics(&strategy_result, "strategy")?,
        metrics(&benchmark_result, "benchmark")?,
        strategy_result.equity_curve,
        benchmark_result.equity_curve,
        criteria,
    );

    Ok(Evidence {
        hypothesis: experiment.hypothesis.clone(),
        experiment: experiment.clone(),
        benchmark: benchmark_experiment.id,
        engine,
        criteria: *criteria,
        evaluation,
    })
}

/// Derives the buy-and-hold run an experiment is scored against.
#[must_use]
pub fn benchmark_for(experiment: &Experiment) -> Experiment {
    Experiment {
        id: ExperimentId(format!("{}-benchmark", experiment.id)),
        strategy: StrategySpec {
            name: BUY_AND_HOLD.to_owned(),
            // Same trade size, so the two curves are the same size of bet and
            // the difference between them is the strategy rather than the
            // stake. Other parameters mean nothing to buy-and-hold.
            params: experiment.strategy.params.clone(),
        },
        ..experiment.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(total_return: f64, max_drawdown: f64, trades: u32) -> Metrics {
        Metrics {
            total_return,
            cagr: total_return,
            max_drawdown,
            volatility: 0.1,
            sharpe: Some(1.0),
            trades,
        }
    }

    /// Dates are irrelevant to every statistic here, so the fixtures walk one
    /// day at a time and the tests stay about the numbers.
    fn curve(values: &[f64]) -> Vec<EquityPoint> {
        let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid");
        values
            .iter()
            .enumerate()
            .map(|(index, equity)| EquityPoint {
                date: start + chrono::Duration::days(index as i64),
                equity: *equity,
            })
            .collect()
    }

    #[test]
    fn a_flat_curve_has_no_sharpe_rather_than_a_sharpe_of_zero() {
        let flat = Metrics::from_curve(&curve(&[100.0, 100.0, 100.0]), 0, TRADING_DAYS_PER_YEAR)
            .expect("three points is enough");
        assert_eq!(flat.sharpe, None);
        assert_eq!(flat.volatility, 0.0);
        assert!((flat.total_return - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_curve_too_short_to_measure_yields_nothing() {
        assert!(Metrics::from_curve(&curve(&[100.0]), 0, TRADING_DAYS_PER_YEAR).is_none());
        assert!(Metrics::from_curve(&curve(&[]), 0, TRADING_DAYS_PER_YEAR).is_none());
        assert!(
            Metrics::from_curve(&curve(&[0.0, 100.0]), 0, TRADING_DAYS_PER_YEAR).is_none(),
            "a curve starting at zero has no defined return"
        );
    }

    #[test]
    fn drawdown_measures_peak_to_trough_not_start_to_end() {
        let recovered = Metrics::from_curve(
            &curve(&[100.0, 150.0, 75.0, 120.0]),
            1,
            TRADING_DAYS_PER_YEAR,
        )
        .expect("four points");
        assert!(
            (recovered.max_drawdown - 0.5).abs() < 1e-12,
            "150 to 75 is a 50% fall even though the curve ends up: {}",
            recovered.max_drawdown
        );
        assert!(
            recovered.total_return > 0.0,
            "and the run was still profitable overall"
        );
    }

    #[test]
    fn too_few_trades_is_inconclusive_however_good_the_return() {
        let criteria = EvaluationCriteria::default();
        let evaluation = Evaluation::new(
            metrics(5.0, 0.01, criteria.min_trades - 1),
            metrics(0.01, 0.01, 1),
            Vec::new(),
            Vec::new(),
            &criteria,
        );

        assert_eq!(
            evaluation.verdict,
            Verdict::Inconclusive,
            "a 500% return on a few trades is a coin flip, not a finding"
        );
        assert!(
            evaluation.reasons[0].contains("trades"),
            "{:?}",
            evaluation.reasons
        );
    }

    #[test]
    fn beating_the_market_is_the_test_not_making_money() {
        let criteria = EvaluationCriteria::default();
        // Made 20% in a market that made 50%.
        let evaluation = Evaluation::new(
            metrics(0.20, 0.05, 100),
            metrics(0.50, 0.05, 1),
            Vec::new(),
            Vec::new(),
            &criteria,
        );

        assert_eq!(evaluation.verdict, Verdict::NotSupported);
        assert!((evaluation.excess_return - -0.30).abs() < 1e-12);
    }

    #[test]
    fn an_unholdable_drawdown_disqualifies_a_winning_strategy() {
        let criteria = EvaluationCriteria::default();
        let evaluation = Evaluation::new(
            metrics(0.40, 0.55, 100),
            metrics(0.10, 0.05, 1),
            Vec::new(),
            Vec::new(),
            &criteria,
        );

        assert_eq!(evaluation.verdict, Verdict::NotSupported);
        assert!(
            evaluation.reasons[0].contains("drawdown"),
            "{:?}",
            evaluation.reasons
        );
    }

    #[test]
    fn a_clean_win_over_the_benchmark_is_supported() {
        let criteria = EvaluationCriteria::default();
        let evaluation = Evaluation::new(
            metrics(0.40, 0.10, 100),
            metrics(0.10, 0.05, 1),
            Vec::new(),
            Vec::new(),
            &criteria,
        );

        assert_eq!(evaluation.verdict, Verdict::Supported);
        assert!((evaluation.excess_return - 0.30).abs() < 1e-12);
    }

    #[test]
    fn the_benchmark_differs_from_its_experiment_only_by_strategy() {
        use crate::{CostModel, DatasetRef, DateRange, HypothesisId};
        use chrono::NaiveDate;
        use std::collections::BTreeMap;

        let day = |d: u32| NaiveDate::from_ymd_opt(2024, 1, d).expect("valid date");
        let experiment = Experiment {
            id: ExperimentId::from("e-1"),
            hypothesis: HypothesisId::from("h-1"),
            instrument: "AAPL.NASDAQ".to_owned(),
            window: DateRange::new(day(1), day(31)).expect("ordered"),
            dataset: DatasetRef {
                id: "d".to_owned(),
                version: "1".to_owned(),
            },
            strategy: StrategySpec {
                name: "sma_cross".to_owned(),
                params: BTreeMap::from([("trade_size".to_owned(), 100.0)]),
            },
            costs: CostModel {
                commission_bps: 1.5,
                slippage_bps: 0.0,
            },
            starting_cash: 100_000.0,
            seed: 7,
        };

        let benchmark = benchmark_for(&experiment);

        assert_eq!(benchmark.strategy.name, BUY_AND_HOLD);
        assert_ne!(benchmark.id, experiment.id, "it is a separate run");
        assert_eq!(benchmark.window, experiment.window);
        assert_eq!(benchmark.dataset, experiment.dataset);
        assert_eq!(benchmark.costs, experiment.costs);
        assert_eq!(benchmark.instrument, experiment.instrument);
        assert_eq!(
            benchmark.starting_cash, experiment.starting_cash,
            "the comparison is only fair at the same stake"
        );
    }
}
