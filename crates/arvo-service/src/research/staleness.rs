//! Saying so when a stored finding goes stale while nobody is looking (#23).
//!
//! Staleness was already detected — History marks a finding whose data no
//! longer hashes to what produced it — but only when History is open. A fetch
//! that revised a year of bars, or a CSV replaced by hand, quietly turned
//! findings into statements about data that no longer exists, and nothing said
//! so until someone happened to look.
//!
//! # Alert, do not re-run
//!
//! Re-running a stale finding costs dozens of backtests and writes a new
//! finding nobody asked for. So this only says which went stale; re-running is
//! a decision for the person reading it, and History already has the button.
//!
//! # Once per finding
//!
//! A stale finding stays stale for good — re-running produces a new record and
//! leaves the old one as it was. Alerting on every check would be an alarm that
//! never stops, which is an alarm nobody reads. So the ids already reported are
//! remembered, and only a finding that was fresh at the last check can raise
//! one.

use std::collections::BTreeSet;
use std::path::Path;

use arvo_research::Summary;
use arvo_api::EventView;

use super::ResearchService;

/// Where the reported ids are kept, in the app data directory. Not in the
/// evidence directory, whose every `.json` is read as a finding.
pub const FILE: &str = "staleness.json";

/// How often the store is checked.
///
/// ponytail: each check re-hashes one bar file per finding. Fine for hundreds;
/// memoise fingerprints by instrument if a store grows past that.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// What one check found that had not been reported before.
#[derive(Debug, Default, PartialEq)]
pub struct Newly {
    /// Findings whose data is still there and hashes differently.
    pub changed: Vec<String>,
    /// Findings whose data is gone altogether.
    pub gone: Vec<String>,
}

/// Every stale finding now, and which of them were not stale last time.
///
/// Pure, so the rule that matters — report once, and again only after a
/// finding has been fresh — can be tested without a window or a disk.
pub fn compare(
    summaries: &[Summary],
    live: impl Fn(&Summary) -> Option<String>,
    ruleset: impl Fn(&Summary) -> Option<String>,
    reported: &BTreeSet<String>,
) -> (BTreeSet<String>, Newly) {
    let mut stale = BTreeSet::new();
    let mut newly = Newly::default();
    for summary in summaries {
        // A reported finding's data is not in the library; there is nothing
        // to compare its version against, and the view says so (ADR-0026).
        if summary.kind == "reported" {
            continue;
        }
        // A ruleset edited since the run makes the finding stale the same
        // way changed data does: the rule it measured is not the rule in
        // the file (#189). A shipped rule has no hash and never changes.
        let ruleset_changed =
            summary.ruleset_hash.as_ref().is_some_and(|ran| ruleset(summary).as_ref() != Some(ran));
        let gone = match live(summary) {
            Some(current) if current == summary.dataset_version && !ruleset_changed => continue,
            Some(_) => false,
            None => true,
        };
        stale.insert(summary.id.clone());
        if !reported.contains(&summary.id) {
            let subject = summary.subject.clone();
            if gone {
                newly.gone.push(subject);
            } else {
                newly.changed.push(subject);
            }
        }
    }
    (stale, newly)
}

