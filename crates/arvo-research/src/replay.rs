//! Checking that a stored finding can still be produced.
//!
//! [`Experiment`](crate::Experiment) makes a claim about itself:
//!
//! > The field list *is* the reproducibility contract: if a run's output can
//! > change without one of these changing, the record is incomplete and the
//! > missing input belongs here.
//!
//! Nothing checked it. Every finding in research memory rests on that claim,
//! and an unchecked reproducibility contract is the one bug that invalidates
//! results rather than producing wrong ones — a stored curve that cannot be
//! regenerated is not evidence, it is a screenshot.
//!
//! So: take a finding, run its experiment again, and compare what comes back
//! to what was written down.
//!
//! # What a difference means
//!
//! Three things can change between a recording and a replay, and only one of
//! them is a broken contract. Reporting them as the same thing would make the
//! check useless, so they are separated *before* the run rather than guessed
//! at afterwards:
//!
//! * **The data changed.** The dataset no longer hashes to the version that
//!   produced the record. Re-running would measure the new data, not the old
//!   claim, and any difference would be explained by that.
//! * **The engine changed.** A different engine version is a different
//!   simulator. Its answer is not evidence that the old one was wrong.
//! * **Neither changed, and the answer moved anyway.** That is the contract
//!   breaking, and it means some input that decides the result is not in
//!   `Experiment`.

use crate::{Evidence, EquityPoint, Record, SimulationProvider};

/// How far two equity curves may differ and still count as the same run.
///
/// Same engine, same machine, same inputs should be bit-identical, and the
/// honest test is exact equality. But accumulation order inside the engine's
/// own collections is not something this crate controls, and reporting a
/// last-bit difference as a broken contract is how a check earns a reputation
/// for crying wolf and stops being read.
///
/// Relative `1e-9` on an equity curve is a fraction of a cent on a six-figure
/// account — below the resolution of any decision taken from the number.
/// Above it, the record and the code genuinely disagree.
pub const TOLERANCE: f64 = 1e-9;

/// What happened when a stored finding was run again.
#[derive(Debug, Clone, PartialEq)]
pub enum Replay {
    /// Same inputs, same engine, same answer. The contract held.
    Reproduced { points: usize, trades: u32 },
    /// The data behind the finding is not the data that produced it, so
    /// re-running would measure something else. Not a failure of the record.
    DataChanged { recorded: String, current: String },
    /// A different simulator. Its disagreement is not evidence about the old
    /// one, so the run is not attempted.
    EngineChanged { recorded: String, current: String },
    /// Same inputs, same engine, different answer.
    ///
    /// The contract is broken: something that decides the result is not in
    /// [`Experiment`](crate::Experiment).
    Diverged(Divergence),
    /// Nothing to compare against, so nothing was run.
    NotReplayable { why: String },
    /// The experiment could not be re-run at all.
    Failed { error: String },
}

/// Where a replay first stopped matching, and by how much.
#[derive(Debug, Clone, PartialEq)]
pub struct Divergence {
    /// What differed, said plainly enough to appear in a report.
    pub what: String,
    /// Position in the equity curve, when the curve was what differed.
    pub at: Option<usize>,
    /// When, when the curve was what differed.
    pub when: Option<chrono::NaiveDateTime>,
    pub recorded: f64,
    pub replayed: f64,
    /// Size of the gap relative to the recorded value, so a report can tell
    /// a rounding difference from a different answer without re-deriving it.
    pub relative: f64,
}

impl Replay {
    /// Whether the finding still stands as evidence.
    ///
    /// A changed dataset or engine is not a reproduction — the claim was not
    /// re-established, it was merely not contradicted — so only
    /// [`Self::Reproduced`] counts.
    #[must_use]
    pub const fn holds(&self) -> bool {
        matches!(self, Self::Reproduced { .. })
    }
}

