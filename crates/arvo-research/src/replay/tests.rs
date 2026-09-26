use super::*;
use crate::{
    CostModel, DatasetRef, DateRange, Evaluation, Experiment, ExperimentId, HypothesisId,
    Metrics, SimulationError, SimulationResult, StrategySpec,
};

const ENGINE: &str = "test 1";

/// A day offset from the start of the fixture window.
///
/// Offset arithmetic rather than a day-of-month, so a fixture longer than
/// January does not panic on an invalid date. `at(1)` is still 1 January,
/// which is what every test written against the old form assumed.
fn at(day: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
        .expect("valid")
        .and_time(chrono::NaiveTime::MIN)
        + chrono::Duration::days(i64::from(day) - 1)
}

fn curve(values: &[f64]) -> Vec<EquityPoint> {
    values
        .iter()
        .enumerate()
        .map(|(index, equity)| EquityPoint {
            at: at(u32::try_from(index).expect("small") + 1),
            equity: *equity,
        })
        .collect()
}

fn metrics(trades: u32) -> Metrics {
    Metrics {
        total_return: 0.1,
        cagr: 0.1,
        max_drawdown: 0.05,
        volatility: 0.1,
        sharpe: Some(1.0),
        sortino: Some(1.2),
        calmar: Some(0.9),
        psr: None,
        trades,
    }
}

/// A study finding whose recorded curve is `recorded`.
fn study(recorded: &[f64], trades: u32) -> Record {
    let experiment = Experiment {
        id: ExperimentId("x".to_owned()),
        hypothesis: HypothesisId("h".to_owned()),
        instrument: "AAPL.NASDAQ".to_owned(),
        alongside: Vec::new(),
        underlying: None,
        window: DateRange {
            from: at(1).date(),
            to: at(9).date(),
        },
        interval: arvo_data::BarInterval::DAILY,
        dataset: DatasetRef {
            id: "bars".to_owned(),
            version: "v1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: StrategySpec {
            rule: None,
            name: "sma_cross".to_owned(),
            params: std::collections::BTreeMap::new(),
        },
        costs: CostModel::proportional(0.0, 0.0),
        risk: crate::RiskModel::default(),
        starting_cash: 100_000.0,
        seed: 7,
    };
    let evaluation = Evaluation {
        strategy: metrics(trades),
        benchmark: metrics(1),
        strategy_curve: curve(recorded),
        benchmark_curve: Vec::new(),
        strategy_trades: crate::TradeStats::default(),
        strategy_ledger: Vec::new(),
        dividend_gap: None,
        stress: None,
        refused_orders: crate::Refused::default(),
        benchmark_instruments: Vec::new(),
        excess_return: 0.1,
        verdict: crate::Verdict::Supported,
        reasons: Vec::new(),
    };
    Record::Study(Box::new(crate::FamilyEvidence {
        hypothesis: HypothesisId("h".to_owned()),
        in_sample: experiment.window,
        out_of_sample: experiment.window,
        selection: crate::Selection {
            trials: 1,
            best_sharpe: 1.0,
            expected_best_under_null: None,
            survived_deflation: true,
            prior_trials: 0,
            scored: Vec::new(),
        },
        selected: experiment.clone(),
        out_of_sample_evidence: Evidence {
            hypothesis: HypothesisId("h".to_owned()),
            experiment,
            benchmark: ExperimentId("b".to_owned()),
            engine: ENGINE.to_owned(),
            criteria: crate::EvaluationCriteria::default(),
            evaluation,
        },
        failures: Vec::new(),
        verdict: crate::Verdict::Supported,
        reasons: Vec::new(),
    
        under_conservative_costs: None,
    
        conservative: None,
    }))
}

/// An engine that returns whatever it was built with, so the comparison
/// itself is what is under test rather than any real simulator.
struct Canned {
    engine: String,
    result: Result<(Vec<f64>, u32), String>,
}

