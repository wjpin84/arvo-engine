//! Testing one idea across many instruments at once.
//!
//! A single-instrument study cannot conclude anything, and that is not a
//! tuning problem. A moving-average crossover trades ten to twenty times in
//! twenty years; thirty round trips is roughly the minimum for a mean return
//! to mean anything. One instrument does not have enough history in it, and
//! never will.
//!
//! Breadth is the answer, and it is a better experiment as well as a bigger
//! one:
//!
//! * **One configuration is chosen for the whole panel**, not one per
//!   instrument. Tuning parameters per instrument is a second search — nine
//!   configurations over ten instruments is ninety chances to find something
//!   that fits — and the per-instrument winners are exactly what
//!   [`crate::family`] exists to distrust. Here a configuration has to work
//!   across the panel in-sample to be chosen at all.
//! * **Evidence pools.** Trades add up across instruments, so the trade-count
//!   bar becomes reachable honestly rather than by lowering it.
//! * **Consistency becomes visible.** Beating the benchmark on eight of ten
//!   instruments is a different claim from beating it on one by a mile, and
//!   the two are indistinguishable in a single-instrument result.
//!
//! # What this is not
//!
//! Not a portfolio backtest. Nothing here allocates capital across
//! instruments, rebalances, or models the correlation between them. It is a
//! *panel test* of a hypothesis: the same rule applied independently to many
//! series, with the results summarised across them. The pooled drawdown is
//! therefore an average of separate drawdowns and understates what a real
//! combined position would have suffered, because correlation is not
//! modelled. Named and stated rather than quietly averaged.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::evaluation::{benchmark_for, EvaluationCriteria};
use crate::family::{expected_best_under_null, ParameterGrid, Selection};
use crate::{
    DateRange, Experiment, ExperimentId, HypothesisId, Metrics, SimulationError,
    SimulationProvider, StrategySpec, Verdict,
};

/// One idea, one grid, many instruments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelStudy {
    pub hypothesis: HypothesisId,
    /// The experiment every run is a variation of. Its `instrument` is
    /// ignored — [`Self::instruments`] supplies those — but its window,
    /// dataset, costs and capital apply to all of them.
    pub template: Experiment,
    pub instruments: Vec<String>,
    pub grid: ParameterGrid,
    pub in_sample_fraction: f64,
}

impl PanelStudy {
    #[must_use]
    pub fn new(template: Experiment, instruments: Vec<String>, grid: ParameterGrid) -> Self {
        Self {
            hypothesis: template.hypothesis.clone(),
            template,
            instruments,
            grid,
            in_sample_fraction: crate::family::DEFAULT_IN_SAMPLE_FRACTION,
        }
    }
}

/// How one instrument fared out-of-sample under the chosen configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentOutcome {
    pub instrument: String,
    pub strategy: Metrics,
    pub benchmark: Metrics,
    pub excess_return: f64,
}

/// The panel's evidence, summarised across instruments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PooledOutcome {
    /// Instruments that produced a usable out-of-sample result.
    pub instruments: usize,
    /// Round trips summed across the panel. This is what makes a verdict
    /// reachable at all.
    pub total_trades: u32,
    /// Equal-weighted mean of the per-instrument excess returns.
    pub mean_excess_return: f64,
    /// How many instruments beat their own benchmark. Consistency and
    /// magnitude are different claims, and this is the one a single-instrument
    /// result cannot make.
    pub beat_benchmark: usize,
    /// Mean of the per-instrument drawdowns. Understates a combined position's
    /// drawdown, because correlation is not modelled — see the module docs.
    pub mean_max_drawdown: f64,
    /// The worst single instrument, kept because an average hides it.
    pub worst_max_drawdown: f64,
}

