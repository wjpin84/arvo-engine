use super::*;
use crate::{
    EvaluationCriteria, FamilyEvidence, PanelEvidence, TradeStats, Verdict, WalkForwardEvidence,
};
use crate::{Direction, ExitReason, Trade};

fn trade(day: u32, held: i64, pnl: f64, reason: ExitReason) -> Trade {
    let opened = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .expect("valid")
        .and_time(chrono::NaiveTime::MIN)
        + chrono::Duration::days(i64::from(day));
    Trade {
        instrument: String::new(),
        opened,
        closed: Some(opened + chrono::Duration::days(held)),
        direction: Direction::Long,
        quantity: 100.0,
        entry: 10.0,
        exit: Some(11.0),
        pnl,
        commission: 1.0,
        exit_reason: reason,
        journal: None,
    }
}

#[test]
fn losing_less_than_a_falling_market_blocks() {
    let item = lost_money(-0.0034, 0.0027).expect("lost money and beat a falling benchmark");
    assert_eq!(item.severity, Severity::Blocking);
    assert!(item.evidence.contains("strategy -0.34%, buy-and-hold -0.61%"), "{}", item.evidence);
    assert!(lost_money(0.01, 0.02).is_none(), "made money");
    assert!(lost_money(-0.05, -0.02).is_none(), "already NotSupported: it lost to the benchmark");
}

#[test]
fn an_option_run_leads_with_its_losses_and_what_its_history_lacks() {
    // Collects 0.2% on 59 days and loses 15% on one: a Sharpe-friendly
    // shape until that one day.
    let start = chrono::NaiveDate::from_ymd_opt(2024, 3, 1).expect("valid");
    let mut equity = 100_000.0;
    let curve: Vec<crate::EquityPoint> = (0..61)
        .map(|day| {
            if day > 0 {
                equity *= if day == 45 { 0.85 } else { 1.002 };
            }
            crate::EquityPoint {
                at: (start + chrono::Duration::days(day)).and_time(chrono::NaiveTime::MIN),
                equity,
            }
        })
        .collect();

    let mut option = experiment();
    option.instrument = "SPY240621P00500000.AOPT".to_owned();
    let advice = option_tail(&option, &curve, &[], None);
    assert_eq!(advice.len(), 2);
    assert!(advice[0].evidence.contains("worst 1day -15.0%"), "{}", advice[0].evidence);
    assert!(advice[0].evidence.contains("worst month"), "{}", advice[0].evidence);
    assert!(advice[1].finding.contains("no crash"));

    assert!(option_tail(&experiment(), &curve, &[], None).is_empty(), "a stock run is unaffected");
    // A put spread reads SPY and holds puts: its ledger makes it an option run.
    let mut put = trade(0, 3, -50.0, ExitReason::Signal);
    put.instrument = "SPY240621P00500000.AOPT".to_owned();
    assert_eq!(option_tail(&experiment(), &curve, &[put.clone()], None).len(), 2);

    // With a replay, the crash leads, and blocks once it would take a tenth
    // of the account.
    let stress = crate::stress::Stress {
        shocks: vec![crate::stress::Shock {
            day: chrono::NaiveDate::from_ymd_opt(2020, 3, 16).expect("valid"),
            what: "covid crash".to_owned(),
            spot_move: -0.1094,
            vix: 82.69,
            position: put.instrument.clone(),
            opened: put.opened,
            loss: 12_000.0,
            credit: 300.0,
        }],
        unpriced: 0,
    };
    let advice = option_tail(&experiment(), &curve, &[put], Some(&stress));
    assert_eq!(advice.len(), 3);
    assert_eq!(advice[0].severity, Severity::Blocking);
    assert!(advice[0].evidence.contains("40.0x the credit"), "{}", advice[0].evidence);
}

#[test]
fn a_high_win_rate_that_loses_money_is_named_as_an_exit_problem() {
    // Six small wins, five large losses: wins more often than it loses,
    // loses money. The one shape a win rate alone always misreads.
    let mut ledger: Vec<Trade> = (0..6)
        .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
        .collect();
    ledger.extend((6..11).map(|day| trade(day, 3, -100.0, ExitReason::Stop)));

    let warnings = shape_warnings(&TradeStats::from_ledger(&ledger), true);
    assert!(
        warnings.iter().any(|r| r.action.contains("exit is the problem")),
        "{warnings:#?}"
    );
}

#[test]
fn one_trade_carrying_the_profit_is_called_out() {
    let mut ledger: Vec<Trade> = (0..10)
        .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
        .collect();
    ledger.push(trade(11, 3, 5_000.0, ExitReason::Signal));

    let warnings = shape_warnings(&TradeStats::from_ledger(&ledger), true);
    let concentration = warnings
        .iter()
        .find(|r| r.finding.contains("One trade"))
        .expect("a single trade produced 98% of profit");
    assert_eq!(concentration.severity, Severity::Warning);
    assert!(concentration.evidence.contains('%'), "cite the share");
}