/// Checks the store and returns the event to raise, if anything newly went
/// stale.
///
/// The record of what was reported is rewritten with *today's* stale set, so a
/// finding that is deleted drops out of it and one that comes back fresh is
/// eligible to be reported again. A record that cannot be read is treated as
/// empty — the cost is one repeated alert — and one that cannot be written is
/// logged, never fatal.
pub fn check(service: &ResearchService, record: &Path) -> Option<EventView> {
    let summaries = match service.memory.summaries() {
        Ok((summaries, _)) => summaries,
        Err(err) => {
            tracing::warn!(error = %err, "staleness check could not list findings");
            return None;
        }
    };
    let reported: BTreeSet<String> = std::fs::read_to_string(record)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();

    crate::rulesets::refresh_at(service.data_dir.parent().unwrap_or(&service.data_dir));
    let (stale, newly) = compare(
        &summaries,
        |summary| super::history::live_version(service, summary),
        |summary| super::StrategyPlan::ruleset_version(&summary.strategy),
        &reported,
    );

    if stale != reported {
        let written = serde_json::to_string(&stale)
            .map_err(|err| err.to_string())
            .and_then(|text| std::fs::write(record, text).map_err(|err| err.to_string()));
        if let Err(err) = written {
            tracing::warn!(error = %err, path = %record.display(), "could not record reported stale findings");
        }
    }

    (!newly.changed.is_empty() || !newly.gone.is_empty())
        .then(|| crate::events::findings_stale(&newly.changed, &newly.gone))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, version: &str) -> Summary {
        Summary {
            attachments: 0,
            id: id.to_owned(),
            recorded_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid"),
            kind: "study".to_owned(),
            subject: format!("{id}.SIM"),
            verdict: arvo_research::Verdict::NotSupported,
            hypothesis: arvo_research::HypothesisId::from("h"),
            dataset_version: version.to_owned(),
            instrument: Some(format!("{id}.SIM")),
            interval: Some(arvo_data::BarInterval::DAILY),
            alongside: Vec::new(),
            agent: None,
            origin: None,
            trials: None,
            strategy: "sma_cross".to_owned(),
            code_commit: String::new(),
            ruleset_hash: None,
        }
    }

    /// The reason this issue exists: a ruleset edited after a run leaves the
    /// finding measuring a rule the file no longer holds.
    #[test]
    fn an_edited_ruleset_makes_its_finding_stale_and_a_shipped_rule_never_is() {
        let mut ran = summary("a", "v1");
        ran.strategy = "my_cross".to_owned();
        ran.ruleset_hash = Some("h1".to_owned());
        let shipped = summary("b", "v1");
        let store = [ran, shipped];
        let same = |s: &Summary| Some(s.dataset_version.clone());

        let (stale, _) = compare(&store, same, |_| Some("h1".to_owned()), &BTreeSet::new());
        assert!(stale.is_empty(), "the file still holds what ran");

        let (stale, newly) = compare(&store, same, |_| Some("h2".to_owned()), &BTreeSet::new());
        assert_eq!(stale.len(), 1, "only the ruleset's finding: {stale:?}");
        assert!(stale.contains("a"));
        assert_eq!(newly.changed, vec!["a.SIM".to_owned()], "reported as changed, not gone");

        let (stale, _) = compare(&store, same, |_| None, &BTreeSet::new());
        assert!(stale.contains("a"), "a ruleset that is gone is a changed rule too");
        assert!(!stale.contains("b"), "a shipped rule has no file to change");
    }

    #[test]
    fn a_finding_is_reported_the_first_time_it_is_stale_and_not_again() {
        let store = [summary("a", "v1"), summary("b", "v1")];
        let live = |s: &Summary| Some(if s.id == "a" { "v2" } else { "v1" }.to_owned());

        let (stale, newly) = compare(&store, live, |_| None, &BTreeSet::new());
        assert_eq!(newly.changed, vec!["a.SIM"]);
        assert!(newly.gone.is_empty());

        let (_, again) = compare(&store, live, |_| None, &stale);
        assert_eq!(
            again,
            Newly::default(),
            "an alarm that never stops is not read"
        );
    }

    #[test]
    fn data_that_is_gone_is_told_apart_from_data_that_changed() {
        let store = [summary("a", "v1"), summary("b", "v1")];
        let live = |s: &Summary| (s.id == "a").then(|| "v2".to_owned());
        let (stale, newly) = compare(&store, live, |_| None, &BTreeSet::new());
        assert_eq!(newly.changed, vec!["a.SIM"]);
        assert_eq!(newly.gone, vec!["b.SIM"]);
        assert_eq!(stale.len(), 2);
    }

    #[test]
    fn a_finding_that_was_fresh_again_can_be_reported_again() {
        let store = [summary("a", "v1")];
        let (stale, _) = compare(&store, |_| Some("v2".to_owned()), |_| None, &BTreeSet::new());
        let (fresh, _) = compare(&store, |_| Some("v1".to_owned()), |_| None, &stale);
        assert!(fresh.is_empty(), "it is fresh, so it leaves the record");
        let (_, newly) = compare(&store, |_| Some("v3".to_owned()), |_| None, &fresh);
        assert_eq!(newly.changed, vec!["a.SIM"]);
    }

    #[test]
    fn a_fresh_store_raises_nothing() {
        let store = [summary("a", "v1")];
        let (stale, newly) = compare(&store, |_| Some("v1".to_owned()), |_| None, &BTreeSet::new());
        assert!(stale.is_empty());
        assert_eq!(newly, Newly::default());
    }
}
