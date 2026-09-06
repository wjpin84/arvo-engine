//! Re-selecting as time moves, instead of choosing once.
//!
//! A single in-sample/out-of-sample split answers "did this configuration hold
//! on data it was not chosen on". That is worth knowing and it is not the
//! question anyone actually has, which is "does *choosing this way* work". One
//! split cannot tell the two apart: a configuration that happened to hold once
//! and a procedure that reliably finds good configurations produce the same
//! evidence.
//!
//! Walk-forward asks the second question. Select on a window, judge on the
//! window after it, move forward, select again. What is being tested is the
//! whole loop, and the winner is allowed — expected — to change.
//!
//! # Two things it buys that a single split cannot
//!
//! **Parameter stability.** If the best `fast` is 5, then 20, then 10, then
//! 5 again, there is no stable edge to find and the search is fitting noise.
//! A single split cannot show this because it only ever selects once. See
//! [`AxisStability`].
//!
//! **A much longer out-of-sample record.** A single split judges one slice;
//! this judges everything after the first selection window. On the fixture
//! data that is sixteen years rather than six.
//!
//! # Two things that cost trades, and are not bugs
//!
//! Both were found by running this on real data, and both understate a rule
//! rather than flatter it — which is the safe direction, but only if the
//! reader knows.
//!
//! **Every fold starts cold.** A fold's out-of-sample period is an independent
//! backtest, so a rule with a 120-bar slow average spends the first 120 bars
//! of each fold warming up and cannot trade in them. At a one-year step that
//! is roughly half of every fold. The stitched record is therefore *not* what
//! running the selected parameters continuously would have produced: it is
//! what re-deploying them from flat each period produces, which is a real
//! thing and a different one. **Choose a step long relative to the warm-up.**
//!
//! ponytail: the fix is to run each fold from `start - warmup` and count only
//! from `start`, which needs the engine to report a warm-up length and needs
//! the ledger and curve trimmed consistently — a trade opened in warm-up and
//! closed in the judged period belongs to neither cleanly. Worth doing when
//! the warm-up is a large share of the step; the honest interim is to say so
//! and to report [`WalkForwardEvidence::folds_without_trades`].
//!
//! **A fold that ends holding never realises.** The position is marked to
//! market in that fold's curve but is not a closed round trip, so sixteen
//! folds can leave sixteen unrealised positions and a trade count far below
//! what the same span traded continuously.
//!
//! # What it does not buy
//!
//! It is not a defence against having tried walk-forward with twenty different
//! window lengths and reported the best. The deflation check inside each fold
//! is about the grid, not about *this* procedure being one of many a person
//! could have run. That outer search is not modelled anywhere and would have
//! to be counted by hand.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::evaluation::EvaluationCriteria;
use crate::family::{run_split, FamilyEvidence, ParameterGrid};
use crate::{
    DateRange, EquityPoint, Experiment, HypothesisId, Metrics, SimulationError,
    SimulationProvider, TradeStats, Verdict,
};

/// Below this many folds it is a split with extra steps.
///
/// Three is not a comfortable number of folds — it is the fewest from which
/// "the selection changed" and "the selection held" are distinguishable at
/// all.
const MIN_FOLDS: usize = 3;

/// A rolling selection procedure to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForward {
    pub hypothesis: HypothesisId,
    /// Carries the whole span in its window; each fold narrows it.
    pub template: Experiment,
    pub grid: ParameterGrid,
    /// Calendar days each selection looks at.
    pub in_sample_days: i64,
    /// Calendar days judged before re-selecting. Also the stride.
    pub step_days: i64,
    /// Expanding rather than sliding: every selection sees all history back to
    /// the start, rather than a fixed-width recent window.
    ///
    /// Neither is right in general, and the difference is a claim about the
    /// world. Anchored assumes old data still describes the current market;
    /// sliding assumes it stops applying and that the rule should re-learn.
    /// Stated in the record because a result is not interpretable without it.
    pub anchored: bool,
}