impl Canned {
    fn returning(values: &[f64], trades: u32) -> Self {
        Self {
            engine: ENGINE.to_owned(),
            result: Ok((values.to_vec(), trades)),
        }
    }
}

impl SimulationProvider for Canned {
    fn engine(&self) -> &str {
        &self.engine
    }

    fn run(&self, experiment: &Experiment) -> Result<SimulationResult, SimulationError> {
        match &self.result {
            Err(message) => Err(SimulationError::Rejected(message.clone())),
            Ok((values, trades)) => Ok(SimulationResult {
                experiment: experiment.id.clone(),
                engine: self.engine.clone(),
                trades: *trades,
                equity_curve: curve(values),
                ledger: Vec::new(),
                refused: crate::Refused::default(),
            }),
        }
    }
}

#[test]
fn the_same_inputs_through_the_same_engine_reproduce_the_record() {
    let record = study(&[100.0, 110.0, 105.0], 4);
    let outcome = replay(&Canned::returning(&[100.0, 110.0, 105.0], 4), &record, Some("v1"));
    assert_eq!(
        outcome,
        Replay::Reproduced {
            points: 3,
            trades: 4
        }
    );
    assert!(outcome.holds());
}

#[test]
fn a_difference_too_small_to_decide_anything_is_not_a_broken_contract() {
    // Below the tolerance: a fraction of a cent on a six-figure account.
    let record = study(&[100_000.0, 110_000.0], 4);
    let outcome = replay(
        &Canned::returning(&[100_000.0, 110_000.000_000_01], 4),
        &record,
        Some("v1"),
    );
    assert!(outcome.holds(), "{outcome:?}");
}

#[test]
fn an_answer_that_moved_names_the_bar_it_moved_at() {
    let record = study(&[100.0, 110.0, 105.0], 4);
    let Replay::Diverged(divergence) =
        replay(&Canned::returning(&[100.0, 110.0, 120.0], 4), &record, Some("v1"))
    else {
        panic!("a different third point is a divergence");
    };
    assert_eq!(divergence.at, Some(2));
    assert_eq!(divergence.when, Some(at(3)));
    assert!((divergence.recorded - 105.0).abs() < 1e-12);
    assert!((divergence.replayed - 120.0).abs() < 1e-12);
}

#[test]
fn a_changed_dataset_is_reported_rather_than_run() {
    // Re-running would measure the new data, so any difference it found
    // would be explained by that and would say nothing about the record.
    let record = study(&[100.0, 110.0], 4);
    let outcome = replay(&Canned::returning(&[1.0, 2.0], 99), &record, Some("v2"));
    assert_eq!(
        outcome,
        Replay::DataChanged {
            recorded: "v1".to_owned(),
            current: "v2".to_owned()
        },
        "the wildly different canned answer must not be reported as a divergence"
    );
    assert!(!outcome.holds());
}

#[test]
fn a_dataset_that_is_gone_is_not_silently_treated_as_a_match() {
    let record = study(&[100.0, 110.0], 4);
    let outcome = replay(&Canned::returning(&[100.0, 110.0], 4), &record, None);
    assert!(matches!(outcome, Replay::DataChanged { .. }), "{outcome:?}");
}

#[test]
fn a_different_engine_is_not_evidence_about_the_old_one() {
    let record = study(&[100.0, 110.0], 4);
    let mut engine = Canned::returning(&[1.0, 2.0], 99);
    engine.engine = "test 2".to_owned();
    let outcome = replay(&engine, &record, Some("v1"));
    assert_eq!(
        outcome,
        Replay::EngineChanged {
            recorded: ENGINE.to_owned(),
            current: "test 2".to_owned()
        }
    );
}

