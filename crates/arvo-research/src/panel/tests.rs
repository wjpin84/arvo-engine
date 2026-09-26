//! Tests for [`super`].
//!
//! Split out of `panel.rs` on 2026-09-26 — it was 479 lines of
//! tests against 758 of code, which is the shape
//! `advice/` and `replay/` already moved out for.

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
        kept: None,
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

/// A provider whose score is deliberately *not* injective in the
/// parameter: two configurations tie for best, so which one wins is
/// decided by ordering alone. That is the property parallelising the
/// panel could quietly break, and the only one worth a test here.
struct Stepped;

impl SimulationProvider for Stepped {
    fn engine(&self) -> &str {
        "stepped"
    }

    fn run(&self, experiment: &Experiment) -> Result<crate::SimulationResult, SimulationError> {
        // Odd `fast` drifts up, even `fast` does not, so `fast` 5 and 7
        // score the same and so do 6 and 8. The instrument shifts the
        // level without touching the ranking, so every member agrees on
        // which configuration is best and the panel selects one.
        let fast = experiment.strategy.params.get("fast").copied().unwrap_or(0.0);
        #[expect(clippy::cast_possible_truncation, reason = "grid values are small integers")]
        let drift = if (fast as i64) % 2 == 1 { 0.002 } else { 0.0 };
        let level = 100.0 + f64::from(u32::from(experiment.instrument.starts_with("MSFT")));
        let mut equity = level;
        let points = (0..40)
            .map(|i| {
                // Alternating either side of the drift, so volatility is
                // finite and the Sharpe is a real number.
                equity *= 1.0 + drift + if i % 2 == 0 { 0.01 } else { -0.01 };
                crate::EquityPoint {
                    at: chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
                        .expect("a date")
                        .checked_add_signed(chrono::Duration::days(i))
                        .expect("in range")
                        .and_hms_opt(0, 0, 0)
                        .expect("midnight"),
                    equity,
                }
            })
            .collect();
        Ok(crate::SimulationResult {
            experiment: experiment.id.clone(),
            engine: "stepped".to_owned(),
            trades: 40,
            equity_curve: points,
            ledger: Vec::new(),
            refused: crate::Refused::default(),
        })
    }
}

fn tied_study() -> PanelStudy {
    PanelStudy::new(
        Experiment {
            id: crate::ExperimentId::from("panel"),
            hypothesis: HypothesisId::from("h"),
            instrument: "AAPL.NASDAQ".to_owned(),
            alongside: Vec::new(),
            underlying: None,
            window: DateRange::new(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("a date"),
                chrono::NaiveDate::from_ymd_opt(2024, 12, 31).expect("a date"),
            )
            .expect("ordered"),
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
        },
        vec!["AAPL.NASDAQ".to_owned(), "MSFT.NASDAQ".to_owned()],
        ParameterGrid::new().axis("fast", vec![5.0, 6.0, 7.0, 8.0]),
    )
}

#[test]
fn running_the_panel_across_the_cores_does_not_change_which_configuration_wins() {
    // The product runs on the thread pool now (#234), so the outcomes
    // arrive in whatever order the cores finish in. What must not change
    // is the order they are *scored* in: with `fast` 5 and 7 tied for
    // best, the sequential loop took the later one, and so must this.
    let study = tied_study();
    let evidence = run_panel(&Stepped, &study, &crate::EvaluationCriteria::default())
        .expect("every trial runs");
    assert_eq!(
        evidence.selected_params.get("fast"),
        Some(&7.0),
        "the last of the tied configurations wins, as the sequential loop had it"
    );

    // Every trial scored, none lost to the threading, and the members
    // came back in the order they were asked for rather than the order
    // they finished in.
    assert_eq!(evidence.selection.trials, 4, "one score per configuration");
    assert!(evidence.failures.is_empty(), "{:?}", evidence.failures);
    let members: Vec<&str> = evidence
        .per_instrument
        .iter()
        .map(|outcome| outcome.instrument.as_str())
        .collect();
    assert_eq!(members, study.instruments, "members stay in panel order");

    // And a second run agrees with the first, which a pool that leaked
    // its completion order into the scoring would not.
    let again = run_panel(&Stepped, &study, &crate::EvaluationCriteria::default())
        .expect("every trial runs");
    assert_eq!(evidence.selection.scored, again.selection.scored);
    assert_eq!(evidence.selected_params, again.selected_params);
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
        universe: None,
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
        ended_early: Vec::new(),
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
fn a_member_whose_bars_stop_a_month_early_ended_early_and_a_long_weekend_does_not() {
    let curve = |month: u32, day: u32| {
        vec![crate::EquityPoint {
            at: chrono::NaiveDate::from_ymd_opt(2023, month, day)
                .expect("valid")
                .and_hms_opt(0, 0, 0)
                .expect("valid"),
            equity: 1.0,
        }]
    };
    let curves = vec![
        ("SPY.AIEX".to_owned(), curve(4, 28)),
        ("KO.AIEX".to_owned(), curve(4, 25)),
        ("SIVB.AIEX".to_owned(), curve(3, 9)),
    ];
    assert_eq!(ended_early(&curves), ["SIVB.AIEX"]);
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
