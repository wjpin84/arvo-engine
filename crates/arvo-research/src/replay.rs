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

/// The stored evidence a finding can be checked against, if it kept any.
///
/// Only a study keeps the curve behind its numbers. A panel deliberately does
/// not — storing every member's curve would add megabytes to say what its
/// correlation matrix says in a few numbers — and a walk-forward's stitched
/// curve is assembled from folds rather than produced by one run, so neither
/// can be checked by running a single experiment. Saying so is more useful
/// than silently reporting them as fine.
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

    fn at(day: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
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
            window: DateRange {
                from: at(1).date(),
                to: at(9).date(),
            },
            interval: arvo_data::BarInterval::DAILY,
            dataset: DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
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

    #[test]
    fn a_panel_says_why_it_cannot_be_checked_instead_of_reporting_success() {
        let record = Record::Panel(Box::new(crate::PanelEvidence {
            hypothesis: HypothesisId("h".to_owned()),
            dataset: DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
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
                scored: Vec::new(),
            },
            per_instrument: Vec::new(),
            pooled: crate::PooledOutcome {
                instruments: 0,
                total_trades: 0,
                mean_excess_return: 0.0,
                beat_benchmark: 0,
                mean_max_drawdown: 0.0,
                worst_max_drawdown: 0.0,
            },
            breadth: None,
            book: None,
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