/// What a panel study concluded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelEvidence {
    pub hypothesis: HypothesisId,
    /// The data every member was run against, combined. Carried so a stored
    /// panel can be checked against the files still on disk — without it a
    /// persisted result could never be found stale.
    pub dataset: crate::DatasetRef,
    pub in_sample: DateRange,
    pub out_of_sample: DateRange,
    /// The one configuration chosen for the whole panel.
    pub selected_params: BTreeMap<String, f64>,
    pub selection: Selection,
    pub per_instrument: Vec<InstrumentOutcome>,
    pub pooled: PooledOutcome,
    /// How much of the panel's apparent breadth is real.
    ///
    /// The pooled statistics read as evidence in proportion to the number of
    /// instruments — three that agree feel like three times the confidence of
    /// one. They are not, if the three moved together, and until this was
    /// measured nothing in the panel could tell the difference.
    ///
    /// `default` because it is a persisted format: a panel recorded before
    /// this existed loads as one that does not know its own breadth.
    #[serde(default)]
    pub breadth: Option<crate::Breadth>,
    /// Instrument/configuration combinations that could not be run.
    pub failures: Vec<String>,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

/// Runs a panel study: choose one configuration in-sample across every
/// instrument, then judge it out-of-sample on all of them.
///
/// Costs `grid × instruments` in-sample runs plus `2 × instruments`
/// out-of-sample ones. The selection pass is the expensive half and is the
/// price of not tuning per instrument.
///
/// # Errors
///
/// Returns [`SimulationError`] if the grid is empty, the window cannot be
/// split, no instrument was supplied, or nothing produced a usable result.
pub fn run_panel(
    provider: &dyn SimulationProvider,
    study: &PanelStudy,
    criteria: &EvaluationCriteria,
) -> Result<PanelEvidence, SimulationError> {
    let combinations = study.grid.combinations();
    if combinations.is_empty() {
        return Err(SimulationError::Rejected(
            "the parameter grid is empty, so the panel tests nothing".to_owned(),
        ));
    }
    if study.instruments.is_empty() {
        return Err(SimulationError::Rejected(
            "a panel needs at least one instrument".to_owned(),
        ));
    }

    let (in_sample, out_of_sample) = study
        .template
        .window
        .split(study.in_sample_fraction)
        .ok_or_else(|| {
            SimulationError::Rejected(format!(
                "window {}..={} cannot be split at {} into two usable periods",
                study.template.window.from, study.template.window.to, study.in_sample_fraction
            ))
        })?;

    let mut failures = Vec::new();

    // --- Selection: one configuration, scored across the whole panel --------
    let mut scored: Vec<(f64, BTreeMap<String, f64>)> = Vec::with_capacity(combinations.len());
    for combination in combinations {
        let mut sharpes = Vec::new();
        for instrument in &study.instruments {
            let trial = variant(&study.template, instrument, &combination, in_sample, "is");
            match provider.run(&trial) {
                Ok(result) => {
                    if let Some(sharpe) = Metrics::from_curve(
                        &result.equity_curve,
                        result.trades,
                        study.template.interval.periods_per_year(),
                    )
                    .and_then(|metrics| metrics.sharpe)
                    {
                        sharpes.push(sharpe);
                    }
                }
                Err(err) => failures.push(format!("{instrument} {combination:?}: {err}")),
            }
        }

        // A configuration is only in the running if it worked across the
        // panel. One instrument's good run is not evidence the rule
        // generalises, which is the entire reason for testing many.
        if sharpes.len() == study.instruments.len() {
            let mean = sharpes.iter().sum::<f64>() / sharpes.len() as f64;
            scored.push((mean, combination));
        } else {
            failures.push(format!(
                "{combination:?}: ran on {} of {} instruments, so it was not considered",
                sharpes.len(),
                study.instruments.len()
            ));
        }
    }

    let sharpes: Vec<f64> = scored.iter().map(|(mean, _)| *mean).collect();
    // The pooled surface, kept for the same reason a single study's is: one
    // bright cell surrounded by nothing looks identical to a plateau if only
    // the maximum is reported.
    let surface: Vec<crate::family::ScoredTrial> = scored
        .iter()
        .map(|(mean, params)| crate::family::ScoredTrial {
            params: params.clone(),
            sharpe: *mean,
        })
        .collect();
    let (best_sharpe, selected_params) = scored
        .into_iter()
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .ok_or_else(|| {
            SimulationError::Rejected(format!(
                "no configuration ran across the whole panel ({} failures)",
                failures.len()
            ))
        })?;

    let expected = expected_best_under_null(&sharpes);
    let survived_deflation = expected.is_none_or(|bar| best_sharpe > bar);
    let selection = Selection {
        trials: sharpes.len(),
        best_sharpe,
        expected_best_under_null: expected,
        survived_deflation,
        scored: surface,
    };

    // --- Judgement: that one configuration, on data it never saw -----------
    let mut per_instrument = Vec::new();
    // Kept only long enough to measure how much the members moved together.
    // Storing N curves in a panel record would add megabytes to say something
    // the correlation matrix says in a few numbers.
    let mut curves: Vec<(String, Vec<crate::EquityPoint>)> = Vec::new();
    for instrument in &study.instruments {
        let experiment = variant(
            &study.template,
            instrument,
            &selected_params,
            out_of_sample,
            "oos",
        );
        let benchmark = benchmark_for(&experiment);

        let outcome = provider.run(&experiment).and_then(|strategy_result| {
            let benchmark_result = provider.run(&benchmark)?;
            let periods = study.template.interval.periods_per_year();
            let metrics = |result: &crate::SimulationResult| {
                Metrics::from_curve(&result.equity_curve, result.trades, periods)
            };
            match (metrics(&strategy_result), metrics(&benchmark_result)) {
                (Some(strategy), Some(benchmark)) => Ok((
                    InstrumentOutcome {
                        instrument: instrument.clone(),
                        excess_return: strategy.total_return - benchmark.total_return,
                        strategy,
                        benchmark,
                    },
                    strategy_result.equity_curve,
                )),
                _ => Err(SimulationError::Rejected(
                    "out-of-sample run produced too few equity points to evaluate".to_owned(),
                )),
            }
        });

        match outcome {
            Ok((outcome, curve)) => {
                curves.push((outcome.instrument.clone(), curve));
                per_instrument.push(outcome);
            }
            Err(err) => failures.push(format!("{instrument} out-of-sample: {err}")),
        }
    }

    if per_instrument.is_empty() {
        return Err(SimulationError::Rejected(format!(
            "no instrument produced an out-of-sample result ({} failures)",
            failures.len()
        )));
    }

    let pooled = pool(&per_instrument);
    let breadth = crate::breadth::measure(&curves);
    let (verdict, reasons) = judge(&pooled, &selection, criteria, &failures, &breadth);

    Ok(PanelEvidence {
        hypothesis: study.hypothesis.clone(),
        dataset: study.template.dataset.clone(),
        in_sample,
        out_of_sample,
        selected_params,
        selection,
        per_instrument,
        pooled,
        breadth: Some(breadth),
        failures,
        verdict,
        reasons,
    })
}