impl WalkForward {
    /// The windows this procedure will run, in order.
    ///
    /// Pure, and separate from running them, because the schedule is the part
    /// most easily got wrong — an off-by-one here silently overlaps a
    /// selection window with the period it is judged on, which is look-ahead
    /// bias wearing the costume of a walk-forward.
    #[must_use]
    pub fn folds(&self) -> Vec<(DateRange, DateRange)> {
        let mut out = Vec::new();
        if self.in_sample_days < 1 || self.step_days < 1 {
            return out;
        }

        let start = self.template.window.from;
        let end = self.template.window.to;
        let mut selection_end = start + chrono::Duration::days(self.in_sample_days - 1);

        while selection_end < end {
            let judge_from = selection_end + chrono::Duration::days(1);
            let judge_to = (judge_from + chrono::Duration::days(self.step_days - 1)).min(end);

            let selection_from = if self.anchored {
                start
            } else {
                selection_end - chrono::Duration::days(self.in_sample_days - 1)
            };

            // A trailing stub shorter than a full step is dropped rather than
            // judged. Keeping it would weight the last few days of the span
            // exactly as heavily as a full fold in every count below.
            if judge_to < judge_from + chrono::Duration::days(self.step_days - 1) {
                break;
            }

            if let (Ok(in_sample), Ok(out_of_sample)) = (
                DateRange::new(selection_from, selection_end),
                DateRange::new(judge_from, judge_to),
            ) {
                out.push((in_sample, out_of_sample));
            }
            selection_end = judge_to;
        }
        out
    }
}

/// How much one parameter moved across the folds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxisStability {
    pub axis: String,
    /// Distinct values the selection landed on. One means the search found the
    /// same answer every time.
    pub distinct: usize,
    /// The value chosen most often, and the share of folds that chose it.
    ///
    /// The number to read. A modal share near 1 says there is something stable
    /// to find; near `1/distinct` says the search is picking at random and the
    /// parameter is doing no work.
    pub modal: f64,
    pub modal_share: f64,
}

/// What a walk-forward run showed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardEvidence {
    pub hypothesis: HypothesisId,
    pub template: Experiment,
    /// Every fold, each a complete study in its own right.
    pub folds: Vec<FamilyEvidence>,
    /// The folds' out-of-sample runs, end to end.
    pub combined: Metrics,
    /// Buy-and-hold over the same stitched period, for the same comparison a
    /// single study makes. Without it the combined return mostly measures
    /// whether the market went up.
    pub benchmark: Metrics,
    pub excess_return: f64,
    pub combined_curve: Vec<EquityPoint>,
    pub benchmark_curve: Vec<EquityPoint>,
    pub combined_trades: TradeStats,
    pub stability: Vec<AxisStability>,
    /// Folds whose in-sample winner beat what a no-skill search of that size
    /// would produce. A procedure whose selections are noise in most folds has
    /// not been shown to select.
    pub folds_surviving_deflation: usize,
    /// Folds in which the selected configuration never opened a position.
    ///
    /// The observable symptom of a step too short for the rule's warm-up: an
    /// empty fold is not evidence the rule does nothing, it is evidence the
    /// fold was too short to let it start. Counted rather than inferred,
    /// because "0.00%" in a fold reads identically to a rule that traded and
    /// broke even.
    pub folds_without_trades: usize,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