#[test]
fn the_same_curve_from_a_different_number_of_trades_is_still_a_divergence() {
    // Easy to miss and worth catching: the equity matched, so anything
    // checking only the curve would call this reproduced.
    let record = study(&[100.0, 110.0], 4);
    let Replay::Diverged(divergence) =
        replay(&Canned::returning(&[100.0, 110.0], 9), &record, Some("v1"))
    else {
        panic!("a different trade count is a divergence");
    };
    assert!((divergence.recorded - 4.0).abs() < 1e-12);
    assert!((divergence.replayed - 9.0).abs() < 1e-12);
}

#[test]
fn a_shorter_replay_is_reported_as_a_length_difference_not_a_point() {
    // The first differing point would be a symptom; the length is cause.
    let record = study(&[100.0, 110.0, 105.0], 4);
    let Replay::Diverged(divergence) =
        replay(&Canned::returning(&[100.0, 110.0], 4), &record, Some("v1"))
    else {
        panic!("a shorter curve is a divergence");
    };
    assert_eq!(divergence.at, None);
    assert!(divergence.what.contains("number of bars"), "{}", divergence.what);
}

#[test]
fn a_finding_that_kept_no_curve_says_so_rather_than_passing() {
    let record = study(&[], 4);
    let outcome = replay(&Canned::returning(&[], 4), &record, Some("v1"));
    assert!(matches!(outcome, Replay::NotReplayable { .. }), "{outcome:?}");
    assert!(!outcome.holds());
}

/// A panel carrying a study but no usable engine still refuses honestly
/// rather than claiming a pass.
#[test]
fn a_panel_says_why_it_cannot_be_checked_instead_of_reporting_success() {
    let record = Record::Panel(Box::new(crate::PanelEvidence {
        universe: None,
        hypothesis: HypothesisId("h".to_owned()),
        dataset: DatasetRef {
            id: "bars".to_owned(),
            version: "v1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        in_sample: DateRange {
            from: at(1).date(),
            to: at(4).date(),
        },
        out_of_sample: DateRange {
            from: at(5).date(),
            to: at(9).date(),
        },
        selected_params: std::collections::BTreeMap::new(),
        selection: crate::Selection {
            trials: 1,
            best_sharpe: 1.0,
            expected_best_under_null: None,
            survived_deflation: true,
            prior_trials: 0,
            scored: Vec::new(),
        },
        per_instrument: Vec::new(),
        pooled: crate::PooledOutcome {
            instruments: 0,
            total_trades: 0,
            mean_excess_return: 0.0,
            mean_return: 0.0,
            beat_benchmark: 0,
            distinct: 0,
            distinct_beat: 0,
            mean_max_drawdown: 0.0,
            worst_max_drawdown: 0.0,
        },
        breadth: None,
        book: None,
        study: None,
        criteria: None,
        ended_early: Vec::new(),
        failures: Vec::new(),
        verdict: crate::Verdict::Inconclusive,
        reasons: Vec::new(),
    }));
    let outcome = replay(&Canned::returning(&[100.0], 0), &record, Some("v1"));
    let Replay::NotReplayable { why } = outcome else {
        panic!("a panel keeps no curve to check against");
    };
    assert!(why.contains("panel"), "{why}");
}

#[test]
fn a_panel_recorded_before_its_study_was_kept_says_so_rather_than_passing() {
    // Every panel already on disk is one of these. The record pinned the
    // dataset, the winning parameters and every outcome, and not the
    // template, the instruments, the grid or the split — so there is
    // nothing to run again, and a clean bill would be a lie about work
    // that was never done.
    let record = panel_record();
    let outcome = replay(&Canned::returning(&[100.0], 0), &record, Some("v1"));
    let Replay::NotReplayable { why } = outcome else {
        panic!("a panel with no study cannot be checked");
    };
    assert!(why.contains("before the study"), "{why}");
    assert!(!Replay::NotReplayable { why }.holds());
}