fn pool(outcomes: &[InstrumentOutcome]) -> PooledOutcome {
    let count = outcomes.len() as f64;
    PooledOutcome {
        instruments: outcomes.len(),
        total_trades: outcomes
            .iter()
            .map(|o| o.strategy.trades)
            .fold(0_u32, u32::saturating_add),
        mean_excess_return: outcomes.iter().map(|o| o.excess_return).sum::<f64>() / count,
        beat_benchmark: outcomes.iter().filter(|o| o.excess_return > 0.0).count(),
        mean_max_drawdown: outcomes
            .iter()
            .map(|o| o.strategy.max_drawdown)
            .sum::<f64>()
            / count,
        worst_max_drawdown: outcomes
            .iter()
            .map(|o| o.strategy.max_drawdown)
            .fold(0.0_f64, f64::max),
    }
}

/// Below this, the panel's members are near enough independent to be counted.
///
/// 1.25 means the pooled average's standard error is a quarter larger than its
/// instrument count implies — small enough to ignore, and the point at which
/// saying so stops being pedantry and starts being a correction.
const OVERSTATEMENT_WORTH_SAYING: f64 = 1.25;

fn judge(
    pooled: &PooledOutcome,
    selection: &Selection,
    criteria: &EvaluationCriteria,
    failures: &[String],
    breadth: &crate::Breadth,
) -> (Verdict, Vec<String>) {
    let mut reasons = Vec::new();

    // Said before the verdict rather than after it, because it changes what
    // every number below means. The pooled statistics read as evidence in
    // proportion to the instrument count; if the instruments moved together,
    // that count is not the sample size it looks like.
    if let (Some(effective), Some(overstatement)) = (breadth.effective, breadth.overstatement()) {
        if overstatement >= OVERSTATEMENT_WORTH_SAYING {
            reasons.push(format!(
                "these {} instruments behave like {effective:.1} independent ones (average                  correlation {:.2}), so the pooled average is about {overstatement:.1}x less                  certain than its instrument count suggests",
                breadth.instruments.len(),
                breadth.mean_correlation.unwrap_or_default(),
            ));
        }
    }

    let verdict = if !selection.survived_deflation {
        reasons.push(format!(
            "best pooled in-sample Sharpe {:.3} across {} configurations did not beat the {:.3} a \
             no-skill search of that size would be expected to produce",
            selection.best_sharpe,
            selection.trials,
            selection.expected_best_under_null.unwrap_or_default()
        ));
        Verdict::NotSupported
    } else if pooled.total_trades < criteria.min_trades {
        reasons.push(format!(
            "{} trades across {} instruments is still below the {} needed",
            pooled.total_trades, pooled.instruments, criteria.min_trades
        ));
        Verdict::Inconclusive
    } else if pooled.mean_excess_return < criteria.min_excess_return {
        reasons.push(format!(
            "mean excess return {:.4} did not clear {:.4} over buy-and-hold; beat the benchmark on \
             {} of {} instruments",
            pooled.mean_excess_return,
            criteria.min_excess_return,
            pooled.beat_benchmark,
            pooled.instruments
        ));
        Verdict::NotSupported
    } else if pooled.mean_max_drawdown > criteria.max_drawdown {
        reasons.push(format!(
            "mean drawdown {:.4} exceeded the {:.4} ceiling (worst instrument {:.4})",
            pooled.mean_max_drawdown, criteria.max_drawdown, pooled.worst_max_drawdown
        ));
        Verdict::NotSupported
    } else {
        reasons.push(format!(
            "beat buy-and-hold by {:.4} on average, on {} of {} instruments, over {} trades",
            pooled.mean_excess_return,
            pooled.beat_benchmark,
            pooled.instruments,
            pooled.total_trades
        ));
        Verdict::Supported
    };

    // Majority-of-one is a real caveat even when the numbers clear the bar: a
    // mean carried by a single instrument is not the cross-sectional evidence
    // a panel was run to get.
    if pooled.instruments > 1 && pooled.beat_benchmark * 2 <= pooled.instruments {
        reasons.push(format!(
            "the average is not consistent — only {} of {} instruments beat their benchmark",
            pooled.beat_benchmark, pooled.instruments
        ));
    }
    if !failures.is_empty() {
        reasons.push(format!("{} runs did not complete", failures.len()));
    }

    (verdict, reasons)
}