/// Runs the whole procedure.
///
/// # Errors
///
/// Returns [`SimulationError::Rejected`] if the span yields fewer than
/// [`MIN_FOLDS`] folds, or if every fold failed to run.
pub fn run_walk_forward(
    provider: &dyn SimulationProvider,
    plan: &WalkForward,
    criteria: &EvaluationCriteria,
) -> Result<WalkForwardEvidence, SimulationError> {
    let schedule = plan.folds();
    if schedule.len() < MIN_FOLDS {
        return Err(SimulationError::Rejected(format!(
            "{}..={} yields {} folds at {} in-sample and {} step days; fewer than {MIN_FOLDS} is \
             a single split with extra steps, and cannot show whether the selection is stable",
            plan.template.window.from,
            plan.template.window.to,
            schedule.len(),
            plan.in_sample_days,
            plan.step_days,
        )));
    }

    let mut folds = Vec::with_capacity(schedule.len());
    let mut failures = Vec::new();
    for (in_sample, out_of_sample) in schedule {
        match run_split(
            provider,
            &plan.hypothesis,
            &plan.template,
            &plan.grid,
            in_sample,
            out_of_sample,
            criteria,
        ) {
            Ok(evidence) => folds.push(evidence),
            // A fold that could not run is recorded, not skipped silently. A
            // procedure that worked in four periods out of ten is a different
            // claim from one that worked in four out of four.
            Err(err) => failures.push(format!("{} → {}: {err}", in_sample.from, out_of_sample.to)),
        }
    }

    if folds.len() < MIN_FOLDS {
        return Err(SimulationError::Rejected(format!(
            "only {} of {} folds ran ({})",
            folds.len(),
            folds.len() + failures.len(),
            failures.join("; ")
        )));
    }

    let combined_curve = stitch(
        plan.template.starting_cash,
        folds
            .iter()
            .map(|fold| fold.out_of_sample_evidence.evaluation.strategy_curve.as_slice()),
    );
    let benchmark_curve = stitch(
        plan.template.starting_cash,
        folds
            .iter()
            .map(|fold| fold.out_of_sample_evidence.evaluation.benchmark_curve.as_slice()),
    );
    let combined_trades = TradeStats::combine(
        folds
            .iter()
            .map(|fold| &fold.out_of_sample_evidence.evaluation.strategy_trades),
    );

    let periods = plan.template.interval.periods_per_year();
    let measure = |curve: &[EquityPoint], trades: u32, what: &str| {
        Metrics::from_curve(curve, trades, periods).ok_or_else(|| {
            SimulationError::Rejected(format!(
                "the stitched {what} record has {} points, too few to evaluate",
                curve.len()
            ))
        })
    };
    let combined = measure(&combined_curve, combined_trades.closed, "strategy")?;
    let benchmark = measure(&benchmark_curve, combined_trades.closed, "benchmark")?;
    let excess_return = combined.total_return - benchmark.total_return;

    let stability = stability(&folds);
    let folds_surviving_deflation = folds
        .iter()
        .filter(|fold| fold.selection.survived_deflation)
        .count();
    let folds_without_trades = folds
        .iter()
        .filter(|fold| fold.out_of_sample_evidence.evaluation.strategy.trades == 0)
        .count();

    let mut reasons = Vec::new();
    let verdict = judge(
        &folds,
        folds_surviving_deflation,
        folds_without_trades,
        &combined,
        excess_return,
        &combined_trades,
        &stability,
        criteria,
        &mut reasons,
    );
    reasons.extend(failures);

    Ok(WalkForwardEvidence {
        hypothesis: plan.hypothesis.clone(),
        template: plan.template.clone(),
        folds,
        combined,
        benchmark,
        excess_return,
        combined_curve,
        benchmark_curve,
        combined_trades,
        stability,
        folds_surviving_deflation,
        folds_without_trades,
        verdict,
        reasons,
    })
}

#[allow(clippy::too_many_arguments, reason = "one verdict, stated in one place")]
fn judge(
    folds: &[FamilyEvidence],
    surviving: usize,
    empty: usize,
    combined: &Metrics,
    excess_return: f64,
    trades: &TradeStats,
    stability: &[AxisStability],
    criteria: &EvaluationCriteria,
    reasons: &mut Vec<String>,
) -> Verdict {
    // Order matters and is the same order a single study uses: too little
    // evidence first, then whether the selection was noise, then the result.
    // Said before the trade-count objection, because it is usually the cause
    // of it and the two are easy to confuse: a rule that never got to start is
    // not a rule that had nothing to say.
    if empty > folds.len() / 2 {
        reasons.push(format!(
            "the selected configuration never opened a position in {empty} of {} folds — the \
             step is probably too short for its warm-up, so each fold spends most of itself \
             unable to trade",
            folds.len()
        ));
    }

    if trades.closed < criteria.min_trades {
        reasons.push(format!(
            "{} round trips across {} folds is below the {} needed to read anything into the \
             result{}",
            trades.closed,
            folds.len(),
            criteria.min_trades,
            if trades.still_open > 0 {
                format!(
                    "; {} more folds ended still holding, and an unrealised position is not a \
                     round trip",
                    trades.still_open
                )
            } else {
                String::new()
            }
        ));
        return Verdict::Inconclusive;
    }

    if surviving == 0 {
        reasons.push(format!(
            "no fold's winner beat what a no-skill search of that size would produce; the \
             procedure selected noise in all {} of them",
            folds.len()
        ));
        return Verdict::NotSupported;
    }

    // Not a hard bar, because a rule whose parameters wander can still make
    // money and the honest thing is to say so rather than to refuse. It is a
    // reason attached to whatever verdict follows.
    if let Some(worst) = stability
        .iter()
        .min_by(|a, b| a.modal_share.total_cmp(&b.modal_share))
        .filter(|worst| worst.modal_share < 0.5)
    {
        reasons.push(format!(
            "{:?} landed on {} different values across {} folds, most often {} in only {:.0}% of \
             them — there is no stable setting for it to find",
            worst.axis,
            worst.distinct,
            folds.len(),
            worst.modal,
            worst.modal_share * 100.0
        ));
    }

    if excess_return < criteria.min_excess_return {
        reasons.push(format!(
            "excess return {excess_return:.4} over buy-and-hold across the stitched \
             out-of-sample record did not clear {:.4}",
            criteria.min_excess_return
        ));
        return Verdict::NotSupported;
    }

    if combined.max_drawdown > criteria.max_drawdown {
        reasons.push(format!(
            "drawdown {:.4} across the stitched record exceeded the {:.4} ceiling, so the excess \
             return was not obtainable in practice",
            combined.max_drawdown, criteria.max_drawdown
        ));
        return Verdict::NotSupported;
    }

    reasons.push(format!(
        "beat buy-and-hold by {excess_return:.4} over {} round trips across {} folds, {surviving} \
         of which selected better than chance; worst drawdown {:.4}",
        trades.closed,
        folds.len(),
        combined.max_drawdown
    ));
    Verdict::Supported
}

