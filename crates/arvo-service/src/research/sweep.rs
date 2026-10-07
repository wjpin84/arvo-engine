//! Which universe and rule nobody has run yet (ADR-0034).
//!
//! The universes shipped with #227 and sat unrun for days with every member's
//! bars already on disk. Nothing was missing and nothing was blocked; there was
//! simply no schedule that asked a question, only schedules that fetched data
//! and wrote reviews.
//!
//! # A gap filler, not a treadmill
//!
//! Every run charges its author's search history
//! ([ADR-0014](https://github.com/wjpin84/arvo-adrs/blob/main/0014-a-shared-experiment-carries-its-search.md)),
//! so a sweep that re-measured every universe nightly would raise the deflated
//! bar until nothing could clear it. This returns a pair that has **never** been
//! run and nothing else: a new universe, a new rule, or a rule whose resolution
//! newly matches a universe's. Re-measuring an existing pair against newer bars
//! is a deliberate act, because spending another draw on the search budget is a
//! judgement rather than a schedule.
//!
//! An option rule is never returned. Those trade a chain, their window is where
//! both the bars and the chain exist, and a panel over a universe is not how they
//! are run. Nor is a rule with nothing to search: a panel refuses an empty grid,
//! and what gets swept is the rulesets that give the rule one.
//!
//! # A queue, not a head
//!
//! This used to return the first unswept pair and nothing else. A pair the
//! engine refuses is never recorded, so it stayed first: for eight days the
//! job asked for the same refused panel every hour and nothing behind it ran,
//! with every universe's bars on disk. So this returns the whole queue in
//! order, and the job passes over a refusal to the next pair.

use std::collections::BTreeSet;
use std::path::Path;

use super::{ResearchService, StrategyPlan};
use crate::universes::Universe;

/// Every universe and rule with no recorded panel between them, in the order
/// they are to be tried.
///
/// Reads the whole store, which is a few dozen small files, and asks each panel
/// which universe and rule it was. A panel recorded before the rule was
/// surfaced answers with an empty name and is treated as covering nothing —
/// honest, since it cannot say what it ran.
#[must_use]
pub fn unswept(service: &ResearchService, root: &Path) -> Vec<(Universe, String)> {
    let swept = already_swept(service);

    let mut queue = Vec::new();
    for (_, universe) in crate::universes::read_all(root) {
        let Ok(universe) = universe else { continue };
        for plan in candidates(universe.interval) {
            if !swept.contains(&(universe.name.clone(), plan.name().to_owned())) {
                queue.push((universe.clone(), plan.name().to_owned()));
            }
        }
    }
    queue
}

/// Every universe and rule that already has a panel.
fn already_swept(service: &ResearchService) -> BTreeSet<(String, String)> {
    let Ok(loaded) = service.memory.load() else {
        // A store that cannot be listed is not a licence to run everything
        // again. Returning nothing swept would do exactly that, so an
        // unreadable store sweeps nothing until it can be read.
        return std::iter::once((String::new(), String::new())).collect();
    };
    loaded
        .records
        .iter()
        .filter_map(|stored| match &stored.record {
            arvo_research::Record::Panel(evidence) => {
                let universe = evidence.universe.as_ref()?.name.clone();
                // The name it was *offered* under, which is what `candidates`
                // returns: a ruleset's own name when there is one, else the
                // engine rule. The template records the engine rule, so keying
                // on that alone would count a sweep of `pullback_search` as a
                // sweep of `regime_pullback` and re-run the ruleset for ever —
                // and would also let one ruleset over a rule suppress another.
                let offered = stored.provenance.ruleset.as_ref().map_or_else(
                    || stored.record.strategy().to_owned(),
                    |ruleset| ruleset.name.clone(),
                );
                (!offered.is_empty()).then_some((universe, offered))
            }
            _ => None,
        })
        .collect()
}

/// The rules worth sweeping a universe at `interval` with: every shipped and
/// project rule defined at that resolution that a panel can run.
fn candidates(interval: arvo_data::BarInterval) -> Vec<&'static StrategyPlan> {
    super::offered().into_iter().filter(|plan| sweepable(plan, interval)).collect()
}

/// Whether a panel over a universe at `interval` can run `plan`: defined at
/// that resolution, not an option rule, and with something to search. A
/// project rule written with no values to vary has an empty grid, which a
/// panel refuses; its rulesets are the candidates, not the rule.
fn sweepable(plan: &StrategyPlan, interval: arvo_data::BarInterval) -> bool {
    !plan.trades_options() && plan.interval() == interval && !plan.axes.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect of 2026-09-29: a project rule with no search was offered,
    /// refused as an empty grid, never recorded, and so offered for ever.
    #[test]
    fn a_rule_with_nothing_to_search_is_not_offered_to_a_panel() {
        let fixed = StrategyPlan { name: "fixed", rule: None, label: "", premise: "", fixed: &[("period", 20.0)], axes: &[], intraday: false, options: false };
        let searched = StrategyPlan { axes: &[("period", &[10.0, 20.0])], ..fixed };
        assert!(!sweepable(&fixed, arvo_data::BarInterval::DAILY));
        assert!(sweepable(&searched, arvo_data::BarInterval::DAILY));
        for plan in candidates(arvo_data::BarInterval::DAILY) {
            assert!(!plan.axes.is_empty(), "{} has an empty grid", plan.name());
        }
    }

    /// The queue is every unswept pair, so the job has somewhere to go after
    /// a refusal.
    #[test]
    fn the_queue_holds_every_unswept_pair_of_every_universe() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(crate::universes::SUBDIR)).expect("mkdir");
        for name in ["one", "two"] {
            let text = format!(r#"{{ "name": "{name}", "reason": "a test", "interval": {{"step":1,"unit":"day"}}, "instruments": ["SPY.YF", "QQQ.YF"] }}"#);
            std::fs::write(dir.path().join(crate::universes::SUBDIR).join(format!("{name}.json")), text).expect("write");
        }
        let service = ResearchService::new(dir.path().join("data"), dir.path().join("evidence"));
        let queue = unswept(&service, dir.path());
        let daily = candidates(arvo_data::BarInterval::DAILY).len();
        assert!(daily > 1, "the build ships more than one daily rule");
        assert_eq!(queue.len(), 2 * daily, "every rule for both universes, not the first pair alone");
        assert_eq!(queue[0].0.name, "one");
        assert_eq!(queue[daily].0.name, "two");
    }

    /// The rules offered for a daily universe are daily and are not options.
    #[test]
    fn a_daily_universe_is_swept_with_daily_rules_and_no_option_rules() {
        let daily = candidates(arvo_data::BarInterval::DAILY);
        assert!(!daily.is_empty(), "the build ships daily rules");
        for plan in &daily {
            assert_eq!(plan.interval(), arvo_data::BarInterval::DAILY);
            assert!(!plan.trades_options(), "{} trades a chain", plan.name());
        }

        // And an intraday universe gets the intraday ones, which are different
        // rules rather than the same rules at another resolution.
        let intraday = candidates(arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute));
        let daily_names: BTreeSet<&str> = daily.iter().map(|plan| plan.name()).collect();
        for plan in &intraday {
            assert!(
                !daily_names.contains(plan.name()),
                "{} cannot be both a daily and an intraday rule",
                plan.name()
            );
        }
    }
}