#[test]
fn a_panel_whose_data_changed_is_reported_before_the_expensive_part() {
    // Replaying a panel costs the whole grid on every instrument. Finding
    // out afterwards that the data moved would be paying for an answer
    // that was never going to mean anything.
    let mut evidence = panel_evidence();
    evidence.study = Some(panel_study());
    let record = Record::Panel(Box::new(evidence));

    let outcome = replay(&Canned::returning(&[100.0], 0), &record, Some("v2"));
    assert!(
        matches!(outcome, Replay::DataChanged { .. }),
        "{outcome:?}"
    );
}

/// A panel with nothing in it, for the paths that never reach a run.
fn panel_evidence() -> crate::PanelEvidence {
    crate::PanelEvidence {
        universe: None,
        hypothesis: HypothesisId("h".to_owned()),
        dataset: DatasetRef {
            id: "bars".to_owned(),
            version: "v1".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        in_sample: DateRange::new(at(1).date(), at(4).date()).expect("ordered"),
        out_of_sample: DateRange::new(at(5).date(), at(9).date()).expect("ordered"),
        selected_params: std::collections::BTreeMap::new(),
        selection: crate::Selection {
            trials: 1,
            best_sharpe: 1.0,
            expected_best_under_null: None,
            survived_deflation: true,
            prior_trials: 0,
            scored: Vec::new(),
        },
        per_instrument: Vec::new(),
        pooled: crate::PooledOutcome {
            instruments: 0,
            total_trades: 0,
            mean_excess_return: 0.0,
            mean_return: 0.0,
            beat_benchmark: 0,
            distinct: 0,
            distinct_beat: 0,
            mean_max_drawdown: 0.0,
            worst_max_drawdown: 0.0,
        },
        breadth: None,
        book: None,
        study: None,
        criteria: None,
        ended_early: Vec::new(),
        failures: Vec::new(),
        verdict: crate::Verdict::Inconclusive,
        reasons: Vec::new(),
    }
}

fn panel_record() -> Record {
    Record::Panel(Box::new(panel_evidence()))
}

fn panel_study() -> crate::PanelStudy {
    let Record::Study(study) = study(&[100.0, 110.0], 4) else {
        unreachable!("study() builds a study");
    };
    crate::PanelStudy::new(
        study.selected.clone(),
        vec!["AAPL.NASDAQ".to_owned()],
        crate::ParameterGrid::new(),
    )
}

#[test]
fn a_walk_forward_recorded_before_its_grid_was_kept_says_so() {
    // The record pinned the template and the cadence and not the search,
    // which is everything except the one thing that decides what each
    // fold chose.
    let record = Record::WalkForward(Box::new(walk_evidence(None)));
    let outcome = replay(&Canned::returning(&[100.0], 0), &record, Some("v1"));
    let Replay::NotReplayable { why } = outcome else {
        panic!("no grid, no procedure");
    };
    assert!(why.contains("before the grid"), "{why}");
}

#[test]
fn a_walk_forward_with_its_grid_gets_as_far_as_running() {
    // With the grid present it is no longer refused for being incomplete.
    // The canned engine cannot actually produce folds, so this asserts the
    // record stopped being the obstacle rather than that the run succeeded.
    let record = Record::WalkForward(Box::new(walk_evidence(Some(
        crate::ParameterGrid::new(),
    ))));
    let outcome = replay(&Canned::returning(&[100.0, 110.0], 4), &record, Some("v1"));
    assert!(
        !matches!(&outcome, Replay::NotReplayable { why } if why.contains("before the grid")),
        "{outcome:?}"
    );
}

fn walk_evidence(grid: Option<crate::ParameterGrid>) -> crate::WalkForwardEvidence {
    let Record::Study(study) = study(&[100.0, 110.0], 4) else {
        unreachable!("study() builds a study");
    };
    crate::WalkForwardEvidence {
        hypothesis: HypothesisId("h".to_owned()),
        template: study.selected.clone(),
        in_sample_days: 365,
        step_days: 90,
        anchored: false,
        grid,
        criteria: None,
        folds: Vec::new(),
        combined: metrics(4),
        benchmark: metrics(1),
        excess_return: 0.1,
        combined_curve: Vec::new(),
        benchmark_curve: Vec::new(),
        combined_trades: crate::TradeStats::default(),
        stability: Vec::new(),
        folds_surviving_deflation: 0,
        folds_without_trades: 0,
        verdict: crate::Verdict::Inconclusive,
        reasons: Vec::new(),
    }
}


/// A curve long enough for `Metrics::from_curve` to evaluate.
///
/// Two points is enough to compare against a record and not enough to
/// *produce* one — the panel and walk-forward procedures both score every
/// run, and scoring needs a series.
fn long_enough() -> Vec<f64> {
    (0..40).map(|i| 100.0 + f64::from(i) * 0.5).collect()
}

/// A panel study small enough to run twice in a test and real enough that
/// `run_panel` accepts it.
fn runnable_panel_study() -> crate::PanelStudy {
    let Record::Study(recorded) = study(&[100.0, 110.0], 4) else {
        unreachable!("study() builds a study");
    };
    crate::PanelStudy::new(
        recorded.selected.clone(),
        vec!["AAPL.NASDAQ".to_owned(), "MSFT.NASDAQ".to_owned()],
        crate::ParameterGrid::new().axis("fast", vec![5.0, 10.0]),
    )
}

/// Evidence produced by *actually running* the procedure.
///
/// A hand-built fixture would agree with itself by construction and prove
/// nothing about the comparison. This runs the real thing and then replays
/// it, which is the only arrangement where `Reproduced` means what it says.
fn ran_panel(provider: &Canned) -> Record {
    let evidence = crate::run_panel(
        provider,
        &runnable_panel_study(),
        &crate::EvaluationCriteria::default(),
    )
    .expect("the canned provider runs every trial");
    Record::Panel(Box::new(evidence))
}

#[test]
fn a_panel_that_still_concludes_the_same_thing_is_reproduced() {
    // The path that carries the whole value of panel replay, and the one
    // nothing reached: every test before this one checked a refusal.
    let provider = Canned::returning(&long_enough(), 4);
    let record = ran_panel(&provider);
    let Record::Panel(evidence) = &record else {
        unreachable!("ran_panel builds a panel");
    };
    let version = evidence.dataset.version.clone();

    let outcome = replay(&provider, &record, Some(&version));
    assert!(
        outcome.holds(),
        "the same procedure over the same answers must reproduce, got {outcome:?}"
    );
}

#[test]
fn a_panel_that_trades_a_different_number_of_times_has_diverged() {
    // Trades first, because a different count means different runs and
    // every pooled average after it would be an average of something else.
    let recorded_with = Canned::returning(&long_enough(), 4);
    let record = ran_panel(&recorded_with);
    let Record::Panel(evidence) = &record else {
        unreachable!("ran_panel builds a panel");
    };
    let version = evidence.dataset.version.clone();

    let now_trades_less = Canned::returning(&long_enough(), 2);
    let Replay::Diverged(divergence) = replay(&now_trades_less, &record, Some(&version)) else {
        panic!("a different trade count is a broken contract");
    };
    assert!(
        divergence.what.contains("traded a different number"),
        "{}",
        divergence.what
    );
    assert!(
        divergence.recorded > divergence.replayed,
        "recorded {} should be the record's larger count, replayed {}",
        divergence.recorded,
        divergence.replayed
    );
}

/// A walk-forward whose procedure can be performed twice.
fn runnable_walk(grid: crate::ParameterGrid) -> crate::WalkForward {
    let Record::Study(recorded) = study(&[100.0, 110.0], 4) else {
        unreachable!("study() builds a study");
    };
    let mut template = recorded.selected.clone();
    // Long enough to roll into several folds at the cadence below.
    template.window =
        DateRange::new(at(1).date(), at(1).date() + chrono::Duration::days(2_000))
            .expect("ordered");
    crate::WalkForward {
        hypothesis: template.hypothesis.clone(),
        template,
        grid,
        in_sample_days: 365,
        step_days: 365,
        anchored: false,
    }
}

/// The same shape of run ending somewhere else.
fn moved() -> Vec<f64> {
    (0..40).map(|i| 100.0 + f64::from(i) * 0.9).collect()
}

fn ran_walk_forward(provider: &Canned) -> Record {
    let plan = runnable_walk(crate::ParameterGrid::new().axis("fast", vec![5.0, 10.0]));
    let evidence =
        crate::run_walk_forward(provider, &plan, &crate::EvaluationCriteria::default())
            .expect("the canned provider runs every fold");
    Record::WalkForward(Box::new(evidence))
}

#[test]
fn a_walk_forward_that_still_concludes_the_same_thing_is_reproduced() {
    let provider = Canned::returning(&long_enough(), 4);
    let record = ran_walk_forward(&provider);
    let Record::WalkForward(evidence) = &record else {
        unreachable!("ran_walk_forward builds one");
    };
    let version = evidence.template.dataset.version.clone();

    let outcome = replay(&provider, &record, Some(&version));
    assert!(
        outcome.holds(),
        "the same procedure over the same answers must reproduce, got {outcome:?}"
    );
}

#[test]
fn a_walk_forward_whose_combined_record_moved_has_diverged() {
    let recorded_with = Canned::returning(&long_enough(), 4);
    let record = ran_walk_forward(&recorded_with);
    let Record::WalkForward(evidence) = &record else {
        unreachable!("ran_walk_forward builds one");
    };
    let version = evidence.template.dataset.version.clone();

    // Same shape of run, different answer: the contract breaking.
    let now_ends_elsewhere = Canned::returning(&moved(), 4);
    let Replay::Diverged(divergence) = replay(&now_ends_elsewhere, &record, Some(&version))
    else {
        panic!("a different combined record is a broken contract");
    };
    assert!(
        divergence.recorded != divergence.replayed,
        "the two figures must actually differ: {divergence:?}"
    );
}

#[test]
fn a_fold_count_divergence_names_the_record_as_recorded() {
    // The bug this pins: `recorded` and `replayed` were the wrong way
    // round in this one branch of three, so the report named both numbers
    // backwards. Only a test that reaches the branch can see it.
    let provider = Canned::returning(&long_enough(), 4);
    let record = ran_walk_forward(&provider);
    let Record::WalkForward(evidence) = &record else {
        unreachable!("ran_walk_forward builds one");
    };
    let recorded_folds = evidence.folds.len();
    let version = evidence.template.dataset.version.clone();

    // Re-run the same record against a plan that rolls differently, by
    // shortening the window the replay will use. Still long enough to
    // clear the three-fold minimum — below that the procedure is refused
    // outright and the answer would be `Failed`, not a divergence.
    let mut shortened = evidence.as_ref().clone();
    shortened.template.window =
        DateRange::new(at(1).date(), at(1).date() + chrono::Duration::days(1_600))
            .expect("ordered");
    let record = Record::WalkForward(Box::new(shortened));

    let Replay::Diverged(divergence) = replay(&provider, &record, Some(&version)) else {
        panic!("a different fold count is a broken contract");
    };
    assert!(
        divergence.what.contains("number of folds"),
        "{}",
        divergence.what
    );
    assert!(
        (divergence.recorded - recorded_folds as f64).abs() < 0.5,
        "recorded should be the record's {recorded_folds} folds, got {}",
        divergence.recorded
    );
}

#[test]
fn an_experiment_that_will_not_run_is_a_failure_not_a_divergence() {
    let record = study(&[100.0, 110.0], 4);
    let engine = Canned {
        engine: ENGINE.to_owned(),
        result: Err("no bars in window".to_owned()),
    };
    let Replay::Failed { error } = replay(&engine, &record, Some("v1")) else {
        panic!("an engine error is not a disagreement about the answer");
    };
    assert!(error.contains("no bars"), "{error}");
}
