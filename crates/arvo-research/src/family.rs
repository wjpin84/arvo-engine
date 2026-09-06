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
        }
    }
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
}

/// Expected maximum of `n` independent draws, given the spread of what was
/// actually observed.
///
/// Uses the asymptotic `sqrt(2 ln n)` approximation for the expected maximum
/// of `n` standard normals, rescaled by the observed mean and spread.
///
/// Two honest caveats, both pushing the same way:
///
/// * Grid trials are *not* independent — neighbouring parameters produce
///   nearly the same strategy — so the true expected maximum is lower than
///   this. The test is therefore harder to pass than it strictly needs to be.
/// * The approximation understates for very small `n`.
///
/// Both make this conservative, which is the right direction for a tool whose
/// purpose is to avoid believing things. A false negative costs an idea; a
/// false positive costs money.
#[must_use]
pub fn expected_best_under_null(sharpes: &[f64]) -> Option<f64> {
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

    Some(mean + spread * (2.0 * count.ln()).sqrt())
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

    run_split(
        provider,
        &family.hypothesis,
        &family.template,
        &family.grid,
        in_sample,
        out_of_sample,
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
    };

    let mut scored: Vec<(f64, BTreeMap<String, f64>)> = Vec::with_capacity(combinations.len());
    let mut failures = Vec::new();

    for combination in combinations {
        let trial = variant(&family.template, &combination, in_sample, "is");
        match provider.run(&trial) {
            Ok(result) => {
                // Annualised at the experiment's own resolution, not a
                // constant: a five-minute Sharpe scaled by 252 is understated
                // by about nine times, and nothing in the output would show it.
                let sharpe = Metrics::from_curve(
                    &result.equity_curve,
                    result.trades,
                    family.template.interval.periods_per_year(),
                )
                .and_then(|metrics| metrics.sharpe);
                match sharpe {
                    Some(sharpe) => scored.push((sharpe, combination)),
                    // A flat curve is a configuration that never traded. It
                    // did not lose the search, it did not enter it.
                    None => {
                        failures.push(format!("{combination:?}: produced no measurable return"))
                    }
                }
            }
            Err(err) => failures.push(format!("{combination:?}: {err}")),
        }
    }

    let sharpes: Vec<f64> = scored.iter().map(|(sharpe, _)| *sharpe).collect();
    let (best_sharpe, best_params) = scored
        .into_iter()
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .ok_or_else(|| {
            SimulationError::Rejected(format!(
                "no configuration in the family produced a measurable result ({} failed)",
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
    };

    let selected = variant(&family.template, &best_params, out_of_sample, "oos");
    let out_of_sample_evidence = evaluate_against_benchmark(provider, &selected, criteria)?;

    let mut reasons = Vec::new();
    let verdict = if survived_deflation {
        out_of_sample_evidence.evaluation.verdict
    } else {
        reasons.push(format!(
            "best in-sample Sharpe {best_sharpe:.3} across {} trials did not beat the {:.3} a \
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
}
