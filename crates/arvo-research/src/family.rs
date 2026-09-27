//! Testing many configurations of one idea, without fooling yourself.
//!
//! A single experiment is easy to evaluate honestly. A *family* — the same
//! hypothesis over a grid of parameters — is where backtesting usually goes
//! wrong, because the obvious procedure is to run fifty configurations, report
//! the best one, and quietly forget the other forty-nine. The best of fifty
//! coin-flip strategies looks excellent.
//!
//! Two defences, applied together:
//!
//! * **Select in-sample, judge out-of-sample.** The winning configuration is
//!   chosen on the first part of the window and then evaluated on a part it
//!   has never touched. This is the defence that actually works; everything
//!   else is a refinement of it.
//! * **Deflate for the number of trials.** With `n` configurations tried, some
//!   Sharpe ratio will look good by chance. [`Selection`] records how good the
//!   best result would be expected to look under a null of no skill, and the
//!   family is refused if the winner does not clear it.
//!
//! Neither is optional and neither is sufficient alone: an out-of-sample
//! period gets reused the moment somebody runs a second family, and a
//! deflation threshold says nothing about whether the edge persists.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::evaluation::EvaluationCriteria;
use crate::{
    evaluate_against_benchmark, DateRange, Evidence, Experiment, ExperimentId, HypothesisId,
    Metrics, SimulationError, SimulationProvider, StrategySpec, Verdict,
};

/// Fraction of the window used to choose a configuration. The rest is held
/// back and not looked at until the choice is made.
/// Also used by [`crate::panel`], which splits its window the same way.
pub const DEFAULT_IN_SAMPLE_FRACTION: f64 = 0.7;

/// A set of parameter values to sweep.
///
/// `BTreeMap` throughout: the order combinations are generated in is part of
/// the record, so a family re-run produces the same trials in the same order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParameterGrid {
    axes: BTreeMap<String, Vec<f64>>,
}

impl ParameterGrid {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an axis. An axis with no values makes the grid empty, which
    /// [`run_family`] refuses rather than silently testing nothing.
    #[must_use]
    pub fn axis(mut self, name: &str, values: Vec<f64>) -> Self {
        self.axes.insert(name.to_owned(), values);
        self
    }

    /// Every combination, in a stable order.
    #[must_use]
    pub fn combinations(&self) -> Vec<BTreeMap<String, f64>> {
        let mut out: Vec<BTreeMap<String, f64>> = vec![BTreeMap::new()];
        for (name, values) in &self.axes {
            let mut next = Vec::with_capacity(out.len() * values.len());
            for base in &out {
                for value in values {
                    let mut combination = base.clone();
                    combination.insert(name.clone(), *value);
                    next.push(combination);
                }
            }
            out = next;
        }
        if self.axes.is_empty() {
            return Vec::new();
        }
        out
    }

    #[must_use]
    pub fn size(&self) -> usize {
        if self.axes.is_empty() {
            return 0;
        }
        self.axes.values().map(Vec::len).product()
    }
}

/// One hypothesis, one template experiment, and a grid of variations on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentFamily {
    pub hypothesis: HypothesisId,
    /// The experiment every trial is a variation of. Its window is the *whole*
    /// period; the split is applied by [`run_family`].
    pub template: Experiment,
    pub grid: ParameterGrid,
    /// Fraction of the window used for selection.
    pub in_sample_fraction: f64,
    /// Configurations already tried somewhere else to arrive at this family.
    ///
    /// Zero for a family designed here. An imported experiment carries the
    /// search that produced it, and this is where it lands: someone who ran
    /// fifty configurations and shared the one that survived has handed over
    /// the maximum of fifty draws, and deflating it against this machine's
    /// grid alone would count one. See [`crate::share`].
    #[serde(default)]
    pub prior_trials: usize,
}

impl ExperimentFamily {
    /// A family sweeping `grid` over `template`, split at the default point.
    #[must_use]
    pub fn new(template: Experiment, grid: ParameterGrid) -> Self {
        Self {
            hypothesis: template.hypothesis.clone(),
            template,
            grid,
            in_sample_fraction: DEFAULT_IN_SAMPLE_FRACTION,
            prior_trials: 0,
        }
    }
}

/// One configuration and what it scored in-sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredTrial {
    pub params: BTreeMap<String, f64>,
    pub sharpe: f64,
}

