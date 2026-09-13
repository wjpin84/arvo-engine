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
    ///
    /// Read against [`Self::distinct`], not [`Self::instruments`]. The two
    /// differ when a panel holds one security more than once.
    pub beat_benchmark: usize,
    /// Distinct securities among [`Self::instruments`].
    ///
    /// A panel pools its members as separate evidence, and two vendors' copies
    /// of the same stock are not separate evidence about anything. Fetching
    /// `PG` from both sources and running a panel over the pair reported "beat
    /// benchmark on 4 of 6" for what was two of three — four independent
    /// confirmations where there were two.
    ///
    /// `breadth` already corrects the *certainty* of the pooled average from
    /// correlation. It cannot correct a count, and a count is what consistency
    /// is, so this is the count's own version of the same fix.
    ///
    /// Same security means same ticker: an instrument id is `SYMBOL.VENUE` by
    /// convention, and `PG.YF` and `PG.RH` are one company however they were
    /// filed. `0` for a panel recorded before this was measured, which reads
    /// as unknown rather than as "no distinct securities".
    #[serde(default)]
    pub distinct: usize,
    /// Distinct securities where *every* copy beat its own benchmark.
    ///
    /// The numerator [`Self::distinct`] is the denominator for. Comparing
    /// [`Self::beat_benchmark`] against `distinct` would be worse than the
    /// original bug: duplicating a winner raises the row count while the
    /// security count stands still, so the ratio improves and double-counting
    /// starts *helping* a panel clear its consistency bar.
    ///
    /// "Every copy" rather than "any copy" because a security whose two
    /// vendors disagree about whether it beat is not a confirmation of
    /// anything.
    ///
    /// An earlier version of this note claimed that case was demonstrated on
    /// real data DASH that PG and JNJ produced opposite-signed fold margins
    /// from two sources. That was measured across different windows, and a
    /// controlled re-run over one window disagrees: the two vendors gave the
    /// same sign on all three instruments tested, the same fold counts, and
    /// stitched returns matching to two decimals. The sign flip was six extra
    /// years of history, not the data.
    ///
    /// The rule stands anyway, on the weaker and true claim: the margins do
    /// not match either. PG came out -0.0175 against -0.0108 on the same
    /// window, which is the same conclusion reached with meaningfully
    /// different confidence, and a disagreement about direction remains the
    /// case this is here for whether or not it has been seen yet.
    ///
    /// `0` for a panel recorded before this was measured, which reads as
    /// unknown rather than as nothing beating.
    #[serde(default)]
    pub distinct_beat: usize,
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
    /// What one account holding every member at equal weight would have done.
    ///
    /// [`PooledOutcome`] averages the members; this combines them. The two
    /// differ in the place that decides whether a rule is usable — falls that
    /// did not coincide hurt a book less than they hurt its average member,
    /// and no mean of drawdowns can say so because it averages numbers that
    /// never happened together.
    ///
    /// `None` for a single-instrument panel: one instrument is not a book.
    /// `default` because it is a persisted format.
    #[serde(default)]
    pub book: Option<Metrics>,
    /// Everything it would take to run this panel again.
    ///
    /// `Experiment` states the rule this obeys: the field list *is* the
    /// reproducibility contract, and if a run's output can change without one
    /// of these changing, the record is incomplete. A panel is built from
    /// experiments and did not obey it. The dataset, the winning parameters
    /// and every outcome were kept; the template, the instrument list, the
    /// grid and the split fraction were not, so nobody — including
    /// `replay` — could re-derive the run that produced them.
    ///
    /// Stored whole rather than as four fields because the study *is* the
    /// input. Four fields can be added to three at a time.
    ///
    /// `None` for a panel recorded before this existed. Those genuinely
    /// cannot be replayed, and saying so is the honest answer rather than a
    /// silent pass.
    #[serde(default)]
    pub study: Option<PanelStudy>,
    /// The bar this verdict was judged against.
    ///
    /// A verdict is a comparison, and half of it was being thrown away. The
    /// record said `NotSupported` and nothing in it said what the result had
    /// needed to clear, so the one sentence that matters — *why* — could
    /// not be re-derived from the finding at all.
    ///
    /// It also decays silently. `EvaluationCriteria::default` is thirty
    /// trades, no negative excess return and a thirty percent drawdown
    /// ceiling; change any of those and every stored verdict means something
    /// different from what it says, with nothing to reveal the change. A
    /// single study already keeps its criteria, and these are built from
    /// studies.
    ///
    /// `Option` rather than `serde(default)` on the bare type, deliberately.
    /// Defaulting would hand an old record today's bar and let it claim that
    /// is what it was judged by, which is the exact substitution this exists
    /// to prevent. `None` means not recorded, and says so.
    #[serde(default)]
    pub criteria: Option<EvaluationCriteria>,
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
        prior_trials: 0,
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
    // The members were each run with the whole account behind them, so this
    // rescales rather than re-simulates: it cannot show capital contention
    // between them. See `crate::book` for the rest of what it assumes.
    let book = crate::book::combine(study.template.starting_cash, &curves).and_then(|curve| {
        Metrics::from_curve(
            &curve,
            pooled.total_trades,
            study.template.interval.periods_per_year(),
        )
    });
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
        book,
        study: Some(study.clone()),
        criteria: Some(*criteria),
        failures,
        verdict,
        reasons,
    })
}

