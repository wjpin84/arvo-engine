//! Deflation across everything an agent has run (#25).
//!
//! [`expected_best_under_null`](crate::family::expected_best_under_null)
//! deflates the grid *inside* one finding. An agent running study after study
//! until one comes out `Supported` is a search nothing counts — and invisibly
//! so, because each study was honestly deflated against its own nine trials.
//! The winner of twenty studies is the best of a hundred and eighty draws.
//!
//! So an agent's finding is held to the bar for every configuration that agent
//! has tried: its prior findings' searches and this one's, pooled.
//!
//! # What this assumes, in the safe direction
//!
//! Pooled scores come from different instruments, windows and rules, and the
//! estimator treats them as independent draws from one spread. Neither is
//! true. Both make the count larger than the effective search, so the bar is
//! harder to clear than it strictly should be — the same stated margin
//! [`expected_best_under_null`](crate::family::expected_best_under_null)
//! keeps inside a grid.
//!
//! # A configuration run again is one try (ADR-0040)
//!
//! The pool counts configurations, not runs. The same rule with the same
//! settings on the same instruments at the same resolution, run twice, is
//! one draw: re-running it a week later reads nearly the same bars and
//! scores nearly the same, and two rulesets that are copies of each other
//! search the same grid. So a configuration seen before in the agent's
//! history is not counted again. A walk-forward's folds stay apart, by
//! their position: each fold re-selects on its own window, and those are
//! different tries.
//!
//! # A confirmation is judged alone (ADR-0040)
//!
//! A confirmation re-runs one finding's chosen configuration on bars that
//! finding never saw. It searched nothing, so there is no selection for the
//! agent's history to explain, and it is not held to the pooled bar.
//!
//! # What this does not do
//!
//! Count a person's searches. The issue is agents, whose loop is unbounded and
//! unwatched; a person comparing findings is already held to a bar by
//! `compare_records`.

use serde::{Deserialize, Serialize};

use crate::family::expected_best_of;
use crate::memory::{Record, StoredRecord};
use crate::walk_forward::selection_beat_chance;

/// The bar an agent's whole search held one finding to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSearch {
    /// Findings the agent had recorded before this one.
    pub prior_findings: usize,
    /// Configurations tried across those findings and this one.
    pub trials: usize,
    /// How good the best of `trials` would look with no skill. `None` when
    /// there was too little spread to say, which — as inside a grid — does
    /// not refuse.
    pub expected_best_under_null: Option<f64>,
    /// Whether the finding cleared it. A finding that did not is
    /// `NotSupported`, whatever it said before.
    pub survived: bool,
}

/// Judges `record` against every search `agent` has recorded in `history`,
/// refusing it if it does not clear the pooled bar.
///
/// Only ever downgrades, the same rule [`crate::family::run_split`] follows:
/// a finding that failed its own deflation is not rescued by a lenient pool.
pub fn deflate_for_agent(
    record: &mut Record,
    agent: &str,
    history: &[StoredRecord],
) -> AgentSearch {
    let prior: Vec<&Record> = history
        .iter()
        .filter(|stored| stored.author.agent() == Some(agent))
        .map(|stored| &stored.record)
        .collect();

    if matches!(&*record, Record::Study(found) if found.confirms.is_some()) {
        return AgentSearch { prior_findings: prior.len(), trials: 1, expected_best_under_null: None, survived: true };
    }

    let (trials, bar) = {
        let mut counted = std::collections::BTreeSet::new();
        let mut trials = 0;
        let mut sharpes = Vec::new();
        for found in prior.iter().copied().chain(std::iter::once(&*record)) {
            let (rule, fixed, on) = configuration(found);
            for (position, selection) in found.selections().into_iter().enumerate() {
                // A finding recorded before its surface was kept counts what
                // ran, as it always did: there is nothing to compare.
                trials += selection.trials.saturating_sub(selection.scored.len());
                for trial in &selection.scored {
                    let mut params = fixed.clone();
                    params.extend(trial.params.iter().map(|(name, value)| (name.clone(), *value)));
                    let settings: Vec<String> = params.iter().map(|(name, value)| format!("{name}={value}")).collect();
                    if counted.insert(format!("{rule}|{on}|{position}|{}", settings.join(","))) {
                        trials += 1;
                        sharpes.push(trial.sharpe);
                    }
                }
            }
        }
        (trials, expected_best_of(&sharpes, trials))
    };

    // Never looser than the bar the finding already met on its own.
    let tighter = |own: Option<f64>| match (own, bar) {
        (Some(own), Some(bar)) => Some(own.max(bar)),
        (own, bar) => own.or(bar),
    };
    let survived = match &*record {
        Record::Study(found) => clears(
            found.selection.best_sharpe,
            tighter(found.selection.expected_best_under_null),
        ),
        Record::Panel(found) => clears(
            found.selection.best_sharpe,
            tighter(found.selection.expected_best_under_null),
        ),
        // A reported finding declares its trials and nothing it scored, so
        // its own bar is none and the agent's history sets the whole of it.
        Record::Reported(found) => clears(
            found.selection.best_sharpe,
            tighter(found.selection.expected_best_under_null),
        ),
        Record::WalkForward(found) => {
            // The same margin test the walk-forward was judged by, with each
            // fold's bar raised to the agent's. A fold with no bar of its own
            // had no search to speak of and stays out of it.
            let mut folds = found.folds.clone();
            for fold in &mut folds {
                let own = fold.selection.expected_best_under_null;
                if own.is_some() {
                    fold.selection.expected_best_under_null = tighter(own);
                }
            }
            bar.is_none() || selection_beat_chance(&folds)
        }
    };

    if !survived {
        record.refuse(format!(
            "{agent} has tried {trials} configurations across {} findings; the best of that many \
             with no skill would be expected to reach an in-sample Sharpe of {:.3}, and this \
             finding's selection did not clear it — it is the survivor of the agent's search, \
             not evidence",
            prior.len() + 1,
            bar.unwrap_or_default(),
        ));
    }

    AgentSearch {
        prior_findings: prior.len(),
        trials,
        expected_best_under_null: bar,
        survived,
    }
}