/// What the search over configurations found, and whether it means anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    /// Configurations that actually ran. Trials that failed are excluded here
    /// and listed in [`FamilyEvidence::failures`] — a trial that never ran is
    /// not a trial that found nothing.
    pub trials: usize,
    /// In-sample Sharpe of the chosen configuration.
    pub best_sharpe: f64,
    /// How good the best of `trials` would be expected to look with no skill
    /// at all. `None` when there are too few trials, or all of them scored
    /// identically, to say.
    pub expected_best_under_null: Option<f64>,
    /// Whether [`Self::best_sharpe`] cleared that bar.
    pub survived_deflation: bool,
    /// Configurations tried elsewhere before this search, counted into the
    /// bar above but not into [`Self::trials`], which is what ran here.
    ///
    /// Recorded because without it a bar raised by an imported search reads
    /// as a miscalibrated one. `default`: every earlier finding had none.
    #[serde(default)]
    pub prior_trials: usize,
    /// Every configuration that ran, with its in-sample score.
    ///
    /// Kept rather than discarded once the winner is known, and it is the
    /// difference between a claim and evidence for it. The *shape* of the
    /// search says whether there was anything to find: a broad region of
    /// configurations that all scored well is a plateau and suggests
    /// something real; one bright cell surrounded by nothing is what fitting
    /// noise looks like from above. Reporting only the maximum shows those
    /// two cases identically — which is precisely the mistake this crate
    /// exists to stop someone making.
    ///
    /// `default` because it is a persisted format: findings recorded before
    /// this loads as one with no surface to draw.
    #[serde(default)]
    pub scored: Vec<ScoredTrial>,
}

/// The result of running a family: what was tried, what was picked, and how
/// the pick did on data it was not chosen on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FamilyEvidence {
    pub hypothesis: HypothesisId,
    pub in_sample: DateRange,
    pub out_of_sample: DateRange,
    pub selection: Selection,
    /// The winning configuration, with its window set to the out-of-sample
    /// period it was judged on.
    pub selected: Experiment,
    /// Full evaluation of the winner out-of-sample, benchmark included.
    pub out_of_sample_evidence: Evidence,
    /// Configurations that could not be run, with the reason. Kept because a
    /// family where half the grid failed is a different claim from one where
    /// it all ran.
    pub failures: Vec<String>,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    /// What the winner's out-of-sample run said under the conservative cost
    /// tier (#192), when it was asked: `None` for a finding recorded before
    /// the question existed. A verdict Supported only under the stated
    /// costs is refused, never upgraded; this keeps what the other tier said
    /// so the reader can see how far the edge is from the costs.
    #[serde(default)]
    pub under_conservative_costs: Option<Verdict>,
    /// What the winner did under the conservative tier (#226): the figures
    /// the leaderboard ranks by, so a finding is ordered by what survives
    /// the costs and not by what the stated costs let it show. `None` when
    /// the tier was not asked, and for findings older than this field.
    #[serde(default)]
    pub conservative: Option<Costed>,
}

/// The out-of-sample result under the conservative cost tier (#192, #226).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Costed {
    pub verdict: Verdict,
    /// Mean profit per closed out-of-sample trade.
    pub expectancy: f64,
    pub total_return: f64,
    pub max_drawdown: f64,
    pub trades: u32,
}

/// Expected maximum of `n` independent draws, given the spread of what was
/// actually observed.
///
/// # The estimator, and the one that was here before
///
/// This used `sqrt(2 ln n)`, the asymptotic expected maximum of `n` standard
/// normals, and justified it with a caveat saying the approximation
/// "understates for very small n". That is backwards, and the error is not
/// small at the sizes this platform actually searches:
///
/// | trials | true E[max] | `sqrt(2 ln n)` | overstates by |
/// |--------|-------------|----------------|---------------|
/// | 6      | 1.268       | 1.893          | 49%           |
/// | 9      | 1.485       | 2.096          | 41%           |
/// | 100    | 2.508       | 3.035          | 21%           |
///
/// `sqrt(2 ln n)` is the leading term of an expansion that converges very
/// slowly; it is only usable in the thousands. Every grid this platform runs
/// is six or nine.
///
/// The consequence was not subtle. Of the thirteen findings on the machine
/// this was found on, **all thirteen** failed deflation, and several by a
/// margin the correction closes: 0.719 against a bar of 0.776, 0.675 against
/// 0.686. Under a correctly calibrated null a no-skill search clears its own
/// expected maximum about half the time. Thirteen for thirteen is not a
/// strict tool, it is a broken one, and it fails in the direction that feels
/// like rigour — which is why it survived.
///
/// So this uses the Bailey and López de Prado estimator instead, the same one
/// the Deflated Sharpe Ratio is built on. It is within 2.5% of the truth
/// across every size used here.
///
/// # What stays conservative, deliberately
///
/// Grid trials are *not* independent: neighbouring parameters produce nearly
/// the same strategy, so the effective number of trials is smaller than the
/// count and the true expected maximum is lower still. This does not correct
/// for that, and the test therefore remains harder to pass than it strictly
/// needs to be.
///
/// That margin is kept on purpose — a false negative costs an idea and a
/// false positive costs money. The difference is that it is now a stated
/// margin rather than an arithmetic mistake wearing one's clothes.
#[must_use]
pub fn expected_best_under_null(sharpes: &[f64]) -> Option<f64> {
    expected_best_of(sharpes, sharpes.len())
}