/// How many distinct securities a set of outcomes covers.
///
/// By ticker, because an instrument id is `SYMBOL.VENUE` and the venue says
/// where a copy came from rather than what it is. An id with no venue at all
/// counts as itself.
fn distinct_securities(
    outcomes: &[InstrumentOutcome],
) -> std::collections::BTreeMap<&str, Vec<f64>> {
    let mut by_ticker: std::collections::BTreeMap<&str, Vec<f64>> =
        std::collections::BTreeMap::new();
    for outcome in outcomes {
        let ticker = outcome
            .instrument
            .rsplit_once('.')
            .map_or(outcome.instrument.as_str(), |(ticker, _)| ticker);
        by_ticker.entry(ticker).or_default().push(outcome.excess_return);
    }
    by_ticker
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
        distinct: distinct_securities(outcomes).len(),
        distinct_beat: distinct_securities(outcomes)
            .into_iter()
            .filter(|(_, results)| results.iter().all(|excess| *excess > 0.0))
            .count(),
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
pub(crate) const OVERSTATEMENT_WORTH_SAYING: f64 = 1.25;

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
                "these {} instruments behave like {effective:.1} independent ones (average \
                 correlation {:.2}), so the pooled average is about {overstatement:.1}x less \
                 certain than its instrument count suggests",
                breadth.instruments.len(),
                breadth.mean_correlation.unwrap_or_default(),
            ));
        }
    }

    // Against distinct securities rather than rows. A panel holding one stock
    // twice would otherwise clear a consistency bar on the strength of
    // counting it twice, which is the arithmetic equivalent of asking the same
    // person the same question and calling it a second opinion.
    //
    // `distinct` is zero on a panel recorded before it was measured; falling
    // back to the row count keeps those judged the way they were.
    let (winners, independent) = if pooled.distinct == 0 {
        (pooled.beat_benchmark, pooled.instruments)
    } else {
        (pooled.distinct_beat, pooled.distinct)
    };

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
            "beat buy-and-hold by {:.4} on average, on {winners} of {independent} securities, \
             over {} trades",
            pooled.mean_excess_return,
            pooled.total_trades
        ));
        Verdict::Supported
    };

    // Majority-of-one is a real caveat even when the numbers clear the bar: a
    // mean carried by a single instrument is not the cross-sectional evidence
    // a panel was run to get.
    if independent > 1 && winners * 2 <= independent {
        reasons.push(format!(
            "the average is not consistent — only {} of {} instruments beat their benchmark",
            winners, independent
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
            psr: None,
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
            prior_trials: 0,
            scored: Vec::new(),
        }
    }

    #[test]
    fn a_stored_verdict_can_be_re_derived_from_the_record_that_carries_it() {
        // The property the criteria field exists for: a finding explains
        // itself. Everything the re-derivation below touches is read back off
        // the record rather than from the locals that built it, because the
        // question is whether the *record* is sufficient, and comparing two
        // calls on the same variables would answer a different and much
        // easier one.
        let outcomes = vec![
            outcome("A.SIM", 0.10, 0.05, 12),
            outcome("B.SIM", 0.08, 0.06, 11),
            outcome("C.SIM", 0.09, 0.04, 10),
        ];
        let criteria = EvaluationCriteria::default();
        let pooled = pool(&outcomes);
        let breadth = unmeasured();
        let (verdict, reasons) = judge(&pooled, &selection(true), &criteria, &[], &breadth);

        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
        let stored = PanelEvidence {
            hypothesis: HypothesisId::from("h"),
            dataset: crate::DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
            in_sample: DateRange::new(day(1), day(4)).expect("ordered"),
            out_of_sample: DateRange::new(day(5), day(9)).expect("ordered"),
            selected_params: BTreeMap::new(),
            selection: selection(true),
            per_instrument: outcomes,
            pooled,
            breadth: Some(breadth),
            book: None,
            study: None,
            criteria: Some(criteria),
            failures: Vec::new(),
            verdict,
            reasons,
        };

        // Nothing from above: only what a reader opening the file would have.
        let recorded_criteria = stored
            .criteria
            .expect("a panel written by this build records its bar");
        let recorded_breadth = stored.breadth.clone().expect("and its breadth");
        let (again, again_reasons) = judge(
            &stored.pooled,
            &stored.selection,
            &recorded_criteria,
            &stored.failures,
            &recorded_breadth,
        );

        assert_eq!(again, stored.verdict, "the record must explain its own verdict");
        assert_eq!(again_reasons, stored.reasons);
    }

    #[test]
    fn the_same_result_under_a_harder_bar_is_a_different_verdict() {
        // Why defaulting an unrecorded value would be a substitution rather
        // than a convenience: the identical panel changes answer when the bar
        // moves, so handing an old record today's bar lets it claim a
        // judgement nobody made.
        let outcomes = vec![
            outcome("A.SIM", 0.10, 0.05, 12),
            outcome("B.SIM", 0.08, 0.06, 11),
            outcome("C.SIM", 0.09, 0.04, 10),
        ];
        let pooled = pool(&outcomes);

        let (lenient, _) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        let (strict, _) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria {
                min_trades: 500,
                ..EvaluationCriteria::default()
            },
            &[],
            &unmeasured(),
        );

        assert_eq!(lenient, Verdict::Supported);
        assert_eq!(strict, Verdict::Inconclusive);
        assert_ne!(lenient, strict, "the bar is half of the verdict");
    }

    #[test]
    fn two_vendors_copies_of_one_stock_are_one_security() {
        // An instrument id is `SYMBOL.VENUE`; the venue says where a copy came
        // from, not what it is.
        let outcomes = vec![
            outcome("PG.YF", 0.02, 0.05, 34),
            outcome("PG.RH", 0.02, 0.05, 34),
            outcome("AAPL.YF", 0.02, 0.05, 30),
            outcome("AAPL.RH", 0.02, 0.05, 30),
            outcome("JNJ.YF", -0.01, 0.05, 31),
            outcome("JNJ.RH", -0.01, 0.05, 31),
        ];
        let pooled = pool(&outcomes);
        assert_eq!(pooled.instruments, 6, "six rows ran");
        assert_eq!(pooled.distinct, 3, "of three companies");
    }

    #[test]
    fn consistency_is_judged_on_securities_rather_than_rows() {
        // Four of six looks like a majority and is two of three. Counting the
        // same stock twice to clear a consistency bar is the arithmetic
        // equivalent of asking one person twice and calling it a second
        // opinion.
        let outcomes = vec![
            outcome("PG.YF", 0.02, 0.05, 34),
            outcome("PG.RH", 0.02, 0.05, 34),
            outcome("AAPL.YF", 0.02, 0.05, 30),
            outcome("AAPL.RH", 0.02, 0.05, 30),
            outcome("JNJ.YF", -0.01, 0.05, 31),
            outcome("JNJ.RH", -0.01, 0.05, 31),
        ];
        let pooled = pool(&outcomes);
        assert_eq!(pooled.beat_benchmark, 4);

        let (_, reasons) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        // Four of six clears `4 * 2 > 6`; four of three clears it too, so the
        // gate does not fire either way here. What must not happen is the
        // message claiming six.
        assert!(
            !reasons.iter().any(|reason| reason.contains("of 6 instruments")),
            "a reason must not count rows as instruments: {reasons:?}"
        );
    }

    #[test]
    fn a_duplicated_winner_does_not_vote_twice() {
        // Two rows of one winner and two distinct losers. Counting rows makes
        // that "2 of 4 beat", a near-majority; counting securities makes it
        // one of three, which is what happened.
        //
        // Getting this wrong is worse than leaving it alone, and the first
        // attempt did: comparing the *row* count of winners against the
        // *security* count of instruments means duplicating a winner improves
        // the ratio, so double-counting starts helping a panel clear its own
        // consistency bar.
        let outcomes = vec![
            outcome("PG.YF", 0.05, 0.05, 34),
            outcome("PG.RH", 0.05, 0.05, 34),
            outcome("AAPL.RH", -0.02, 0.05, 30),
            outcome("JNJ.RH", -0.02, 0.05, 31),
        ];
        let pooled = pool(&outcomes);
        assert_eq!(pooled.beat_benchmark, 2, "two rows beat");
        assert_eq!(pooled.distinct, 3, "of three securities");
        assert_eq!(pooled.distinct_beat, 1, "one of which beat");

        let (_, reasons) = judge(
            &pooled,
            &selection(true),
            &EvaluationCriteria::default(),
            &[],
            &unmeasured(),
        );
        assert!(
            reasons.iter().any(|reason| reason.contains("1 of 3")),
            "consistency is counted in securities: {reasons:?}"
        );
        assert!(
            !reasons.iter().any(|reason| reason.contains("2 of 4")),
            "and never in rows: {reasons:?}"
        );
    }

    #[test]
    fn a_security_whose_two_sources_disagree_is_not_a_confirmation() {
        // A security whose copies disagree about whether it beat has confirmed
        // nothing, so it is not counted as having beaten.
        //
        // Not yet seen on real data: a controlled comparison over one window
        // had two vendors agreeing on sign for every instrument tested. They
        // disagreed on magnitude, which is the near miss this guards against.
        let outcomes = vec![
            outcome("PG.YF", 0.05, 0.05, 34),
            outcome("PG.RH", -0.01, 0.05, 34),
            outcome("AAPL.RH", 0.04, 0.05, 30),
        ];
        let pooled = pool(&outcomes);
        assert_eq!(pooled.beat_benchmark, 2, "two rows beat");
        assert_eq!(pooled.distinct, 2);
        assert_eq!(
            pooled.distinct_beat, 1,
            "only AAPL; PG's sources contradict each other"
        );
    }

    #[test]
    fn an_instrument_with_no_venue_counts_as_itself() {
        let outcomes = vec![outcome("AAPL", 0.02, 0.05, 10), outcome("MSFT", 0.02, 0.05, 10)];
        assert_eq!(pool(&outcomes).distinct, 2);
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
