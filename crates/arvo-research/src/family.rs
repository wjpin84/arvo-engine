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
    let under_conservative_costs = if survived_deflation && out_of_sample_evidence.evaluation.verdict == Verdict::Supported {
        let mut costly = selected.clone();
        costly.costs = costly.costs.at(arvo_risk::CostTier::Conservative);
        costly.id = crate::ExperimentId(format!("{}-conservative", selected.id));
        Some(evaluate_against_benchmark(provider, &costly, criteria)?.evaluation.verdict)
    } else {
        None
    };

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
            name: template.strategy.name.clone(),
            params: merged,
        },
        ..template.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn a_grid_produces_every_combination_in_a_stable_order() {
        let grid = ParameterGrid::new()
            .axis("fast", vec![5.0, 10.0])
            .axis("slow", vec![20.0, 30.0, 40.0]);

        assert_eq!(grid.size(), 6);
        let combinations = grid.combinations();
        assert_eq!(combinations.len(), 6);
        assert_eq!(
            combinations,
            grid.combinations(),
            "the order is part of the record and must not vary"
        );
        assert_eq!(combinations[0]["fast"], 5.0);
        assert_eq!(combinations[0]["slow"], 20.0);
    }

    #[test]
    fn an_empty_grid_tests_nothing_and_says_so() {
        assert_eq!(ParameterGrid::new().size(), 0);
        assert!(ParameterGrid::new().combinations().is_empty());
        assert!(
            ParameterGrid::new()
                .axis("fast", vec![])
                .combinations()
                .is_empty(),
            "an axis with no values leaves nothing to run"
        );
    }

    #[test]
    fn a_window_splits_into_two_periods_that_do_not_overlap() {
        let window = DateRange::new(date(2024, 1, 1), date(2024, 1, 10)).expect("ordered");
        let (head, tail) = window.split(0.7).expect("ten days splits");

        assert_eq!(head.from, date(2024, 1, 1));
        assert_eq!(tail.to, date(2024, 1, 10));
        assert!(head.to < tail.from, "no day appears in both periods");
        assert_eq!(
            head.days() + tail.days(),
            window.days(),
            "and no day is lost between them"
        );
    }

    #[test]
    fn a_window_too_short_to_hold_back_anything_refuses_to_split() {
        let single = DateRange::new(date(2024, 1, 1), date(2024, 1, 1)).expect("ordered");
        assert!(single.split(0.7).is_none());

        let pair = DateRange::new(date(2024, 1, 1), date(2024, 1, 2)).expect("ordered");
        assert!(
            pair.split(1.0).is_none(),
            "a split leaving no out-of-sample period is not a split"
        );
        assert!(pair.split(0.0).is_none());
    }

    /// Expected maximum of `n` standard normals, to three decimals, from
    /// 200,000 simulated draws each. The numbers this estimator has to hit.
    const TRUE_EXPECTED_MAXIMUM: &[(f64, f64)] = &[
        (3.0, 0.846),
        (5.0, 1.163),
        (6.0, 1.268),
        (9.0, 1.485),
        (12.0, 1.629),
        (25.0, 1.965),
        (100.0, 2.508),
    ];

    #[test]
    fn the_null_bar_is_calibrated_at_the_sizes_actually_searched() {
        // The bug this replaced. Every grid this platform runs is six or nine
        // trials, and the asymptotic form overstated the expected maximum by
        // half there — so a no-skill search was held to a bar it could not
        // reach, and thirteen findings in a row were refused for it.
        for (n, truth) in TRUE_EXPECTED_MAXIMUM {
            let estimate = expected_maximum(*n);
            let error = (estimate / truth - 1.0).abs() * 100.0;
            assert!(
                error < 5.0,
                "n={n}: estimated {estimate:.3} against a true {truth:.3} ({error:.1}% out)"
            );
        }
    }

    #[test]
    fn the_asymptotic_form_this_replaced_is_the_one_that_is_wrong() {
        // Kept as a test rather than only as prose, so the claim in the doc
        // above is checkable and stays true.
        for (n, truth) in TRUE_EXPECTED_MAXIMUM {
            if *n > 50.0 {
                continue;
            }
            let asymptotic = (2.0 * n.ln()).sqrt();
            assert!(
                asymptotic > truth * 1.2,
                "n={n}: sqrt(2 ln n) is {asymptotic:.3} against a true {truth:.3}, \
                 which should be at least 20% high"
            );
        }
    }

    #[test]
    fn a_no_skill_search_clears_its_own_bar_about_half_the_time() {
        // The property that makes the bar meaningful, and the one thirteen
        // consecutive refusals said was missing. Draws with no skill at all:
        // the observed best should land above the estimated expected best
        // roughly half the time, because that is what an expectation is.
        //
        // Deterministic draws rather than a seeded generator, so this cannot
        // fail on somebody else's machine for a reason that is not the code.
        let mut cleared = 0;
        let mut total = 0;
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            // xorshift64*, and a Box-Muller pair from it.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let u =
                ((state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64) / ((1_u64 << 53) as f64);
            u.clamp(1e-12, 1.0 - 1e-12)
        };

        for _ in 0..2000 {
            let sharpes: Vec<f64> = (0..9)
                .map(|_| {
                    let (u1, u2) = (next(), next());
                    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
                })
                .collect();
            let best = sharpes.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            if let Some(bar) = expected_best_under_null(&sharpes) {
                total += 1;
                if best > bar {
                    cleared += 1;
                }
            }
        }

        let share = f64::from(cleared) / f64::from(total) * 100.0;
        assert!(
            (30.0..=70.0).contains(&share),
            "a no-skill search cleared its own bar {share:.0}% of the time; \
             the old estimator managed 0 of 13 on real runs"
        );
    }

    #[test]
    fn the_null_bar_rises_with_the_number_of_trials() {
        let few = expected_best_under_null(&[0.0, 1.0, -1.0]).expect("three trials");
        let many: Vec<f64> = (0..100).map(|i| f64::from(i % 3) - 1.0).collect();
        let many = expected_best_under_null(&many).expect("hundred trials");

        assert!(
            many > few,
            "searching harder should require a better result: {many} vs {few}"
        );
    }

    #[test]
    fn a_search_where_everything_scored_the_same_has_nothing_to_deflate() {
        assert_eq!(expected_best_under_null(&[1.0, 1.0, 1.0]), None);
        assert_eq!(
            expected_best_under_null(&[1.0]),
            None,
            "one trial is no search"
        );
        assert_eq!(expected_best_under_null(&[]), None);
    }

    /// A provider whose result depends only on `fast`, so two configurations
    /// that share it tie exactly. The sequential loop broke a tie by taking
    /// the later configuration in grid order; the parallel one must too.
    struct Scripted;

    impl crate::SimulationProvider for Scripted {
        fn engine(&self) -> &str {
            "scripted 0"
        }

        fn run(&self, experiment: &Experiment) -> Result<crate::SimulationResult, crate::SimulationError> {
            let fast = experiment.strategy.params.get("fast").copied().unwrap_or(1.0);
            let step = 0.0005 * fast;
            let start = date(2023, 1, 2).and_time(chrono::NaiveTime::MIN);
            let equity_curve = (0..300)
                .map(|i| crate::EquityPoint {
                    at: start + chrono::Duration::days(i),
                    // A steady drift with a deterministic wobble, so the
                    // Sharpe is finite and differs by `fast` alone.
                    equity: 100_000.0 * (1.0 + step).powi(i as i32) * (1.0 + 0.002 * ((i as f64) * 0.7).sin()),
                })
                .collect();
            Ok(crate::SimulationResult {
                experiment: experiment.id.clone(),
                engine: "scripted 0".to_owned(),
                trades: 4,
                equity_curve,
                ledger: Vec::new(),
                refused: Default::default(),
            })
        }
    }

    /// A provider whose curve is a steady edge minus the stated slippage,
    /// so a winner that is Supported at one basis point is not at five.
    struct Costly {
        edge_bps: f64,
    }

    impl crate::SimulationProvider for Costly {
        fn engine(&self) -> &str {
            "costly 0"
        }

        fn run(&self, experiment: &Experiment) -> Result<crate::SimulationResult, crate::SimulationError> {
            let benchmark = experiment.strategy.name == crate::evaluation::BUY_AND_HOLD;
            let net = if benchmark { 0.0 } else { (self.edge_bps - experiment.costs.slippage_bps) / 10_000.0 };
            let start = date(2023, 1, 2).and_time(chrono::NaiveTime::MIN);
            let equity_curve = (0..300)
                .map(|i| crate::EquityPoint {
                    at: start + chrono::Duration::days(i),
                    equity: 100_000.0 * (1.0 + net).powi(i as i32) * (1.0 + 0.001 * ((i as f64) * 0.7).sin()),
                })
                .collect();
            Ok(crate::SimulationResult {
                experiment: experiment.id.clone(),
                engine: "costly 0".to_owned(),
                trades: 40,
                equity_curve,
                ledger: Vec::new(),
                refused: Default::default(),
            })
        }
    }

    #[test]
    fn a_finding_supported_only_under_the_stated_costs_is_refused() {
        let crate::memory::Record::Study(seed) = crate::memory::tests::study("AAPL.NASDAQ", "hash-a") else { unreachable!() };
        let mut template = seed.selected.clone();
        template.costs = crate::CostModel::proportional(0.0, 1.0);
        let grid = ParameterGrid::new().axis("fast", vec![5.0, 10.0]).axis("slow", vec![20.0, 30.0]);
        let family = ExperimentFamily {
            hypothesis: template.hypothesis.clone(),
            template,
            grid,
            in_sample_fraction: 0.7,
            prior_trials: 0,
        };
        let criteria = EvaluationCriteria { min_trades: 10, ..EvaluationCriteria::default() };

        // Three basis points of edge: positive at one point of slippage,
        // negative at the conservative five.
        let thin = run_family(&Costly { edge_bps: 3.0 }, &family, &criteria).expect("runs");
        assert_eq!(thin.out_of_sample_evidence.evaluation.verdict, Verdict::Supported, "under the stated costs");
        assert_eq!(thin.under_conservative_costs, Some(Verdict::NotSupported));
        assert_eq!(thin.verdict, Verdict::NotSupported, "refused, never upgraded");
        assert!(thin.reasons.iter().any(|why| why.contains("conservative tier")), "{:?}", thin.reasons);

        // Ten basis points survives both, and the record says so.
        let wide = run_family(&Costly { edge_bps: 10.0 }, &family, &criteria).expect("runs");
        assert_eq!(wide.under_conservative_costs, Some(Verdict::Supported));
        assert_eq!(wide.verdict, Verdict::Supported);

        // A result refused on its own terms is not asked the question.
        let none = run_family(&Costly { edge_bps: -3.0 }, &family, &criteria).expect("runs");
        assert_eq!(none.under_conservative_costs, None);
        assert_ne!(none.verdict, Verdict::Supported);
    }

    /// The sweep runs across the cores (#212); nothing about which trial wins
    /// may depend on which finished first.
    #[test]
    fn a_parallel_sweep_scores_trials_in_grid_order_and_breaks_a_tie_as_the_loop_did() {
        let crate::memory::Record::Study(seed) = crate::memory::tests::study("AAPL.NASDAQ", "hash-a") else { unreachable!() };
        let template = seed.selected.clone();
        let grid = ParameterGrid::new().axis("fast", vec![5.0, 10.0]).axis("slow", vec![20.0, 30.0]);
        let family = ExperimentFamily {
            hypothesis: template.hypothesis.clone(),
            template,
            grid: grid.clone(),
            in_sample_fraction: 0.7,
            prior_trials: 0,
        };
        let criteria = EvaluationCriteria::default();

        let first = run_family(&Scripted, &family, &criteria).expect("runs");
        let second = run_family(&Scripted, &family, &criteria).expect("runs again");

        let order: Vec<_> = first.selection.scored.iter().map(|trial| trial.params.clone()).collect();
        assert_eq!(order, grid.combinations(), "trials are scored in grid order, whatever finished first");
        assert_eq!(first.selection.scored, second.selection.scored, "a re-run is the same search");

        // fast=10 beats fast=5; between (10, 20) and (10, 30) the Sharpe is
        // identical, and the later in grid order is the one that won before.
        let winner = &first.selected.strategy.params;
        assert_eq!(winner.get("fast"), Some(&10.0));
        assert_eq!(winner.get("slow"), Some(&30.0), "the tie breaks as the sequential loop broke it");
        assert_eq!(second.selected.strategy.params, *winner);
    }
}
