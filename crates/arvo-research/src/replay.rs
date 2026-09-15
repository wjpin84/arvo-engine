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
mod tests;