/// Runs a stored finding's experiment again and compares it to the record.
///
/// `current_dataset_version` is the hash of the data on disk now; `None` means
/// the dataset could not be resolved at all, which is reported rather than
/// treated as a match.
///
/// Costs exactly one engine run. A study stores the winning configuration with
/// its window already set to the period it was judged on, so replaying a
/// finding does not repeat the parameter search that found it — only the run
/// whose numbers were written down.
#[must_use]
pub fn replay(
    provider: &dyn SimulationProvider,
    record: &Record,
    current_dataset_version: Option<&str>,
) -> Replay {
    // A panel and a walk-forward are procedures, not single runs: replaying
    // one means performing the whole procedure again, so they take their own
    // paths below rather than pretending to be one experiment.
    match record {
        Record::Panel(evidence) => return replay_panel(provider, evidence, current_dataset_version),
        Record::WalkForward(evidence) => {
            return replay_walk_forward(provider, evidence, current_dataset_version)
        }
        Record::Study(_) => {}
    }

    let Some(evidence) = replayable(record) else {
        return Replay::NotReplayable {
            why: format!(
                "a {} finding does not store the curve it would be checked against",
                record.kind()
            ),
        };
    };

    // Both checks come before the run, not after: a difference either one
    // explains is not evidence about the record, and running anyway would
    // produce a divergence that reads like a broken contract.
    let recorded_version = &evidence.experiment.dataset.version;
    match current_dataset_version {
        None => {
            return Replay::DataChanged {
                recorded: recorded_version.clone(),
                current: "not on disk".to_owned(),
            }
        }
        Some(current) if current != recorded_version => {
            return Replay::DataChanged {
                recorded: recorded_version.clone(),
                current: current.to_owned(),
            }
        }
        Some(_) => {}
    }

    if provider.engine() != evidence.engine {
        return Replay::EngineChanged {
            recorded: evidence.engine.clone(),
            current: provider.engine().to_owned(),
        };
    }

    match provider.run(&evidence.experiment) {
        Err(error) => Replay::Failed {
            error: error.to_string(),
        },
        Ok(result) => compare(
            &evidence.evaluation.strategy_curve,
            evidence.evaluation.strategy.trades,
            &result.equity_curve,
            result.trades,
        ),
    }
}

/// Runs a panel's whole study again and compares what it concluded.
///
/// Costs the entire panel — the grid on every instrument in-sample, then
/// the winner and its benchmark out-of-sample on each. Far more than a study's
/// single run, and the honest price of checking a procedure rather than a
/// result.
///
/// Compared on the pooled figures rather than on curves, because those are
/// what a panel *is*: the per-instrument curves were deliberately never
/// stored, and the pooled numbers are the claim anybody read.
fn replay_panel(
    provider: &dyn SimulationProvider,
    evidence: &crate::PanelEvidence,
    current_dataset_version: Option<&str>,
) -> Replay {
    let Some(study) = &evidence.study else {
        return Replay::NotReplayable {
            why: "this panel was recorded before the study behind it was kept, so there is \
                  nothing to run again"
                .to_owned(),
        };
    };

    if let Some(changed) = data_or_engine_changed(
        provider,
        &evidence.dataset.version,
        // A panel's engine is not stored on the panel; every member ran on
        // whatever the build supplied, and a divergence caused by a different
        // engine would show up as a divergence. Named here so that is a stated
        // limitation rather than an omission.
        None,
        current_dataset_version,
    ) {
        return changed;
    }

    let found = match crate::run_panel(provider, study, &crate::EvaluationCriteria::default()) {
        Ok(found) => found,
        Err(error) => {
            return Replay::Failed {
                error: error.to_string(),
            }
        }
    };

    // Trades first: a different count means different runs, and every pooled
    // average after it would be an average of something else.
    if found.pooled.total_trades != evidence.pooled.total_trades {
        return Replay::Diverged(Divergence {
            what: "the panel traded a different number of times".to_owned(),
            at: None,
            when: None,
            recorded: f64::from(evidence.pooled.total_trades),
            replayed: f64::from(found.pooled.total_trades),
            relative: 1.0,
        });
    }

    if !agrees(evidence.pooled.mean_excess_return, found.pooled.mean_excess_return) {
        return Replay::Diverged(Divergence {
            what: "the panel's mean excess return moved".to_owned(),
            at: None,
            when: None,
            recorded: evidence.pooled.mean_excess_return,
            replayed: found.pooled.mean_excess_return,
            relative: (found.pooled.mean_excess_return - evidence.pooled.mean_excess_return).abs(),
        });
    }

    if found.selected_params != evidence.selected_params {
        return Replay::Diverged(Divergence {
            what: "the search chose a different configuration".to_owned(),
            at: None,
            when: None,
            recorded: 0.0,
            replayed: 0.0,
            relative: 1.0,
        });
    }

    Replay::Reproduced {
        points: found.pooled.instruments,
        trades: found.pooled.total_trades,
    }
}

