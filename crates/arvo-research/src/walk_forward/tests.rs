//! Tests for [`super`].
//!
//! Split out of `walk_forward.rs` on 2026-09-26 — it was 391 lines of
//! tests against 665 of code, which is the shape
//! `advice/` and `replay/` already moved out for.

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
        underlying: None,
        window,
        interval: arvo_data::BarInterval::DAILY,
        dataset: crate::DatasetRef {
            id: "fixture".to_owned(),
            version: "1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: crate::StrategySpec {
            rule: None,
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
            prior_trials: 0,
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
            dividend_gap: None,
            stress: None,
            refused_orders: crate::Refused::default(),
                benchmark_instruments: Vec::new(),
                excess_return: 0.0,
                verdict: Verdict::Inconclusive,
                reasons: Vec::new(),
            },
        },
        failures: Vec::new(),
        verdict: Verdict::Inconclusive,
        reasons: Vec::new(),
    
        under_conservative_costs: None,
    
        conservative: None,
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
        journal: None,
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