/// Joins the folds' out-of-sample curves into one continuous record.
///
/// **Additive, not compounding.** Each fold's curve opens at the starting
/// balance, so the join carries forward the *difference* rather than the
/// ratio. That matches how the strategies actually size: risk is a fraction of
/// *starting* capital, fixed-fractional rather than compounding, so a profit
/// in fold one does not increase the stake in fold two. Compounding the join
/// while the sizing does not compound would inflate every later fold.
fn stitch<'a>(
    starting_cash: f64,
    curves: impl Iterator<Item = &'a [EquityPoint]>,
) -> Vec<EquityPoint> {
    let mut out = Vec::new();
    let mut running = starting_cash;

    for curve in curves {
        let Some(opening) = curve.first() else {
            continue;
        };
        if out.is_empty() {
            out.push(EquityPoint {
                at: opening.at,
                equity: running,
            });
        }
        for point in &curve[1..] {
            out.push(EquityPoint {
                at: point.at,
                equity: running + (point.equity - opening.equity),
            });
        }
        if let Some(last) = curve.last() {
            running += last.equity - opening.equity;
        }
    }
    out
}

/// How often each axis landed on the same value.
fn stability(folds: &[FamilyEvidence]) -> Vec<AxisStability> {
    let mut counts: BTreeMap<String, BTreeMap<String, (f64, usize)>> = BTreeMap::new();
    for fold in folds {
        for (axis, value) in &fold.selected.strategy.params {
            counts
                .entry(axis.clone())
                .or_default()
                // Keyed by the value's own spelling: `f64` is not `Ord` and
                // not sensibly hashable, and the values here come from a grid
                // written by hand, so their text form is exact.
                .entry(value.to_string())
                .or_insert((*value, 0))
                .1 += 1;
        }
    }

    let total = folds.len() as f64;
    counts
        .into_iter()
        .filter_map(|(axis, values)| {
            let distinct = values.len();
            let (modal, count) = values.into_values().max_by_key(|(_, count)| *count)?;
            Some(AxisStability {
                axis,
                distinct,
                modal,
                modal_share: count as f64 / total,
            })
        })
        // Parameters the grid does not vary are constant by construction and
        // saying they were stable would be flattering noise into a finding.
        .filter(|axis| axis.distinct > 1 || axis.modal_share < 1.0)
        .collect()
}

impl TradeStats {
    /// Adds several runs' statistics together as if they were one record.
    ///
    /// Every field is additive or reconstructible from additive parts, which
    /// is why this is exact rather than an approximation: gross profit is
    /// `average_win × wins`, and summing those across folds gives the same
    /// number a single pass over the concatenated ledgers would.
    #[must_use]
    pub fn combine<'a>(parts: impl Iterator<Item = &'a Self>) -> Self {
        let mut out = Self::default();
        let mut gross_win = 0.0;
        let mut gross_loss = 0.0;
        let mut holding = 0.0;

        for part in parts {
            out.closed += part.closed;
            out.still_open += part.still_open;
            out.wins += part.wins;
            out.losses += part.losses;
            out.total_commission += part.total_commission;
            out.signal_exits += part.signal_exits;
            out.stop_exits += part.stop_exits;
            gross_win += part.average_win.unwrap_or_default() * f64::from(part.wins);
            gross_loss += part.average_loss.unwrap_or_default() * f64::from(part.losses);
            holding += part.average_holding_secs.unwrap_or_default() * f64::from(part.closed);
            out.largest_win = pick(out.largest_win, part.largest_win, f64::max);
            out.largest_loss = pick(out.largest_loss, part.largest_loss, f64::min);
        }

        if out.closed > 0 {
            let closed = f64::from(out.closed);
            out.win_rate = Some(f64::from(out.wins) / closed);
            out.average_holding_secs = Some(holding / closed);
        }
        if out.wins > 0 {
            out.average_win = Some(gross_win / f64::from(out.wins));
        }
        if out.losses > 0 {
            out.average_loss = Some(gross_loss / f64::from(out.losses));
        }
        if gross_loss > 0.0 {
            out.profit_factor = Some(gross_win / gross_loss);
        }
        out
    }
}