/// Runs a walk-forward's whole procedure again and compares what it concluded.
///
/// The stitched curve is stored, but the folds are what produced it, so the
/// comparison is on the procedure's own findings: how many folds there were,
/// how many of them selected above the no-skill bar, and where the combined
/// track record ended.
fn replay_walk_forward(
    provider: &dyn SimulationProvider,
    evidence: &crate::WalkForwardEvidence,
    current_dataset_version: Option<&str>,
) -> Replay {
    let Some(grid) = &evidence.grid else {
        return Replay::NotReplayable {
            why: "this run was recorded before the grid it searched was kept, so the \
                  procedure cannot be performed again"
                .to_owned(),
        };
    };

    if let Some(changed) = data_or_engine_changed(
        provider,
        &evidence.template.dataset.version,
        None,
        current_dataset_version,
    ) {
        return changed;
    }

    let plan = crate::WalkForward {
        hypothesis: evidence.hypothesis.clone(),
        template: evidence.template.clone(),
        grid: grid.clone(),
        in_sample_days: evidence.in_sample_days,
        step_days: evidence.step_days,
        anchored: evidence.anchored,
    };

    let found =
        match crate::run_walk_forward(provider, &plan, &crate::EvaluationCriteria::default()) {
            Ok(found) => found,
            Err(error) => {
                return Replay::Failed {
                    error: error.to_string(),
                }
            }
        };

    if found.folds.len() != evidence.folds.len() {
        return Replay::Diverged(Divergence {
            what: "the procedure produced a different number of folds".to_owned(),
            at: None,
            when: None,
            // Recorded is the record's, replayed is the re-run's. These were
            // the wrong way round, so a divergence report named both numbers
            // backwards — the one branch out of three that had it inverted,
            // which is exactly the kind of thing only a test that reaches the
            // branch would find.
            recorded: evidence.folds.len() as f64,
            replayed: found.folds.len() as f64,
            relative: 1.0,
        });
    }

    if found.folds_surviving_deflation != evidence.folds_surviving_deflation {
        return Replay::Diverged(Divergence {
            what: "a different number of folds selected above the no-skill bar".to_owned(),
            at: None,
            when: None,
            recorded: evidence.folds_surviving_deflation as f64,
            replayed: found.folds_surviving_deflation as f64,
            relative: 1.0,
        });
    }

    if !agrees(evidence.combined.total_return, found.combined.total_return) {
        return Replay::Diverged(Divergence {
            what: "the combined track record ended somewhere else".to_owned(),
            at: None,
            when: None,
            recorded: evidence.combined.total_return,
            replayed: found.combined.total_return,
            relative: (found.combined.total_return - evidence.combined.total_return).abs(),
        });
    }

    Replay::Reproduced {
        points: found.folds.len(),
        trades: found.combined.trades,
    }
}

/// Whether two independently produced figures agree.
///
/// Replay's own tolerance, not `reconcile`'s: that one absorbs per-fill
/// currency rounding inside a single run, and this one absorbs the
/// accumulation order of a run performed twice. They are different quantities
/// and sharing a constant between them would tie two unrelated decisions
/// together.
fn agrees(recorded: f64, replayed: f64) -> bool {
    (replayed - recorded).abs() / recorded.abs().max(1.0) <= TOLERANCE
}

