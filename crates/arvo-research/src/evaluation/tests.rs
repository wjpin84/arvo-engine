//! Tests for [`super`].
//!
//! Split out of `evaluation.rs` on 2026-09-26 — it was 294 lines of
//! tests against 770 of code, which is the shape
//! `advice/` and `replay/` already moved out for.

use super::*;

fn metrics(total_return: f64, max_drawdown: f64, trades: u32) -> Metrics {
    Metrics {
        total_return,
        cagr: total_return,
        max_drawdown,
        volatility: 0.1,
        sharpe: Some(1.0),
        sortino: Some(1.2),
        calmar: Some(0.9),
        psr: None,
        trades,
    }
}

#[test]
fn a_strategy_that_lost_money_is_not_supported_by_a_benchmark_that_lost_more() {
    let evaluation = Evaluation::new(
        metrics(-0.0034, 0.008, 42),
        metrics(-0.0061, 0.04, 1),
        Vec::new(),
        Vec::new(),
        &EvaluationCriteria::default(),
    );
    assert_eq!(evaluation.verdict, Verdict::Inconclusive);
    assert!(evaluation.excess_return > 0.0, "it did beat the benchmark");
    assert!(evaluation.reasons[0].contains("lost 0.34% itself"), "{:?}", evaluation.reasons);

    let made_money = Evaluation::new(
        metrics(0.002, 0.008, 42),
        metrics(-0.0061, 0.04, 1),
        Vec::new(),
        Vec::new(),
        &EvaluationCriteria::default(),
    );
    assert_eq!(made_money.verdict, Verdict::Supported, "making money while the market fell still counts");
}

/// Dates are irrelevant to every statistic here, so the fixtures walk one
/// day at a time and the tests stay about the numbers.
#[test]
fn the_tail_is_the_worst_period_the_worst_month_and_the_worst_twentieth() {
    // 40 daily periods: 38 small gains, a -10% and a -4% day.
    let mut values = vec![100.0];
    for day in 1..=40 {
        let last = *values.last().expect("seeded");
        values.push(last * match day {
            10 => 0.90,
            30 => 0.96,
            _ => 1.001,
        });
    }
    let tail = tail(&curve(&values)).expect("a tail");
    assert!((tail.worst_period + 0.10).abs() < 1e-9, "{}", tail.worst_period);
    // 5% of 40 is two periods: (-10% + -4%) / 2.
    assert!((tail.expected_shortfall + 0.07).abs() < 1e-9, "{}", tail.expected_shortfall);
    assert!(tail.worst_month.is_some_and(|month| month < -0.05));
    assert_eq!(super::tail(&curve(&[100.0])), None);
}

fn curve(values: &[f64]) -> Vec<EquityPoint> {
    let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .expect("valid")
        .and_time(chrono::NaiveTime::MIN);
    values
        .iter()
        .enumerate()
        .map(|(index, equity)| EquityPoint {
            at: start + chrono::Duration::days(index as i64),
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
fn sortino_ignores_upside_volatility_that_sharpe_punishes() {
    // Same total gain, but one path is all upward jumps and the other
    // gives some back. Sortino should separate them more sharply than
    // Sharpe does, because only one of them ever actually lost money.
    let steady = Metrics::from_curve(&curve(&[100.0, 110.0, 120.0, 130.0]), 3, 252.0)
        .expect("four points");
    let choppy = Metrics::from_curve(&curve(&[100.0, 130.0, 105.0, 130.0]), 3, 252.0)
        .expect("four points");

    assert_eq!(
        steady.sortino, None,
        "a curve that never falls has no downside to divide by"
    );
    assert!(
        choppy.sortino.is_some(),
        "one that does should have a finite Sortino"
    );
}

#[test]
fn calmar_is_return_per_unit_of_worst_drawdown() {
    let flat = Metrics::from_curve(&curve(&[100.0, 110.0, 120.0]), 1, 252.0).expect("three");
    assert_eq!(flat.calmar, None, "no drawdown, no ratio");

    let dipped =
        Metrics::from_curve(&curve(&[100.0, 150.0, 75.0, 120.0]), 1, 252.0).expect("four");
    let calmar = dipped.calmar.expect("there was a drawdown");
    assert!(
        (calmar - dipped.cagr / dipped.max_drawdown).abs() < 1e-12,
        "{calmar}"
    );
}

#[test]
fn monthly_returns_chain_so_compounding_them_gives_the_total() {
    let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .expect("valid")
        .and_time(chrono::NaiveTime::MIN);
    let points: Vec<EquityPoint> = [(0, 100.0), (20, 110.0), (40, 121.0), (75, 108.9)]
        .into_iter()
        .map(|(offset, equity)| EquityPoint {
            at: start + chrono::Duration::days(offset),
            equity,
        })
        .collect();

    let months = monthly_returns(&points);
    assert_eq!(months.len(), 3, "January, February, March");
    assert_eq!((months[0].year, months[0].month), (2024, 1));

    let compounded = months.iter().fold(1.0, |acc, m| acc * (1.0 + m.value));
    assert!(
        (compounded - 108.9 / 100.0).abs() < 1e-9,
        "months must chain, not each measure from the start: {compounded}"
    );
}

#[test]
fn an_empty_curve_has_no_months() {
    assert!(monthly_returns(&[]).is_empty());
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
        alongside: Vec::new(),
        underlying: None,
        window: DateRange::new(day(1), day(31)).expect("ordered"),
        interval: arvo_data::BarInterval::DAILY,
        dataset: DatasetRef {
            id: "d".to_owned(),
            version: "1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: StrategySpec {
            rule: None,
            name: "sma_cross".to_owned(),
            params: BTreeMap::from([("trade_size".to_owned(), 100.0)]),
        },
        costs: CostModel::proportional(1.5, 0.0),
        risk: crate::RiskModel::default(),
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