/// The same bar, for a search of `trials` of which only `sharpes` were kept.
///
/// A search spanning many findings can count more trials than it has scores
/// for: a finding recorded before [`Selection::scored`] existed says how many
/// configurations it tried but not what they scored. Those still widen the
/// search, so they count towards `n`; the spread is estimated from the scores
/// that survive. Dropping them instead would shrink the count, which is the
/// one direction this must never be wrong in.
#[must_use]
pub fn expected_best_of(sharpes: &[f64], trials: usize) -> Option<f64> {
    let n = sharpes.len();
    if n < 2 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "trial counts are small")]
    let count = n as f64;

    let mean = sharpes.iter().sum::<f64>() / count;
    let variance = sharpes.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (count - 1.0);
    let spread = variance.sqrt();
    if spread <= 0.0 {
        // Every configuration scored the same. There was no search, so there
        // is nothing to deflate — and no way to estimate a spread.
        return None;
    }

    #[expect(clippy::cast_precision_loss, reason = "trial counts are small")]
    let searched = trials.max(n) as f64;
    Some(mean + spread * expected_maximum(searched))
}

/// Expected maximum of `n` independent standard normals.
///
/// Bailey and López de Prado's estimator, as used by the Deflated Sharpe
/// Ratio: a weighted blend of two upper quantiles, with the Euler-Mascheroni
/// constant as the weight. Accurate to a few percent from `n = 3` upward,
/// where the asymptotic form is out by half.
fn expected_maximum(n: f64) -> f64 {
    /// Euler-Mascheroni.
    const GAMMA: f64 = 0.577_215_664_901_532_9;

    (1.0 - GAMMA) * inverse_normal_cdf(1.0 - 1.0 / n)
        + GAMMA * inverse_normal_cdf(1.0 - 1.0 / (n * std::f64::consts::E))
}

/// The value a standard normal falls below with probability `p`.
///
/// By bisection on `libm::erf`, which is already in the build for
/// [`crate::psr`]. Slower than a rational approximation and correct by
/// construction: it inverts the same function the rest of the crate uses to
/// go the other way, so the two cannot disagree, and there is no polynomial
/// here to have mistyped. It runs twice per family.
fn inverse_normal_cdf(p: f64) -> f64 {
    let cdf = |x: f64| 0.5 * (1.0 + libm::erf(x / std::f64::consts::SQRT_2));

    // Wide enough for any `p` a trial count can produce: at n = 1, 1 - 1/n is
    // 0 and at enormous n it approaches 1, and both ends are clamped by the
    // bracket rather than running away.
    let (mut low, mut high) = (-10.0_f64, 10.0_f64);
    for _ in 0..200 {
        let mid = f64::midpoint(low, high);
        if cdf(mid) < p {
            low = mid;
        } else {
            high = mid;
        }
    }
    f64::midpoint(low, high)
}

/// Runs every configuration in a family and reports what survives.
///
/// The procedure, in order, because the order is the whole point:
///
/// 1. Split the window. Everything after the boundary is untouched until 5.
/// 2. Run each configuration on the in-sample period only.
/// 3. Rank by in-sample Sharpe and take the best.
/// 4. Ask whether that best is better than a no-skill search of the same size
///    would be expected to produce.
/// 5. Re-run the winner out-of-sample, against its benchmark, and evaluate.
///
/// # Errors
///
/// Returns [`SimulationError`] if the grid is empty, the window cannot be
/// split, every trial failed, or the winner's out-of-sample run failed.
pub fn run_family(
    provider: &dyn SimulationProvider,
    family: &ExperimentFamily,
    criteria: &EvaluationCriteria,
) -> Result<FamilyEvidence, SimulationError> {
    let combinations = family.grid.combinations();
    if combinations.is_empty() {
        return Err(SimulationError::Rejected(
            "the parameter grid is empty, so the family tests nothing".to_owned(),
        ));
    }

    let (in_sample, out_of_sample) = family
        .template
        .window
        .split(family.in_sample_fraction)
        .ok_or_else(|| {
            SimulationError::Rejected(format!(
                "window {}..={} cannot be split at {} into two usable periods",
                family.template.window.from, family.template.window.to, family.in_sample_fraction
            ))
        })?;

    search(
        provider,
        &family.hypothesis,
        &family.template,
        &family.grid,
        (in_sample, out_of_sample),
        family.prior_trials,
        criteria,
    )
}