#[test]
fn shape_is_not_characterised_on_too_few_trades() {
    // Four round trips, one of which is 90% of the profit. True, and not
    // worth saying: it is a statement about four round trips.
    let mut ledger: Vec<Trade> = (0..3)
        .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
        .collect();
    ledger.push(trade(4, 3, 5_000.0, ExitReason::Signal));

    assert!(
        shape_warnings(&TradeStats::from_ledger(&ledger), true).is_empty(),
        "a handful of trades has no shape to describe"
    );
}

#[test]
fn a_stop_that_never_bound_is_a_note_not_a_warning() {
    let ledger: Vec<Trade> = (0..12)
        .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
        .collect();
    let notes = shape_warnings(&TradeStats::from_ledger(&ledger), true);
    let untested = notes
        .iter()
        .find(|r| r.finding.contains("stop never bound"))
        .expect("no exit was a stop");
    assert_eq!(untested.severity, Severity::Note);
}

#[test]
fn stops_ending_almost_everything_is_a_warning() {
    let mut ledger: Vec<Trade> = (0..10)
        .map(|day| trade(day, 3, -10.0, ExitReason::Stop))
        .collect();
    ledger.push(trade(11, 3, 10.0, ExitReason::Signal));

    let warnings = shape_warnings(&TradeStats::from_ledger(&ledger), true);
    assert!(
        warnings.iter().any(|r| r.finding.contains("ended at the stop")),
        "{warnings:#?}"
    );
}

#[test]
fn severity_orders_blocking_before_warning_before_note() {
    // The sort in `recommend` relies on this, and a derived `Ord` follows
    // declaration order — which is easy to break by tidying the enum.
    let mut severities = [Severity::Note, Severity::Blocking, Severity::Warning];
    severities.sort_unstable();
    assert_eq!(
        severities,
        [Severity::Blocking, Severity::Warning, Severity::Note]
    );
}

// ---- panels ----------------------------------------------------------

/// A panel that clears every bar, so each test can break exactly one thing.
fn clean_panel() -> PanelEvidence {
    let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
    PanelEvidence {
        hypothesis: crate::HypothesisId("h".to_owned()),
        dataset: crate::DatasetRef {
            id: "bars".to_owned(),
            version: "v1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        in_sample: crate::DateRange {
            from: day(1),
            to: day(4),
        },
        out_of_sample: crate::DateRange {
            from: day(5),
            to: day(9),
        },
        selected_params: std::collections::BTreeMap::new(),
        selection: crate::Selection {
            trials: 9,
            best_sharpe: 1.5,
            expected_best_under_null: Some(0.5),
            survived_deflation: true,
            prior_trials: 0,
            scored: Vec::new(),
        },
        per_instrument: Vec::new(),
        pooled: crate::PooledOutcome {
            instruments: 4,
            total_trades: 60,
            mean_excess_return: 0.1,
            mean_return: 0.0,
            beat_benchmark: 4,
        distinct: 4,
        distinct_beat: 4,
            mean_max_drawdown: 0.1,
            worst_max_drawdown: 0.12,
        },
        breadth: None,
        book: None,
        study: None,
        criteria: None,
        ended_early: Vec::new(),
        failures: Vec::new(),
        verdict: Verdict::Supported,
        reasons: Vec::new(),
    }
}

