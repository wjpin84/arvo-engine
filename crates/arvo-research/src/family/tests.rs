//! Tests for [`super`].
//!
//! Split out of `family.rs` on 2026-09-26 — it was 314 lines of
//! tests against 617 of code, which is the shape
//! `advice/` and `replay/` already moved out for.

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
    // The costed figures are what the leaderboard ranks by (#226). The fixture provider
    // fabricates a curve without closed trades, so the return carries the sign here.
    assert!(thin.conservative.as_ref().is_some_and(|costed| costed.verdict == Verdict::NotSupported && costed.total_return < 0.0), "{:?}", thin.conservative);
    assert_eq!(thin.verdict, Verdict::NotSupported, "refused, never upgraded");
    assert!(thin.reasons.iter().any(|why| why.contains("conservative tier")), "{:?}", thin.reasons);

    // Ten basis points survives both, and the record says so.
    let wide = run_family(&Costly { edge_bps: 10.0 }, &family, &criteria).expect("runs");
    assert_eq!(wide.under_conservative_costs, Some(Verdict::Supported));
    assert!(wide.conservative.as_ref().is_some_and(|costed| costed.verdict == Verdict::Supported && costed.total_return > 0.0), "{:?}", wide.conservative);
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
