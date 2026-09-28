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
//! are run.

use std::collections::BTreeSet;
use std::path::Path;

use super::{ResearchService, StrategyPlan};
use crate::universes::Universe;

/// The first universe and rule with no recorded panel between them, if there is
/// one.
///
/// Reads the whole store, which is a few dozen small files, and asks each panel
/// which universe and rule it was. A panel recorded before the rule was
/// surfaced answers with an empty name and is treated as covering nothing —
/// honest, since it cannot say what it ran.
#[must_use]
pub fn next(service: &ResearchService, root: &Path) -> Option<(Universe, String)> {
    let swept = already_swept(service);

    for (_, universe) in crate::universes::read_all(root) {
        let Ok(universe) = universe else { continue };
        for plan in candidates(universe.interval) {
            if !swept.contains(&(universe.name.clone(), plan.name().to_owned())) {
                return Some((universe, plan.name().to_owned()));
            }
        }
    }
    None
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
/// project rule defined at that resolution, minus the ones that trade options.
fn candidates(interval: arvo_data::BarInterval) -> Vec<&'static StrategyPlan> {
    super::offered()
        .into_iter()
        .filter(|plan| !plan.trades_options() && plan.interval() == interval)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