fn findings(items: &[Recommendation]) -> String {
    items
        .iter()
        .map(|item| format!("{}: {}", item.severity.label(), item.finding))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_panel_holding_one_security_twice_is_told_so() {
    // The easiest way to have one, and the way this was found: fetch the
    // same ticker from two sources and run a panel over the pair. It read
    // "beat benchmark on 4 of 6" for what was two of three.
    let mut panel = clean_panel();
    panel.pooled.instruments = 6;
    panel.pooled.distinct = 3;

    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    let item = out
        .iter()
        .find(|item| item.finding.contains("same security more than once"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
    assert!(item.evidence.contains("6 rows covering 3"), "{}", item.evidence);
}

#[test]
fn a_panel_of_distinct_securities_is_not_warned() {
    let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
    assert!(
        !out.iter().any(|item| item.finding.contains("same security")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_panel_recorded_before_distinct_was_measured_makes_no_accusation() {
    // Zero means not measured, not "no distinct securities".
    let mut panel = clean_panel();
    panel.pooled.distinct = 0;
    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    assert!(
        !out.iter().any(|item| item.finding.contains("same security")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_panel_average_carried_by_a_minority_is_named_as_such() {
    // The finding a mean cannot express. One member carrying five and
    // every member edging ahead produce the same positive average, and
    // only one of them is a result about instruments in general.
    let mut panel = clean_panel();
    panel.pooled.beat_benchmark = 1;

    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    assert!(
        out.iter().any(|item| item.finding.contains("minority")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_panel_where_most_members_won_says_nothing_about_minorities() {
    let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
    assert!(
        !out.iter().any(|item| item.finding.contains("minority")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_worst_member_through_the_ceiling_is_reported_even_when_the_mean_passed() {
    // Averaging is exactly what hides this, so the average is the wrong
    // place to notice it.
    let mut panel = clean_panel();
    panel.pooled.worst_max_drawdown = 0.55;

    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    let item = out
        .iter()
        .find(|item| item.finding.contains("breached the drawdown ceiling"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
}

#[test]
fn a_panel_whose_members_fell_together_is_called_one_bet() {
    let mut panel = clean_panel();
    // A book that fell as hard as the average member: no diversification.
    panel.book = Some(crate::Metrics {
        total_return: 0.1,
        cagr: 0.1,
        max_drawdown: 0.1,
        volatility: 0.2,
        sharpe: Some(1.0),
        sortino: Some(1.2),
        calmar: Some(0.9),
        psr: None,
        trades: 60,
    });

    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    assert!(
        out.iter().any(|item| item.action.contains("one bet")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_panel_that_diversified_is_not_warned_about_it() {
    let mut panel = clean_panel();
    // Fell less than the average member did: the point of the panel.
    panel.book = Some(crate::Metrics {
        total_return: 0.1,
        cagr: 0.1,
        max_drawdown: 0.04,
        volatility: 0.2,
        sharpe: Some(1.0),
        sortino: Some(1.2),
        calmar: Some(0.9),
        psr: None,
        trades: 60,
    });

    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    assert!(
        !out.iter().any(|item| item.action.contains("one bet")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_panel_that_dropped_instruments_blocks_rather_than_warns() {
    // The ones that fail are rarely a random sample of the panel.
    let mut panel = clean_panel();
    panel.failures = vec!["MSFT.NASDAQ out-of-sample: no bars".to_owned()];

    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    let item = out
        .iter()
        .find(|item| item.finding.contains("never ran"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Blocking);
}

#[test]
fn a_clean_panel_still_says_one_finding_is_where_the_work_starts() {
    let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
    let severities: Vec<_> = out.iter().map(|item| item.severity).collect();
    assert_eq!(
        severities,
        [Severity::Warning, Severity::Note],
        "survivorship, then the note: {}",
        findings(&out)
    );
}

#[test]
fn every_panel_is_told_its_members_are_survivors() {
    // Nothing records point-in-time membership or delistings (#9), so a
    // panel that looks clean is exactly the one this matters for.
    let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
    let item = out
        .iter()
        .find(|item| item.finding.contains("still trade"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
    assert!(
        item.evidence.contains("4 securities judged over 2024-01-01 to 2024-01-09"),
        "{}",
        item.evidence
    );
}

#[test]
fn a_member_that_stopped_trading_is_named_and_counted_as_a_casualty() {
    let mut panel = clean_panel();
    panel.ended_early = vec!["SIVB.AIEX".to_owned()];
    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    let item = out
        .iter()
        .find(|item| item.finding.contains("stopped trading"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert!(item.evidence.contains("1 of 4 members ended early: SIVB.AIEX"), "{}", item.evidence);
    assert!(
        out.iter().any(|item| item.evidence.contains("1 of them a casualty")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_one_instrument_panel_is_not_called_a_universe() {
    let mut panel = clean_panel();
    panel.pooled.instruments = 1;
    panel.pooled.distinct = 1;
    let out = recommend_panel(&panel, &EvaluationCriteria::default());
    assert!(
        !out.iter().any(|item| item.finding.contains("still trade")),
        "{}",
        findings(&out)
    );
}

// ---- walk-forward ----------------------------------------------------

fn metrics(total_return: f64) -> crate::Metrics {
    crate::Metrics {
        total_return,
        cagr: total_return,
        max_drawdown: 0.1,
        volatility: 0.2,
        sharpe: Some(1.0),
        sortino: Some(1.2),
        calmar: Some(0.9),
        psr: None,
        trades: 40,
    }
}

/// A walk-forward that clears every bar.
fn clean_walk() -> WalkForwardEvidence {
    WalkForwardEvidence {
        hypothesis: crate::HypothesisId("h".to_owned()),
        template: experiment(),
        in_sample_days: 365,
        step_days: 90,
        anchored: false,
        folds: vec![],
        combined: metrics(0.3),
        benchmark: metrics(0.1),
        excess_return: 0.2,
        combined_curve: Vec::new(),
        benchmark_curve: Vec::new(),
        combined_trades: TradeStats::default(),
        stability: Vec::new(),
        folds_surviving_deflation: 5,
        grid: None,
        criteria: None,
        folds_without_trades: 0,
        verdict: Verdict::Supported,
        reasons: Vec::new(),
    }
}

fn experiment() -> crate::Experiment {
    let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
    crate::Experiment {
        id: crate::ExperimentId("x".to_owned()),
        hypothesis: crate::HypothesisId("h".to_owned()),
        instrument: "AAPL.NASDAQ".to_owned(),
        alongside: Vec::new(),
        underlying: None,
        window: crate::DateRange {
            from: day(1),
            to: day(9),
        },
        interval: arvo_data::BarInterval::DAILY,
        dataset: crate::DatasetRef {
            id: "bars".to_owned(),
            version: "v1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: crate::StrategySpec {
            name: "sma_cross".to_owned(),
            params: std::collections::BTreeMap::new(),
        },
        costs: crate::CostModel::proportional(0.0, 0.0),
        risk: crate::RiskModel::default(),
        starting_cash: 100_000.0,
        seed: 7,
    }
}

/// Five folds, because the gate is a binomial tail: all of four is one in
/// sixteen under the null and does not clear 5%, all of five is one in
/// thirty-two and does.
fn with_folds(mut walk: WalkForwardEvidence) -> WalkForwardEvidence {
    // Winners well clear of their own no-skill bars, which is what the
    // selection test reads. `folds_surviving_deflation` is carried beside
    // it for the reasons that quote it, and the two are set to agree.
    walk.folds = (0..5)
        .map(|_| crate::FamilyEvidence {
            hypothesis: crate::HypothesisId("h".to_owned()),
            in_sample: experiment().window,
            out_of_sample: experiment().window,
            selection: crate::Selection {
                trials: 9,
                best_sharpe: 1.5,
                expected_best_under_null: Some(1.0),
                survived_deflation: true,
                prior_trials: 0,
                scored: Vec::new(),
            },
            selected: experiment(),
            out_of_sample_evidence: crate::Evidence {
                hypothesis: crate::HypothesisId("h".to_owned()),
                experiment: experiment(),
                benchmark: crate::ExperimentId("b".to_owned()),
                engine: "test 1".to_owned(),
                criteria: EvaluationCriteria::default(),
                evaluation: crate::Evaluation {
                    strategy: metrics(0.3),
                    benchmark: metrics(0.1),
                    strategy_curve: Vec::new(),
                    benchmark_curve: Vec::new(),
                    strategy_trades: TradeStats::default(),
                    strategy_ledger: Vec::new(),
            dividend_gap: None,
            stress: None,
            refused_orders: crate::Refused::default(),
                    benchmark_instruments: Vec::new(),
                    excess_return: 0.2,
                    verdict: Verdict::Supported,
                    reasons: Vec::new(),
                },
            },
            failures: Vec::new(),
            verdict: Verdict::Supported,
            reasons: Vec::new(),
        
            under_conservative_costs: None,
        })
        .collect();
    walk
}

#[test]
fn a_procedure_that_selects_noise_in_most_folds_blocks_the_combined_return() {
    // The finding the combined curve cannot show: a procedure whose
    // selections are noise still draws a curve, and it looks the same.
    //
    // Expressed as margins rather than a count, because that is what the
    // test reads now. Each fold's winner landed near its own no-skill bar,
    // some a little above and some a little below, which is what a search
    // with nothing to find produces.
    let mut walk = with_folds(clean_walk());
    walk.folds_surviving_deflation = 1;
    for (index, fold) in walk.folds.iter_mut().enumerate() {
        fold.selection.expected_best_under_null = Some(1.0);
        fold.selection.best_sharpe = if index % 2 == 0 { 1.02 } else { 0.97 };
    }

    let out = recommend_walk_forward(&walk);
    let item = out
        .iter()
        .find(|item| item.finding.contains("picking noise"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Blocking);
}

#[test]
fn an_empty_fold_is_reported_as_too_short_rather_than_as_a_rule_doing_nothing() {
    let mut walk = with_folds(clean_walk());
    walk.folds_without_trades = 2;

    let out = recommend_walk_forward(&walk);
    let item = out
        .iter()
        .find(|item| item.finding.contains("never opened a position"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert!(item.action.contains("warm-up"), "{}", item.action);
}

#[test]
fn an_axis_the_search_never_settled_on_is_named_by_name() {
    let mut walk = with_folds(clean_walk());
    walk.stability = vec![crate::AxisStability {
        axis: "fast".to_owned(),
        distinct: 4,
        modal: 10.0,
        modal_share: 0.25,
    }];

    let out = recommend_walk_forward(&walk);
    assert!(
        out.iter().any(|item| item.finding.contains("`fast`")),
        "{}",
        findings(&out)
    );
}

#[test]
fn an_axis_the_search_agreed_on_is_left_alone() {
    let mut walk = with_folds(clean_walk());
    walk.stability = vec![crate::AxisStability {
        axis: "fast".to_owned(),
        distinct: 2,
        modal: 10.0,
        modal_share: 0.75,
    }];

    let out = recommend_walk_forward(&walk);
    assert!(
        !out.iter().any(|item| item.finding.contains("never settled")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_procedure_that_did_not_beat_holding_is_told_to_compare_against_one_window() {
    let mut walk = with_folds(clean_walk());
    walk.excess_return = -0.05;
    walk.verdict = Verdict::NotSupported;

    let out = recommend_walk_forward(&walk);
    assert!(
        out.iter()
            .any(|item| item.finding.contains("did not beat holding")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_walk_forward_with_no_folds_says_so_and_stops() {
    let out = recommend_walk_forward(&clean_walk());
    assert_eq!(out.len(), 1, "{}", findings(&out));
    assert_eq!(out[0].severity, Severity::Blocking);
    assert!(out[0].finding.contains("no folds"), "{}", out[0].finding);
}

#[test]
fn a_clean_walk_forward_is_still_told_it_is_one_claim() {
    let out = recommend_walk_forward(&with_folds(clean_walk()));
    assert_eq!(out.len(), 1, "{}", findings(&out));
    assert_eq!(out[0].severity, Severity::Note);
    assert!(out[0].action.contains("one claim"), "{}", out[0].action);
}

// ---- books -----------------------------------------------------------

/// A study of a two-instrument book whose ledger holds `traded`.
fn book_study(traded: &[&str]) -> FamilyEvidence {
    let mut experiment = experiment();
    experiment.alongside = vec!["MSFT.NASDAQ".to_owned()];
    let ledger: Vec<crate::Trade> = traded
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut round_trip = trade(
                u32::try_from(index).expect("small"),
                3,
                10.0,
                crate::ExitReason::Signal,
            );
            round_trip.instrument = (*name).to_owned();
            round_trip
        })
        .collect();

    let mut study = with_folds(clean_walk()).folds.remove(0);
    study.out_of_sample_evidence.experiment = experiment.clone();
    study.selected = experiment;
    study.out_of_sample_evidence.evaluation.strategy_ledger = ledger;
    study
}

#[test]
fn a_run_the_venue_refused_orders_in_blocks() {
    // A refused entry is a signal never acted on: the run measured a
    // starved strategy, not the rule.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.evaluation.refused_orders = crate::Refused {
        entries: 69,
        exits: 0,
    };
    let out = recommend(&study);
    let item = out
        .iter()
        .find(|item| item.finding.contains("refused orders"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Blocking);
    assert!(item.evidence.contains("69 entries"), "{}", item.evidence);

    study.out_of_sample_evidence.evaluation.refused_orders = crate::Refused::default();
    assert!(!recommend(&study).iter().any(|item| item.finding.contains("refused orders")));
}

#[test]
fn a_run_whose_curve_contradicts_its_ledger_blocks_before_anything_else() {
    let day = |d: u32| {
        chrono::NaiveDate::from_ymd_opt(2024, 1, d)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    };
    // The category the gap analysis had no room for: not "too little
    // evidence" but "the evidence contradicts itself". More data does not
    // fix it, so it outranks every other objection.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    let ledger = &mut study.out_of_sample_evidence.evaluation.strategy_ledger;
    ledger[0].pnl = 10.0;
    ledger[0].commission = 0.0;
    study.out_of_sample_evidence.evaluation.strategy_curve = vec![
        crate::EquityPoint {
            at: day(1),
            equity: 100_000.0,
        },
        crate::EquityPoint {
            at: day(2),
            // Nowhere near the 10.0 the ledger realised.
            equity: 190_000.0,
        },
    ];

    let out = recommend(&study);
    assert_eq!(
        out.first().map(|item| item.severity),
        Some(Severity::Blocking),
        "{}",
        findings(&out)
    );
    assert!(
        out.iter().any(|item| item.finding.contains("disagrees with itself")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_benchmark_that_could_not_hold_the_whole_book_is_reported() {
    // Both sides starve out of the same account, which keeps the
    // comparison fair and makes it about fewer instruments than the title
    // claims. Measured against the engine: three instruments and room for
    // one left the strategy holding one and buy-and-hold holding one, with
    // an excess return that reads as a statement about three.
    let mut study = book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]);
    study.out_of_sample_evidence.evaluation.benchmark_instruments =
        vec!["AAPL.NASDAQ".to_owned()];

    let out = recommend(&study);
    let item = out
        .iter()
        .find(|item| item.finding.contains("benchmark could not hold"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
    assert!(item.evidence.contains("1 of 2"), "{}", item.evidence);
}

#[test]
fn a_benchmark_that_held_everything_is_not_reported() {
    let mut study = book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]);
    study.out_of_sample_evidence.evaluation.benchmark_instruments =
        vec!["AAPL.NASDAQ".to_owned(), "MSFT.NASDAQ".to_owned()];

    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("benchmark could not hold")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_finding_recorded_before_benchmark_instruments_were_kept_makes_no_accusation() {
    // An empty list means "not recorded", not "held nothing". Reading it
    // the other way accuses every stored book of a shortage.
    let study = book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]);
    assert!(
        study
            .out_of_sample_evidence
            .evaluation
            .benchmark_instruments
            .is_empty(),
        "the fixture stands in for an older record"
    );
    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("benchmark could not hold")),
        "{}",
        findings(&out)
    );
}



/// A study whose window trends for its first half and ranges for its
/// second, with a rule that beats the benchmark in one and loses in the
/// other.
fn two_regimes() -> FamilyEvidence {
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();

    let mut benchmark = Vec::new();
    let mut strategy = Vec::new();
    let (mut market, mut equity) = (100.0, 100.0);
    for index in 0..120 {
        if index < 60 {
            market += 1.0; // goes somewhere
            equity += 2.0; // and this outruns it
        } else {
            market += if index % 2 == 0 { 3.0 } else { -3.0 }; // arrives nowhere
            equity -= 0.4; // and this bleeds
        }
        let at = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
            + chrono::Duration::days(index);
        benchmark.push(crate::EquityPoint { at, equity: market });
        strategy.push(crate::EquityPoint { at, equity });
    }

    let evaluation = &mut study.out_of_sample_evidence.evaluation;
    evaluation.strategy_curve = strategy;
    evaluation.benchmark_curve = benchmark;
    study
}

#[test]
fn a_result_earned_in_one_regime_and_lost_in_another_says_so() {
    // The finding this exists for: a rule judged across a trend and a range
    // gets one number describing neither, and the verdict above it is that
    // number.
    let out = recommend(&two_regimes());
    let item = out
        .iter()
        .find(|item| item.finding.contains("two different results"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));

    assert_eq!(item.severity, Severity::Warning);
    assert!(
        item.evidence.contains("trending up") && item.evidence.contains("ranging"),
        "it should name both regimes: {}",
        item.evidence
    );
}

#[test]
fn the_regime_split_is_a_caveat_and_never_a_filter() {
    // The property that keeps this from becoming a look-ahead machine.
    // These labels are computed after the run; selecting on them would be
    // the most flattering bias available, and the action text has to say so
    // rather than leaving a reader to infer it.
    let out = recommend(&two_regimes());
    let item = out
        .iter()
        .find(|item| item.finding.contains("two different results"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));

    assert!(
        item.action.contains("caveat, not a filter"),
        "{}",
        item.action
    );
    assert!(
        item.action.contains("look-ahead"),
        "it must name what selecting on these would be: {}",
        item.action
    );
}

#[test]
fn a_result_that_behaved_the_same_way_throughout_is_not_split() {
    // Silence is the right answer. A line on every result saying "this
    // behaved consistently" is a line that teaches a reader to skip the
    // section the day it does not.
    let out = recommend(&book_study(&["AAPL.NASDAQ"]));
    assert!(
        !out.iter().any(|item| item.finding.contains("two different results")),
        "{}",
        findings(&out)
    );
}

/// A study at `interval` whose curve lost within both sessions and gained
/// across the night between them.
fn gapped(interval: arvo_data::BarInterval) -> FamilyEvidence {
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.interval = interval;
    let point = |day: u32, hour: u32, equity: f64| crate::EquityPoint {
        at: chrono::NaiveDate::from_ymd_opt(2024, 7, day)
            .expect("valid")
            .and_hms_opt(hour, 0, 0)
            .expect("valid"),
        equity,
    };
    study.out_of_sample_evidence.evaluation.strategy_curve = vec![
        point(1, 14, 100.0),
        point(1, 19, 98.0),
        point(2, 14, 106.0),
        point(2, 19, 104.0),
    ];
    study
}

#[test]
fn an_intraday_rule_that_only_made_money_overnight_is_told_so() {
    let out = recommend(&gapped(arvo_data::BarInterval::new(
        5,
        arvo_data::IntervalUnit::Minute,
    )));
    let item = out
        .iter()
        .find(|item| item.finding.contains("while the market was closed"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
    assert!(item.evidence.contains("1 nights"), "{}", item.evidence);
}

#[test]
fn a_daily_rule_is_not_accused_of_holding_overnight() {
    // Every daily bar is a night; holding through them is the rule.
    let out = recommend(&gapped(arvo_data::BarInterval::DAILY));
    assert!(
        !out.iter().any(|item| item.finding.contains("while the market was closed")),
        "{}",
        findings(&out)
    );
}

/// A study with a measured gap of `overstatement`, against `excess`.
fn measured(excess: f64, gap: crate::DividendGap) -> FamilyEvidence {
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    let evaluation = &mut study.out_of_sample_evidence.evaluation;
    evaluation.excess_return = excess;
    evaluation.dividend_gap = Some(gap);
    study
}

fn gap(overstatement: f64, covered: usize, instruments: usize) -> crate::DividendGap {
    crate::DividendGap {
        events: 4,
        strategy_income: 120.0,
        benchmark_income: 120.0 + overstatement * 100_000.0,
        overstatement,
        adjustment: arvo_data::source::Adjustment::Split,
        covered,
        instruments,
    }
}

#[test]
fn a_measured_gap_states_the_corrected_margin_instead_of_a_rule_of_thumb() {
    // The whole point of fetching distributions. It used to say "smaller by
    // roughly the yield times the time out of the market", which nobody can
    // act on without going and finding the dividend history themselves.
    let out = recommend(&measured(0.08, gap(0.02, 1, 1)));
    let item = out
        .iter()
        .find(|item| item.finding.contains("smaller than it reads"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));

    assert_eq!(item.severity, Severity::Warning);
    assert!(item.action.contains("6.00%"), "corrected: {}", item.action);
    assert!(item.action.contains("8.00%"), "reported: {}", item.action);
    assert!(
        item.evidence.contains("4 distributions"),
        "{}",
        item.evidence
    );
    assert!(
        !out.iter().any(|item| item.finding.contains("flattered by dividends")),
        "the estimate must not run beside the measurement: {}",
        findings(&out)
    );
}

#[test]
fn a_margin_that_is_entirely_dividends_is_blocking_rather_than_a_warning() {
    // The case that changes a decision: the rule did not beat holding, and
    // the verdict above it was reached on the uncorrected figure. A warning
    // beside a passing verdict would be read as a caveat on a win.
    let out = recommend(&measured(0.01, gap(0.03, 1, 1)));
    let item = out
        .iter()
        .find(|item| item.finding.contains("entirely dividends"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));

    assert_eq!(item.severity, Severity::Blocking);
    assert!(
        item.action.contains("did not beat holding"),
        "{}",
        item.action
    );
    assert!(item.action.contains("-2.00%"), "{}", item.action);
}

#[test]
fn on_a_total_return_series_the_gap_describes_the_margin_instead_of_correcting_it() {
    // Same measurement, different claim. Subtracting here would take the
    // distributions out of a figure that already contains them and report
    // a margin smaller than the account earned — so the recommendation must
    // not quote a corrected number, and must not be Blocking on a rule that
    // did beat holding.
    let mut gap = gap(0.03, 1, 1);
    gap.adjustment = arvo_data::source::Adjustment::TotalReturn;
    let out = recommend(&measured(0.01, gap));

    let item = out
        .iter()
        .find(|item| item.finding.contains("dividends rather than timing"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));

    assert_eq!(item.severity, Severity::Note);
    assert!(item.action.contains("Do not subtract"), "{}", item.action);
    assert!(
        item.evidence.contains("4 distributions"),
        "the measurement still stands: {}",
        item.evidence
    );
    assert!(
        !out.iter().any(|item| {
            item.finding.contains("entirely dividends")
                || item.finding.contains("smaller than it reads")
        }),
        "no correction may be offered on this basis: {}",
        findings(&out)
    );
    // And the -2.00% the split-adjusted case reports must appear nowhere.
    assert!(
        !out.iter().any(|item| item.action.contains("-2.00%")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_measured_gap_too_small_to_matter_is_not_mentioned_at_all() {
    // Deliberately silent. A line saying "this bias is negligible" on every
    // result is a line that teaches a reader to skip the section the day it
    // is not — and this one is measured, so silence is a real answer rather
    // than an omission.
    let out = recommend(&measured(0.08, gap(0.0001, 1, 1)));
    assert!(
        !out.iter().any(|item| {
            item.finding.contains("dividend") || item.finding.contains("smaller than it reads")
        }),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_partly_covered_book_says_the_figure_is_a_floor() {
    // A precise, understated number presented as the answer is worse than
    // no number, because it invites belief.
    let out = recommend(&measured(0.08, gap(0.02, 1, 3)));
    let item = out
        .iter()
        .find(|item| item.finding.contains("smaller than it reads"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));

    assert!(item.evidence.contains("floor"), "{}", item.evidence);
    assert!(
        item.evidence.contains("2 of 3"),
        "names what is missing: {}",
        item.evidence
    );
}

#[test]
fn measuring_no_distributions_at_all_is_better_news_than_not_measuring() {
    // `Some(events: 0)` means the series was there and these instruments
    // pay nothing, so the excess return needs no correction. That must not
    // fall back to the estimate, which would claim an unknown bias where
    // one has been shown not to exist.
    let mut study = measured(
        0.08,
        crate::DividendGap {
            events: 0,
            strategy_income: 0.0,
            benchmark_income: 0.0,
            overstatement: 0.0,
            adjustment: arvo_data::source::Adjustment::Split,
            covered: 1,
            instruments: 1,
        },
    );
    let trades = &mut study.out_of_sample_evidence.evaluation.strategy_trades;
    trades.closed = 10;
    trades.average_holding_secs = Some(3.0 * 86_400.0);

    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("dividend")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_rule_that_sits_out_most_of_the_window_is_told_the_margin_is_flattered() {
    // The bias nothing else here can see, and it always favours the
    // strategy: prices are split-adjusted but not total-return adjusted,
    // so no dividend is paid to anything — and the benchmark holds through
    // every ex-date while the rule holds through only some.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    study.out_of_sample_evidence.experiment.window = crate::DateRange::new(
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        chrono::NaiveDate::from_ymd_opt(2024, 12, 31).expect("valid"),
    )
    .expect("ordered");
    let trades = &mut study.out_of_sample_evidence.evaluation.strategy_trades;
    trades.closed = 10;
    // Ten trades of three days each across a year: about 8% of it.
    trades.average_holding_secs = Some(3.0 * 86_400.0);

    let out = recommend(&study);
    let item = out
        .iter()
        .find(|item| item.finding.contains("flattered by dividends"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
    assert!(item.evidence.contains("8%"), "{}", item.evidence);
}

#[test]
fn a_rule_that_is_always_in_the_market_is_not_warned() {
    // It forgoes the same dividends the benchmark does, so the comparison
    // is very nearly fair and the line would be noise.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    study.out_of_sample_evidence.experiment.window = crate::DateRange::new(
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        chrono::NaiveDate::from_ymd_opt(2024, 12, 31).expect("valid"),
    )
    .expect("ordered");
    let trades = &mut study.out_of_sample_evidence.evaluation.strategy_trades;
    trades.closed = 1;
    trades.average_holding_secs = Some(365.0 * 86_400.0);

    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("flattered by dividends")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_run_that_never_closed_a_trade_makes_no_exposure_claim() {
    // Zero exposure would fire this on every run that did not trade, where
    // the trade-count bar has already said the only useful thing.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.evaluation.strategy_trades = TradeStats::default();
    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("flattered by dividends")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_sharpe_that_could_still_be_zero_is_flagged() {
    // The objection deflation cannot make: this run had no grid behind it
    // at all, and the Sharpe is still an estimate with a standard error.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(1.2);
    study.out_of_sample_evidence.evaluation.strategy.psr = Some(0.62);

    let out = recommend(&study);
    let item = out
        .iter()
        .find(|item| item.finding.contains("not distinguishable"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Warning);
    assert!(item.evidence.contains("62%"), "{}", item.evidence);
}

#[test]
fn a_well_evidenced_sharpe_is_left_alone() {
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(1.2);
    study.out_of_sample_evidence.evaluation.strategy.psr = Some(0.99);

    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("not distinguishable")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_losing_sharpe_is_not_also_told_it_is_uncertain() {
    // It has already lost on the return. Piling a weak objection onto a
    // decided question is how a list of recommendations stops being read.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(-0.4);
    study.out_of_sample_evidence.evaluation.strategy.psr = Some(0.10);

    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("not distinguishable")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_finding_recorded_before_psr_existed_makes_no_claim() {
    let mut study = book_study(&["AAPL.NASDAQ"]);
    study.out_of_sample_evidence.experiment.alongside.clear();
    study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(1.2);
    assert!(study.out_of_sample_evidence.evaluation.strategy.psr.is_none());

    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("not distinguishable")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_book_member_that_never_traded_is_named_and_blocks() {
    // Found by running two identical instruments against an account with
    // room for one: the second was denied every fill, the return looked
    // like a single-instrument run, and the finding claimed to be about
    // two instruments.
    let out = recommend(&book_study(&["AAPL.NASDAQ"]));
    let item = out
        .iter()
        .find(|item| item.finding.contains("never traded"))
        .unwrap_or_else(|| panic!("{}", findings(&out)));
    assert_eq!(item.severity, Severity::Blocking);
    assert!(
        item.evidence.contains("MSFT.NASDAQ"),
        "the silent member should be named: {}",
        item.evidence
    );
}

#[test]
fn a_book_where_everyone_traded_says_nothing_about_it() {
    let out = recommend(&book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]));
    assert!(
        !out.iter().any(|item| item.finding.contains("never traded")),
        "{}",
        findings(&out)
    );
}

#[test]
fn a_ledger_from_before_trades_named_instruments_makes_no_accusation() {
    // Every stored finding has an unnamed ledger. Reading that as "no
    // instrument traded" would accuse all of them of crowding out.
    let mut study = book_study(&["AAPL.NASDAQ"]);
    for round_trip in &mut study.out_of_sample_evidence.evaluation.strategy_ledger {
        round_trip.instrument = String::new();
    }
    let out = recommend(&study);
    assert!(
        !out.iter().any(|item| item.finding.contains("never traded")),
        "{}",
        findings(&out)
    );
}