/// The two checks that come before any re-run, shared by all three kinds.
///
/// `recorded_engine` is `None` where the record does not pin one. A panel and
/// a walk-forward do not: every member ran on whatever the build supplied, so
/// an engine change shows up as a divergence rather than as its own answer.
/// Stated rather than silently skipped.
fn data_or_engine_changed(
    provider: &dyn SimulationProvider,
    recorded_dataset: &str,
    recorded_engine: Option<&str>,
    current_dataset_version: Option<&str>,
) -> Option<Replay> {
    match current_dataset_version {
        None => {
            return Some(Replay::DataChanged {
                recorded: recorded_dataset.to_owned(),
                current: "not on disk".to_owned(),
            })
        }
        Some(current) if current != recorded_dataset => {
            return Some(Replay::DataChanged {
                recorded: recorded_dataset.to_owned(),
                current: current.to_owned(),
            })
        }
        Some(_) => {}
    }

    match recorded_engine {
        Some(engine) if engine != provider.engine() => Some(Replay::EngineChanged {
            recorded: engine.to_owned(),
            current: provider.engine().to_owned(),
        }),
        _ => None,
    }
}

/// The stored evidence a study can be checked against.
///
/// Studies only: a panel and a walk-forward are procedures rather than single
/// runs, and are replayed by performing the procedure again — see
/// `replay_panel` and `replay_walk_forward`.
///
/// This used to say that neither could be checked at all, on the grounds that
/// a panel stores no member curves and a walk-forward's curve is stitched
/// rather than produced by one run. Both facts are true and neither was the
/// obstacle. The comparison targets were always there — pooled figures,
/// fold counts, the combined return. What was missing was the *inputs*: a
/// panel kept no template, instruments or grid, and a walk-forward kept no
/// grid, so nobody could re-derive the run that produced any of it.
fn replayable(record: &Record) -> Option<&Evidence> {
    match record {
        Record::Study(evidence) => Some(&evidence.out_of_sample_evidence),
        Record::Panel(_) | Record::WalkForward(_) => None,
    }
}

fn compare(
    recorded: &[EquityPoint],
    recorded_trades: u32,
    replayed: &[EquityPoint],
    replayed_trades: u32,
) -> Replay {
    if recorded.is_empty() {
        return Replay::NotReplayable {
            why: "the finding was recorded before curves were kept, so there is \
                  nothing to compare against"
                .to_owned(),
        };
    }

    // Length first. A curve of a different length means a different number of
    // bars was seen, which explains every later difference — reporting the
    // first point that differs would name a symptom of this.
    if recorded.len() != replayed.len() {
        return Replay::Diverged(Divergence {
            what: "the replay saw a different number of bars".to_owned(),
            at: None,
            when: None,
            recorded: recorded.len() as f64,
            replayed: replayed.len() as f64,
            relative: 1.0,
        });
    }

    for (index, (was, now)) in recorded.iter().zip(replayed).enumerate() {
        if was.at != now.at {
            return Replay::Diverged(Divergence {
                what: format!(
                    "bar {index} is dated {} in the replay and {} in the record",
                    now.at, was.at
                ),
                at: Some(index),
                when: Some(was.at),
                recorded: was.equity,
                replayed: now.equity,
                relative: 1.0,
            });
        }
        // Scaled by the recorded equity, floored at one unit of account so a
        // curve passing through zero cannot divide the comparison into
        // nonsense.
        let relative = (now.equity - was.equity).abs() / was.equity.abs().max(1.0);
        if relative > TOLERANCE {
            return Replay::Diverged(Divergence {
                what: format!("the equity curve parts company at bar {index}"),
                at: Some(index),
                when: Some(was.at),
                recorded: was.equity,
                replayed: now.equity,
                relative,
            });
        }
    }

    // After the curve, because a curve that matches point for point while the
    // trade count moved is the more surprising finding of the two, and it
    // would be hidden if the count were checked first.
    if recorded_trades != replayed_trades {
        return Replay::Diverged(Divergence {
            what: "the same equity curve was produced by a different number of trades"
                .to_owned(),
            at: None,
            when: None,
            recorded: f64::from(recorded_trades),
            replayed: f64::from(replayed_trades),
            relative: 1.0,
        });
    }

    Replay::Reproduced {
        points: recorded.len(),
        trades: recorded_trades,
    }
}

#[cfg(test)]
mod tests {
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
}