fn pick(left: Option<f64>, right: Option<f64>, choose: fn(f64, f64) -> f64) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(choose(left, right)),
        (found, None) | (None, found) => found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, ExitReason, Trade};

    fn date(year: i32, month: u32, day: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(year, month, day).expect("valid")
    }

    fn experiment(window: DateRange) -> Experiment {
        Experiment {
            id: crate::ExperimentId::from("wf"),
            hypothesis: HypothesisId::from("h"),
            instrument: "AAPL.NASDAQ".to_owned(),
            window,
            interval: arvo_data::BarInterval::DAILY,
            dataset: crate::DatasetRef {
                id: "fixture".to_owned(),
                version: "1".to_owned(),
            },
            strategy: crate::StrategySpec {
                name: "sma_cross".to_owned(),
                params: BTreeMap::new(),
            },
            costs: crate::CostModel::proportional(1.0, 1.0),
            risk: crate::RiskModel::default(),
            starting_cash: 100_000.0,
            seed: 1,
        }
    }

    fn plan(anchored: bool, in_sample_days: i64, step_days: i64) -> WalkForward {
        WalkForward {
            hypothesis: HypothesisId::from("h"),
            template: experiment(
                DateRange::new(date(2024, 1, 1), date(2024, 12, 31)).expect("ordered"),
            ),
            grid: ParameterGrid::new().axis("fast", vec![5.0, 10.0]),
            in_sample_days,
            step_days,
            anchored,
        }
    }

    #[test]
    fn a_selection_window_never_touches_the_period_it_is_judged_on() {
        // The failure this schedule exists to prevent: an off-by-one that laps
        // a selection window over the days it is scored on is look-ahead bias
        // wearing the costume of a walk-forward, and every number downstream
        // would still look ordinary.
        for anchored in [true, false] {
            for (in_sample, out_of_sample) in plan(anchored, 90, 30).folds() {
                assert!(
                    out_of_sample.from > in_sample.to,
                    "{anchored}: judged from {} but selected up to {}",
                    out_of_sample.from,
                    in_sample.to
                );
            }
        }
    }

    #[test]
    fn folds_are_contiguous_and_cover_the_span_once() {
        // Out-of-sample periods must abut, not overlap: a day counted twice is
        // a trade counted twice in the stitched record.
        let folds = plan(true, 90, 30).folds();
        for pair in folds.windows(2) {
            assert_eq!(
                pair[1].1.from,
                pair[0].1.to + chrono::Duration::days(1),
                "out-of-sample periods must abut"
            );
        }
    }

    #[test]
    fn an_anchored_run_keeps_every_selection_window_starting_at_the_beginning() {
        let folds = plan(true, 90, 30).folds();
        assert!(folds.len() >= MIN_FOLDS);
        assert!(
            folds.iter().all(|(is, _)| is.from == date(2024, 1, 1)),
            "anchored means every selection sees all history"
        );
        // And each one is longer than the last, which is what "expanding"
        // means and what distinguishes it from sliding.
        for pair in folds.windows(2) {
            assert!(pair[1].0.days() > pair[0].0.days());
        }
    }

    #[test]
    fn a_sliding_run_keeps_every_selection_window_the_same_length() {
        let folds = plan(false, 90, 30).folds();
        assert!(folds.len() >= MIN_FOLDS);
        assert!(
            folds.iter().all(|(is, _)| is.days() == 90),
            "sliding means a fixed-width window that forgets"
        );
    }

    #[test]
    fn a_trailing_stub_is_dropped_rather_than_judged_as_a_fold() {
        // A four-day tail counted as a fold would weigh as heavily in every
        // count as a full thirty-day one.
        let folds = plan(true, 90, 30).folds();
        assert!(
            folds.iter().all(|(_, oos)| oos.days() == 30),
            "every judged period is a whole step"
        );
    }

    #[test]
    fn a_span_too_short_to_roll_produces_no_folds() {
        let mut plan = plan(true, 300, 90);
        plan.template.window = DateRange::new(date(2024, 1, 1), date(2024, 3, 1)).expect("ordered");
        assert!(plan.folds().is_empty());
    }

    #[test]
    fn nonsense_window_lengths_produce_no_folds_rather_than_looping() {
        assert!(plan(true, 0, 30).folds().is_empty());
        assert!(plan(true, 90, 0).folds().is_empty());
    }

    fn point(day: u32, equity: f64) -> EquityPoint {
        EquityPoint {
            at: date(2024, 1, day).and_time(chrono::NaiveTime::MIN),
            equity,
        }
    }

    #[test]
    fn stitching_carries_the_difference_forward_not_the_ratio() {
        // Additive, because sizing is fixed-fractional against *starting*
        // capital. Compounding the join while the sizing does not compound
        // would inflate every later fold.
        let first = [point(1, 1_000.0), point(2, 1_100.0)];
        let second = [point(3, 1_000.0), point(4, 1_100.0)];

        let curve = stitch(1_000.0, [first.as_slice(), second.as_slice()].into_iter());
        let last = curve.last().expect("non-empty").equity;
        assert!(
            (last - 1_200.0).abs() < 1e-9,
            "two 100-unit gains is 200, not 1210: {last}"
        );
    }

    #[test]
    fn a_stitched_curve_opens_at_the_starting_balance() {
        let first = [point(1, 1_000.0), point(2, 900.0)];
        let curve = stitch(1_000.0, [first.as_slice()].into_iter());
        assert!((curve[0].equity - 1_000.0).abs() < f64::EPSILON);
        assert!((curve[1].equity - 900.0).abs() < f64::EPSILON);
    }

    fn stats(wins: u32, win: f64, losses: u32, loss: f64) -> TradeStats {
        let ledger: Vec<Trade> = (0..wins)
            .map(|_| trade(win))
            .chain((0..losses).map(|_| trade(-loss)))
            .collect();
        TradeStats::from_ledger(&ledger)
    }

    fn trade(pnl: f64) -> Trade {
        Trade {
            opened: date(2024, 1, 1).and_time(chrono::NaiveTime::MIN),
            closed: Some(date(2024, 1, 3).and_time(chrono::NaiveTime::MIN)),
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: Some(110.0),
            pnl,
            commission: 1.0,
            exit_reason: ExitReason::Signal,
        }
    }

    #[test]
    fn combining_folds_gives_what_one_pass_over_the_whole_ledger_would() {
        // Exact rather than approximate: every field is additive or rebuilt
        // from additive parts. If this drifts, the stitched record disagrees
        // with the folds it is made of.
        let a = stats(3, 100.0, 1, 50.0);
        let b = stats(1, 200.0, 4, 25.0);
        let combined = TradeStats::combine([&a, &b].into_iter());
        let whole = stats(4, 0.0, 5, 0.0); // shape only; values checked below

        assert_eq!(combined.closed, whole.closed);
        assert_eq!(combined.wins, 4);
        assert_eq!(combined.losses, 5);
        assert_eq!(combined.win_rate, Some(4.0 / 9.0));
        // 3×100 + 1×200 = 500 gross profit; 1×50 + 4×25 = 150 gross loss.
        let factor = combined.profit_factor.expect("both sides present");
        assert!((factor - 500.0 / 150.0).abs() < 1e-9, "{factor}");
        assert!((combined.average_win.expect("wins") - 125.0).abs() < 1e-9);
        assert!((combined.total_commission - 9.0).abs() < 1e-9);
    }

    #[test]
    fn combining_keeps_the_worst_and_best_single_trades() {
        let a = stats(1, 100.0, 1, 10.0);
        let b = stats(1, 20.0, 1, 500.0);
        let combined = TradeStats::combine([&a, &b].into_iter());
        assert_eq!(combined.largest_win, Some(100.0));
        assert_eq!(combined.largest_loss, Some(-500.0));
    }

    #[test]
    fn combining_nothing_is_an_empty_record_not_a_zeroed_one() {
        let combined = TradeStats::combine([].into_iter());
        assert_eq!(combined.closed, 0);
        assert_eq!(combined.win_rate, None, "no trades is not a 0% win rate");
        assert_eq!(combined.profit_factor, None);
    }
}