/// The same procedure against windows chosen by the caller.
///
/// Split out so walk-forward can drive it fold by fold without restating any
/// of it. Selection, deflation and evaluation are the part that has to be
/// identical between a single split and a rolling one — a walk-forward that
/// scored its folds even slightly differently from a plain study would not be
/// comparable to one, and comparing them is the entire point.
///
/// # Errors
///
/// Returns [`SimulationError`] if the grid is empty, every trial failed, or
/// the winner's out-of-sample run failed.
pub fn run_split(
    provider: &dyn SimulationProvider,
    hypothesis: &HypothesisId,
    template: &Experiment,
    grid: &ParameterGrid,
    in_sample: DateRange,
    out_of_sample: DateRange,
    criteria: &EvaluationCriteria,
) -> Result<FamilyEvidence, SimulationError> {
    search(
        provider,
        hypothesis,
        template,
        grid,
        (in_sample, out_of_sample),
        0,
        criteria,
    )
}

/// [`run_split`], deflated against `prior_trials` more configurations than it
/// runs itself.
fn search(
    provider: &dyn SimulationProvider,
    hypothesis: &HypothesisId,
    template: &Experiment,
    grid: &ParameterGrid,
    (in_sample, out_of_sample): (DateRange, DateRange),
    prior_trials: usize,
    criteria: &EvaluationCriteria,
) -> Result<FamilyEvidence, SimulationError> {
    let combinations = grid.combinations();
    if combinations.is_empty() {
        return Err(SimulationError::Rejected(
            "the parameter grid is empty, so the family tests nothing".to_owned(),
        ));
    }
    let family = &ExperimentFamily {
        hypothesis: hypothesis.clone(),
        template: template.clone(),
        grid: grid.clone(),
        in_sample_fraction: 0.0,
        prior_trials,
    };

    // Every configuration's backtest is independent of every other, so they
    // run across the cores (#212). The outcomes come back in grid order —
    // rayon's collect keeps an indexed iterator's order — and are scored in
    // that order below, so a re-run produces the same trials in the same
    // order and a tie between two configurations is broken exactly as the
    // sequential loop broke it. Only how fast the trials run changes; never
    // which trial wins.
    use rayon::prelude::*;
    let outcomes: Vec<Result<Option<f64>, String>> = combinations
        .par_iter()
        .map(|combination| {
            let trial = variant(&family.template, combination, in_sample, "is");
            provider.run(&trial).map_err(|err| err.to_string()).map(|result| {
                // Annualised at the experiment's own resolution, not a
                // constant: a five-minute Sharpe scaled by 252 is understated
                // by about nine times, and nothing in the output would show it.
                Metrics::from_curve(
                    &result.equity_curve,
                    result.trades,
                    family.template.interval.periods_per_year(),
                )
                .and_then(|metrics| metrics.sharpe)
            })
        })
        .collect();

    let mut scored: Vec<(f64, BTreeMap<String, f64>)> = Vec::with_capacity(combinations.len());
    let mut failures = Vec::new();
    for (combination, outcome) in combinations.into_iter().zip(outcomes) {
        match outcome {
            Ok(Some(sharpe)) => scored.push((sharpe, combination)),
            // A flat curve is a configuration that never traded. It did not
            // lose the search, it did not enter it.
            Ok(None) => failures.push(format!("{combination:?}: produced no measurable return")),
            Err(err) => failures.push(format!("{combination:?}: {err}")),
        }
    }

    let sharpes: Vec<f64> = scored.iter().map(|(sharpe, _)| *sharpe).collect();
    let surface: Vec<ScoredTrial> = scored
        .iter()
        .map(|(sharpe, params)| ScoredTrial {
            params: params.clone(),
            sharpe: *sharpe,
        })
        .collect();
    let (best_sharpe, best_params) = scored
        .into_iter()
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .ok_or_else(|| {
            SimulationError::Rejected(format!(
                "no configuration in the family produced a measurable result ({} failed)",
                failures.len()
            ))
        })?;

    let expected = expected_best_of(&sharpes, sharpes.len() + prior_trials);
    let survived_deflation = expected.is_none_or(|bar| best_sharpe > bar);
    let selection = Selection {
        trials: sharpes.len(),
        best_sharpe,
        expected_best_under_null: expected,
        survived_deflation,
        prior_trials,
        scored: surface,
    };

    let selected = variant(&family.template, &best_params, out_of_sample, "oos");
    let out_of_sample_evidence = evaluate_against_benchmark(provider, &selected, criteria)?;

    // The same winner, the same window, fills that cost more (#192). Asked
    // only of a result that would otherwise be Supported: a result already
    // refused has nothing to lose, and the run is not free.
    let conservative = if survived_deflation && out_of_sample_evidence.evaluation.verdict == Verdict::Supported {
        let mut costly = selected.clone();
        costly.costs = costly.costs.at(arvo_risk::CostTier::Conservative);
        costly.id = crate::ExperimentId(format!("{}-conservative", selected.id));
        let costed = evaluate_against_benchmark(provider, &costly, criteria)?.evaluation;
        let closed: Vec<f64> = costed.strategy_ledger.iter().filter(|trade| trade.closed.is_some()).map(|trade| trade.pnl).collect();
        Some(Costed {
            verdict: costed.verdict,
            expectancy: if closed.is_empty() { 0.0 } else { closed.iter().sum::<f64>() / closed.len() as f64 },
            total_return: costed.strategy.total_return,
            max_drawdown: costed.strategy.max_drawdown,
            trades: costed.strategy.trades,
        })
    } else {
        None
    };
    let under_conservative_costs = conservative.as_ref().map(|costed| costed.verdict);

    let mut reasons = Vec::new();
    let verdict = if let Some(costly) = under_conservative_costs.filter(|costly| *costly != Verdict::Supported) {
        reasons.push(format!(
            "Supported under the stated costs ({:.1} bps commission, {:.1} bps slippage) and {costly:?} under the              conservative tier ({:.1} bps, {:.1} bps): the edge is the cost assumption's, not the rule's",
            selected.costs.commission_bps,
            selected.costs.slippage_bps,
            selected.costs.at(arvo_risk::CostTier::Conservative).commission_bps,
            selected.costs.at(arvo_risk::CostTier::Conservative).slippage_bps,
        ));
        Verdict::NotSupported
    } else if survived_deflation {
        out_of_sample_evidence.evaluation.verdict
    } else {
        let elsewhere = if prior_trials > 0 {
            format!(" and {prior_trials} tried before it was shared")
        } else {
            String::new()
        };
        reasons.push(format!(
            "best in-sample Sharpe {best_sharpe:.3} across {} trials{elsewhere} did not beat the {:.3} a \
             no-skill search of that size would be expected to produce; the winner is selection \
             noise",
            selection.trials,
            expected.unwrap_or_default()
        ));
        // Never upgrade a verdict, only refuse one. A configuration that looks
        // good out-of-sample after failing deflation is still the survivor of
        // a search that explains it.
        Verdict::NotSupported
    };
    reasons.extend(out_of_sample_evidence.evaluation.reasons.iter().cloned());
    if !failures.is_empty() {
        reasons.push(format!(
            "{} of the grid's configurations did not run",
            failures.len()
        ));
    }

    Ok(FamilyEvidence {
        hypothesis: family.hypothesis.clone(),
        in_sample,
        out_of_sample,
        selection,
        selected,
        out_of_sample_evidence,
        failures,
        verdict,
        reasons,
        under_conservative_costs,
        conservative,
    })
}

/// Builds one trial: the template, with these parameters, over this window.
fn variant(
    template: &Experiment,
    params: &BTreeMap<String, f64>,
    window: DateRange,
    phase: &str,
) -> Experiment {
    let mut merged = template.strategy.params.clone();
    for (name, value) in params {
        merged.insert(name.clone(), *value);
    }

    // The id names the configuration, so two trials of the same family are
    // never confusable and a result can be traced back to what produced it.
    let signature = params
        .iter()
        .map(|(name, value)| format!("{name}{value}"))
        .collect::<Vec<_>>()
        .join("-");

    Experiment {
        id: ExperimentId(format!("{}-{phase}-{signature}", template.id)),
        window,
        strategy: StrategySpec {
            // A rule written as data travels with every trial (#225).
            rule: template.strategy.rule.clone(),
            name: template.strategy.name.clone(),
            params: merged,
        },
        ..template.clone()
    }
}

#[cfg(test)]
mod tests;