/// What makes two tries the same try: the rule, its fixed settings, and the
/// instruments and resolution it ran on. The window is left out on purpose:
/// the same configuration re-run on a later window reads nearly the same bars.
fn configuration(record: &Record) -> (String, std::collections::BTreeMap<String, f64>, String) {
    let of = |experiment: &crate::Experiment, on: Vec<String>| {
        (experiment.strategy.name.clone(), experiment.strategy.params.clone(), format!("{}@{}", on.join(","), experiment.interval))
    };
    match record {
        Record::Study(found) => of(&found.selected, found.selected.instruments()),
        Record::Panel(found) => found
            .study
            .as_ref()
            .map_or_else(|| (String::new(), Default::default(), String::new()), |study| of(&study.template, study.instruments.clone())),
        Record::WalkForward(found) => of(&found.template, found.template.instruments()),
        Record::Reported(found) => of(&found.reported.experiment, found.reported.experiment.instruments()),
    }
}

fn clears(best: f64, bar: Option<f64>) -> bool {
    bar.is_none_or(|bar| best > bar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::{expected_best_under_null, ScoredTrial};
    use crate::memory::{tests::study, Author};
    use crate::Verdict;
    use chrono::{DateTime, Utc};

    fn at(second: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + second, 0).expect("valid")
    }

    /// A study whose own grid scored `scores`, recorded as `Supported` if the
    /// winner cleared its own bar.
    fn scored_study(scores: &[f64]) -> Record {
        let mut record = study("MSFT.NASDAQ", "v1");
        let Record::Study(found) = &mut record else {
            unreachable!("study() builds a study")
        };
        let bar = expected_best_under_null(scores);
        found.selection.trials = scores.len();
        found.selection.best_sharpe = scores.iter().copied().fold(f64::MIN, f64::max);
        found.selection.expected_best_under_null = bar;
        found.selection.survived_deflation = clears(found.selection.best_sharpe, bar);
        // Each fixture study searches its own settings, so no two are the
        // same configuration run again: a real grid's trials differ.
        static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as f64;
        found.selection.scored = scores
            .iter()
            .enumerate()
            .map(|(trial, sharpe)| ScoredTrial {
                params: [("run".to_owned(), run), ("trial".to_owned(), trial as f64)].into_iter().collect(),
                sharpe: *sharpe,
            })
            .collect();
        found.verdict = if found.selection.survived_deflation {
            Verdict::Supported
        } else {
            Verdict::NotSupported
        };
        record
    }

    const NOISE: [f64; 9] = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];

    /// Twenty nine-configuration studies by `agent`, each scoring `scores`.
    fn history_of(agent: &str, scores: &[f64]) -> Vec<StoredRecord> {
        (0..20)
            .map(|i| StoredRecord::by_agent(scored_study(scores), agent, &[], at(i)))
            .collect()
    }

    /// Twenty unremarkable studies by `agent`.
    fn history(agent: &str) -> Vec<StoredRecord> {
        history_of(agent, &NOISE)
    }

    /// Passes its own nine-trial bar, but is only a little ahead of the field.
    const LUCKY: [f64; 9] = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 1.0];

    #[test]
    fn the_twenty_first_study_is_held_to_the_bar_for_all_twenty_one() {
        let lucky = scored_study(&LUCKY);
        assert_eq!(
            lucky.verdict(),
            Verdict::Supported,
            "precondition: it clears its own grid's bar"
        );

        let stored = StoredRecord::by_agent(lucky, "agent-a", &history("agent-a"), at(99));
        let Author::Agent { search, .. } = &stored.author else {
            panic!("recorded as the agent's")
        };

        assert_eq!(search.prior_findings, 20);
        assert_eq!(search.trials, 21 * 9);
        assert!(
            !search.survived,
            "bar {:?}",
            search.expected_best_under_null
        );
        assert_eq!(stored.record.verdict(), Verdict::NotSupported);
    }

    #[test]
    fn a_winner_genuinely_ahead_of_the_whole_search_keeps_its_verdict() {
        // The check has to be passable, or it only ever says no.
        let clear = scored_study(&[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 3.0]);
        let stored = StoredRecord::by_agent(clear, "agent-a", &history("agent-a"), at(99));
        assert_eq!(stored.record.verdict(), Verdict::Supported);
    }

    #[test]
    fn only_this_agents_searches_are_counted() {
        let mut others = history("agent-b");
        others.push(StoredRecord::new(scored_study(&LUCKY), at(50)));

        let stored = StoredRecord::by_agent(scored_study(&LUCKY), "agent-a", &others, at(99));
        let Author::Agent { search, .. } = &stored.author else {
            panic!("recorded as the agent's")
        };
        assert_eq!(search.prior_findings, 0);
        assert_eq!(search.trials, 9);
        assert_eq!(stored.record.verdict(), Verdict::Supported);
    }

    #[test]
    fn a_finding_that_failed_its_own_grid_is_not_rescued_by_the_pool() {
        // A history of tight, low scores pools to a bar *below* this grid's
        // own. The winner clears the pool and still misses its own bar, and
        // the tighter of the two decides.
        let failed = scored_study(&NOISE);
        assert_eq!(failed.verdict(), Verdict::NotSupported, "precondition");
        let tight = [0.0, 0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08];
        let stored =
            StoredRecord::by_agent(failed, "agent-a", &history_of("agent-a", &tight), at(99));
        let Author::Agent { search, .. } = &stored.author else {
            panic!("recorded as the agent's")
        };
        let pooled = search.expected_best_under_null.expect("a bar");
        assert!(
            pooled < 0.8,
            "precondition: the pool alone would pass it, bar {pooled}"
        );
        assert!(!search.survived, "recorded as clearing a bar it did not");
        assert_eq!(stored.record.verdict(), Verdict::NotSupported);
    }

    #[test]
    fn an_agent_finding_survives_the_store_round_trip_with_its_bar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::EvidenceStore::new(dir.path());
        let stored =
            StoredRecord::by_agent(scored_study(&LUCKY), "agent-a", &history("agent-a"), at(99));
        store.save(&stored).expect("saves");
        assert_eq!(store.load().expect("loads").records[0], stored);
    }

    #[test]
    fn a_finding_recorded_before_authors_existed_reads_as_a_persons() {
        let mut value =
            serde_json::to_value(StoredRecord::new(study("A.SIM", "v1"), at(0))).expect("encodes");
        value.as_object_mut().expect("object").remove("author");
        let old: StoredRecord = serde_json::from_value(value).expect("still reads");
        assert_eq!(old.author, Author::Person);
    }

    /// The same nine settings run again, twenty times: one search of nine.
    #[test]
    fn a_configuration_run_again_is_one_try() {
        let again = |at_second| {
            let mut record = scored_study(&NOISE);
            let Record::Study(found) = &mut record else { unreachable!() };
            for (trial, scored) in found.selection.scored.iter_mut().enumerate() {
                scored.params = [("trial".to_owned(), trial as f64)].into_iter().collect();
            }
            StoredRecord::by_agent(record, "agent-a", &[], at(at_second))
        };
        let history: Vec<StoredRecord> = (0..20).map(again).collect();
        let next = again(99);
        let Author::Agent { search, .. } = &StoredRecord::by_agent(next.record, "agent-a", &history, at(100)).author else {
            unreachable!()
        };
        assert_eq!(search.trials, 9, "twenty runs of the same nine settings are nine tries, not 189");

        // Twenty different studies of nine are 189, as before.
        let different = history_of("agent-b", &NOISE);
        let Author::Agent { search, .. } = &StoredRecord::by_agent(scored_study(&NOISE), "agent-b", &different, at(100)).author else {
            unreachable!()
        };
        assert_eq!(search.trials, 189);
    }

    /// A confirmation searched nothing; the agent's history has nothing to
    /// explain in it, however long that history is.
    #[test]
    fn a_confirmation_is_judged_alone() {
        let mut confirmation = scored_study(&LUCKY);
        let Record::Study(found) = &mut confirmation else { unreachable!() };
        found.confirms = Some(crate::family::Confirms { finding: "earlier".into(), look: 1 });
        found.selection.best_sharpe = 0.1;
        let before = confirmation.verdict();
        let stored = StoredRecord::by_agent(confirmation, "agent-a", &history("agent-a"), at(100));
        let Author::Agent { search, .. } = &stored.author else { unreachable!() };
        assert!(search.survived);
        assert_eq!(search.expected_best_under_null, None);
        assert_eq!(stored.record.verdict(), before, "not downgraded by the pool");
    }
}