/// One run: the template, for this instrument, with these parameters, over
/// this window.
fn variant(
    template: &Experiment,
    instrument: &str,
    params: &BTreeMap<String, f64>,
    window: DateRange,
    phase: &str,
) -> Experiment {
    let mut merged = template.strategy.params.clone();
    for (name, value) in params {
        merged.insert(name.clone(), *value);
    }
    let signature = params
        .iter()
        .map(|(name, value)| format!("{name}{value}"))
        .collect::<Vec<_>>()
        .join("-");

    Experiment {
        id: ExperimentId(format!("{}-{instrument}-{phase}-{signature}", template.id)),
        instrument: instrument.to_owned(),
        window,
        strategy: StrategySpec {
            name: template.strategy.name.clone(),
            params: merged,
        },
        ..template.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(instrument: &str, excess: f64, drawdown: f64, trades: u32) -> InstrumentOutcome {
        let metrics = |total_return: f64| Metrics {
            total_return,
            cagr: total_return,
            max_drawdown: drawdown,
            volatility: 0.1,
            sharpe: Some(1.0),
            sortino: Some(1.2),
            calmar: Some(0.9),
            trades,
        };
        InstrumentOutcome {
            instrument: instrument.to_owned(),
            strategy: metrics(excess),
            benchmark: metrics(0.0),
            excess_return: excess,
        }
    }

    /// A panel whose members were never measured against each other, so the
    /// breadth line stays out of tests that are about something else.
    fn unmeasured() -> crate::Breadth {
        crate::breadth::measure(&[])
    }

    fn selection(survived: bool) -> Selection {
        Selection {
            trials: 9,
            best_sharpe: 1.0,
            expected_best_under_null: Some(if survived { 0.5 } else { 2.0 }),
            survived_deflation: survived,
            scored: Vec::new(),
        }
    }

    #[test]
    fn trades_pool_so_a_panel_can_reach_a_verdict_one_instrument_cannot() {
        let outcomes = vec![
            outcome("A.SIM", 0.10, 0.05, 12),
            outcome("B.SIM", 0.08, 0.06, 11),
            outcome("C.SIM", 0.09, 0.04, 10),
        ];
        let pooled = pool(&outcomes);

        assert_eq!(pooled.total_trades, 33, "each alone is short of the 30 bar");
        assert_eq!(pooled.beat_benchmark, 3);
        let (verdict, _) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        assert_eq!(verdict, Verdict::Supported);
    }

    #[test]
    fn a_panel_that_still_lacks_trades_is_inconclusive_not_refuted() {
        let pooled = pool(&[outcome("A.SIM", 0.5, 0.02, 3)]);
        let (verdict, reasons) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        assert_eq!(verdict, Verdict::Inconclusive);
        assert!(reasons[0].contains("trades"), "{reasons:?}");
    }

    #[test]
    fn failing_deflation_refuses_the_panel_however_well_it_pooled() {
        let outcomes = vec![
            outcome("A.SIM", 0.30, 0.02, 40),
            outcome("B.SIM", 0.30, 0.02, 40),
        ];
        let (verdict, reasons) = judge(
            &pool(&outcomes),
            &selection(false),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        assert_eq!(verdict, Verdict::NotSupported);
        assert!(reasons[0].contains("no-skill"), "{reasons:?}");
    }

    #[test]
    fn an_average_carried_by_one_instrument_is_called_out() {
        // One big winner, three losers: the mean clears the bar, the panel
        // does not actually support the idea.
        let outcomes = vec![
            outcome("A.SIM", 2.00, 0.02, 20),
            outcome("B.SIM", -0.10, 0.02, 20),
            outcome("C.SIM", -0.10, 0.02, 20),
            outcome("D.SIM", -0.10, 0.02, 20),
        ];
        let pooled = pool(&outcomes);
        assert!(pooled.mean_excess_return > 0.0);
        assert_eq!(pooled.beat_benchmark, 1);

        let (verdict, reasons) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        assert_eq!(verdict, Verdict::Supported, "the numbers do clear the bar");
        assert!(
            reasons.iter().any(|r| r.contains("not consistent")),
            "but the inconsistency must be stated: {reasons:?}"
        );
    }

    #[test]
    fn the_worst_drawdown_survives_the_averaging_that_hides_it() {
        let pooled = pool(&[
            outcome("A.SIM", 0.1, 0.02, 20),
            outcome("B.SIM", 0.1, 0.40, 20),
        ]);
        assert!((pooled.mean_max_drawdown - 0.21).abs() < 1e-12);
        assert!((pooled.worst_max_drawdown - 0.40).abs() < 1e-12);
    }
}
