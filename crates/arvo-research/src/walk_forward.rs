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
    /// The cadence that produced this, pinned into the record.
    ///
    /// Not a display detail. The same rule re-selected yearly and re-selected
    /// every three years is not the same experiment — it sees different
    /// selection windows, restarts a different number of times, and pays the
    /// cold-start cost a different number of times. A stored finding that did
    /// not say which one it was would not be reproducible, and could not even
    /// be redrawn.
    pub in_sample_days: i64,
    pub step_days: i64,
    pub anchored: bool,
    /// The grid each fold re-selected from.
    ///
    /// The template and the cadence were already here; this was the one input
    /// missing, and without it the procedure cannot be run again. A record
    /// that pins everything except the search it performed describes a result
    /// nobody can reproduce.
    ///
    /// `None` for a run recorded before this existed.
    #[serde(default)]
    pub grid: Option<crate::ParameterGrid>,
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
        in_sample_days: plan.in_sample_days,
        step_days: plan.step_days,
        anchored: plan.anchored,
        grid: Some(plan.grid.clone()),
        criteria: Some(*criteria),
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

/// Whether the folds' selections beat what no ability to select would produce.
///
/// # Counting folds was the wrong statistic
///
/// This asked how many folds cleared their own no-skill bar and tested that
/// count against a binomial null. It was a real improvement on the gate before
/// it, which refused only when *no* fold cleared, and it had two faults.
///
/// It threw away the magnitudes. A fold that beat its bar by 0.5 and one that
/// missed by 0.01 counted as a one and a zero, when together they are evidence
/// of selection. That is the difference between a sign test and a test that
/// uses the numbers, and it is a large difference in power at seven
/// observations.
///
/// And a count of seven is a coarse thing to test. `P(>= 6 of 7) = 0.0625`
/// sits just the wrong side of 5%, so seven folds demanded all seven while
/// eight folds demanded seven of eight. Nineteen years of daily bars — the
/// most the broker will serve — produces exactly seven folds at the pinned
/// cadence, so the bar landed on its harshest setting precisely where the data
/// lands.
///
/// # What this does instead
///
/// Each fold contributes a *margin*: how far its winner's in-sample Sharpe sat
/// above the maximum a no-skill search of that size would be expected to
/// reach. A procedure that cannot select produces margins scattered around
/// zero; one that can produces margins that are positive more often and by
/// more.
///
/// The test is a sign-flip permutation on those margins. Under a null of no
/// ability, each margin is as likely to have come out negative as positive, so
/// every assignment of signs is equally probable; this asks where the observed
/// mean sits among all of them. Exact for the fold counts a walk-forward
/// produces, free of any distributional assumption beyond that symmetry, and
/// smooth in the number of folds rather than lurching between 100% and 87.5%
/// required.
///
/// # What it still assumes
///
/// That the margins are symmetric about zero under the null. Maxima are
/// right-skewed, so their margins are mildly right-skewed too and this is
/// slightly anticonservative. Stated rather than corrected: the correction is
/// small next to the effect it is looking for, and inventing one would be
/// another piece of arithmetic to be wrong about.
#[must_use]
pub(crate) fn selection_beat_chance(folds: &[FamilyEvidence]) -> bool {
    /// The false-positive rate this accepts from the margins alone.
    const ALPHA: f64 = 0.05;
    /// Beyond this many folds, enumerating every sign pattern stops being
    /// free. Sampled deterministically past it, so a verdict never depends on
    /// when it was computed.
    const EXACT_UP_TO: usize = 20;

    let margins: Vec<f64> = folds
        .iter()
        .filter_map(|fold| {
            // A fold whose trials all scored alike has no bar and no margin.
            // Skipped rather than counted as zero, which would be evidence of
            // failing to select where there was no search to speak of.
            let bar = fold.selection.expected_best_under_null?;
            Some(fold.selection.best_sharpe - bar)
        })
        .collect();

    if margins.len() < MIN_FOLDS {
        return false;
    }
    #[expect(clippy::cast_precision_loss, reason = "fold counts are small")]
    let count = margins.len() as f64;
    let observed = margins.iter().sum::<f64>() / count;
    if observed <= 0.0 {
        return false;
    }

    let (mut at_least_as_extreme, mut total) = (0_u64, 0_u64);
    let mut consider = |signs: u64| {
        let mut sum = 0.0;
        for (index, margin) in margins.iter().enumerate() {
            if signs >> index & 1 == 0 {
                sum += margin;
            } else {
                sum -= margin;
            }
        }
        total += 1;
        if sum / count >= observed {
            at_least_as_extreme += 1;
        }
    };

    if margins.len() <= EXACT_UP_TO {
        for signs in 0..(1_u64 << margins.len()) {
            consider(signs);
        }
    } else {
        // xorshift64*, fixed seed: the same folds must always give the same
        // verdict, and a test whose answer moves between runs is not one a
        // stored finding could be checked against.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..200_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            consider(state.wrapping_mul(0x2545_f491_4f6c_dd1d));
        }
    }

    #[expect(clippy::cast_precision_loss, reason = "counts fit a double exactly")]
    let p = at_least_as_extreme as f64 / total as f64;
    p <= ALPHA
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

    if !selection_beat_chance(folds) {
        reasons.push(format!(
            "{surviving} of {} folds selected a winner above the no-skill bar, which a \
             procedure with no ability to select would manage about half the time; that is \
             not enough of them to say this one selects",
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
            // Any fold halting is the whole record halting: the stitched run
            // has a hole in it wherever that fold stopped early.
            out.halted |= part.halted;
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
            alongside: Vec::new(),
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

    /// One fold, with only the fields the selection test reads meaning
    /// anything. Everything else is the least a `FamilyEvidence` will accept.
    fn experiment_fold() -> FamilyEvidence {
        let window = DateRange::new(date(2024, 1, 1), date(2024, 12, 31)).expect("ordered");
        let metrics = Metrics {
            total_return: 0.1,
            cagr: 0.1,
            max_drawdown: 0.05,
            volatility: 0.1,
            sharpe: Some(1.0),
            sortino: Some(1.2),
            calmar: Some(0.9),
            psr: None,
            trades: 10,
        };
        FamilyEvidence {
            hypothesis: HypothesisId::from("h"),
            in_sample: window,
            out_of_sample: window,
            selection: crate::Selection {
                trials: 9,
                best_sharpe: 1.0,
                expected_best_under_null: Some(1.0),
                survived_deflation: true,
                scored: Vec::new(),
            },
            selected: experiment(window),
            out_of_sample_evidence: crate::Evidence {
                hypothesis: HypothesisId::from("h"),
                experiment: experiment(window),
                benchmark: crate::ExperimentId::from("b"),
                engine: "test 1".to_owned(),
                criteria: EvaluationCriteria::default(),
                evaluation: crate::Evaluation {
                    strategy: metrics.clone(),
                    benchmark: metrics,
                    strategy_curve: Vec::new(),
                    benchmark_curve: Vec::new(),
                    strategy_trades: TradeStats::default(),
                    strategy_ledger: Vec::new(),
                    benchmark_instruments: Vec::new(),
                    excess_return: 0.0,
                    verdict: Verdict::Inconclusive,
                    reasons: Vec::new(),
                },
            },
            failures: Vec::new(),
            verdict: Verdict::Inconclusive,
            reasons: Vec::new(),
        }
    }

    /// A fold whose winner beat its no-skill bar by `margin`.
    fn fold_with_margin(margin: f64) -> FamilyEvidence {
        let mut fold = experiment_fold();
        fold.selection.best_sharpe = 1.0 + margin;
        fold.selection.expected_best_under_null = Some(1.0);
        fold
    }

    fn folds_with(margins: &[f64]) -> Vec<FamilyEvidence> {
        margins.iter().copied().map(fold_with_margin).collect()
    }

    #[test]
    fn margins_scattered_around_zero_are_what_no_ability_produces() {
        // Four up, three down, none by much: the shape of a search that is
        // picking whichever configuration happened to score highest.
        let folds = folds_with(&[0.05, -0.04, 0.03, -0.06, 0.02, -0.01, 0.04]);
        assert!(!selection_beat_chance(&folds));
    }

    #[test]
    fn margins_that_are_positive_and_large_are_selection() {
        let folds = folds_with(&[0.30, 0.25, 0.41, 0.18, 0.33, 0.29, 0.22]);
        assert!(selection_beat_chance(&folds));
    }

    #[test]
    fn the_size_of_a_miss_counts_and_not_only_its_sign() {
        // The whole reason for changing the statistic. Both of these have five
        // folds above their bar and two below; counting cannot tell them
        // apart. One clears by a lot and misses by a hair, the other the
        // reverse, and they are not the same evidence.
        let convincing = folds_with(&[0.40, 0.35, 0.30, 0.45, 0.38, -0.01, -0.02]);
        let unconvincing = folds_with(&[0.02, 0.01, 0.03, 0.02, 0.01, -0.40, -0.35]);

        assert!(selection_beat_chance(&convincing));
        assert!(!selection_beat_chance(&unconvincing));
    }

    #[test]
    fn seven_folds_no_longer_demand_all_seven() {
        // The coupling that forced this change. A binomial test on a count of
        // seven put `P(>= 6 of 7)` at 0.0625, just the wrong side of 5%, so
        // seven folds demanded perfection — and nineteen years of daily bars,
        // the most the broker serves, produces exactly seven folds.
        let six_of_seven = folds_with(&[0.30, 0.28, 0.35, 0.31, 0.26, 0.33, -0.02]);
        assert_eq!(six_of_seven.len(), 7);
        assert!(selection_beat_chance(&six_of_seven));
    }

    #[test]
    fn a_procedure_that_missed_on_average_is_never_selection() {
        // No amount of permuting rescues a mean below zero, and the test
        // returns before doing any.
        let folds = folds_with(&[0.10, -0.20, 0.05, -0.30, 0.02, -0.15, 0.01]);
        assert!(!selection_beat_chance(&folds));
    }

    #[test]
    fn too_few_folds_cannot_demonstrate_a_process_however_good_they_look() {
        // Two folds have four sign patterns; the best possible p-value is
        // 0.25. Refusing on the fold count rather than letting the arithmetic
        // return an answer it cannot support.
        assert!(!selection_beat_chance(&folds_with(&[0.9, 0.8])));
    }

    #[test]
    fn a_fold_whose_trials_all_scored_alike_is_skipped_not_counted_as_a_miss() {
        // There was no search in that fold, so there is nothing it failed to
        // select from. Counting it as a zero would be evidence against a
        // procedure for a fold that never tested it.
        let mut folds = folds_with(&[0.30, 0.28, 0.35, 0.31, 0.26, 0.33]);
        let mut flat = experiment_fold();
        flat.selection.expected_best_under_null = None;
        folds.push(flat);

        assert!(
            selection_beat_chance(&folds),
            "the six real folds are what decides it"
        );
    }

    #[test]
    fn the_test_lets_through_about_one_no_skill_procedure_in_twenty() {
        // The property that makes it a test. Margins drawn symmetrically about
        // zero, which is what the null asserts, and the pass rate counted.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let u = ((state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64)
                / ((1_u64 << 53) as f64);
            u * 2.0 - 1.0
        };

        let mut passed = 0;
        let runs = 400;
        for _ in 0..runs {
            let margins: Vec<f64> = (0..7).map(|_| next()).collect();
            if selection_beat_chance(&folds_with(&margins)) {
                passed += 1;
            }
        }
        let rate = f64::from(passed) / f64::from(runs) * 100.0;
        assert!(
            rate <= 10.0,
            "{rate:.0}% of no-skill procedures passed, against a 5% target"
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
            instrument: String::new(),
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
