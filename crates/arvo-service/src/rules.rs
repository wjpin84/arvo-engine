//! Rules written as data (#225), read from the project's `rules/` folder and
//! put in front of the picker beside Arvo's own.
//!
//! A rule file is a [`RuleDefinition`]: indicators by their TA-Lib names and
//! JSON Logic conditions over them, with defaults for every number a grid may
//! vary. A ruleset under `rulesets/` names one the way it names `sma_cross`,
//! and a finding on it carries the definition inside its experiment, so the
//! file is the author's copy and not the record's.
//!
//! Refreshed before the rulesets are, since a ruleset may name a rule that
//! was written a moment ago.

use std::collections::BTreeMap;
use std::path::Path;

use arvo_research::rule::RuleDefinition;

pub const SUBDIR: &str = "rules";

/// Every rule file under `root`, in path order: what it says, or why it
/// cannot run.
#[must_use]
pub fn read_all(root: &Path) -> Vec<(String, Result<RuleDefinition, String>)> {
    let dir = root.join(SUBDIR);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let relative = format!("{SUBDIR}/{}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()));
            (relative, read_one(&path))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Reads one rule file and checks that its defaults run.
///
/// # Errors
///
/// The file cannot be read or parsed, or names a parameter with no default,
/// a period that is not a whole number of bars, or a value nothing supplies.
pub fn read_one(path: &Path) -> Result<RuleDefinition, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let rule: RuleDefinition = serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    rule.resolve(&BTreeMap::new()).map_err(|err| err.to_string())?;
    Ok(rule)
}

/// Puts the project's rules in front of the picker. A file that cannot run
/// is left out here; [`read_all`] says why.
pub fn refresh_at(root: &Path) {
    let rules: Vec<RuleDefinition> = read_all(root).into_iter().filter_map(|(_, rule)| rule.ok()).collect();
    crate::research::set_project_rules(&rules);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::StrategyPlan;

    const TWIN_CROSS: &str = r#"{
      "name": "twin_cross",
      "label": "Moving-average crossover, as data",
      "premise": "The control, written down.",
      "interval": { "step": 1, "unit": "day" },
      "params": { "fast": 10, "slow": 30 },
      "indicators": {
        "fast": { "kind": "SMA", "period": "fast" },
        "slow": { "kind": "SMA", "period": "slow" }
      },
      "entry": { "cross_above": [ { "var": "fast" }, { "var": "slow" } ] },
      "exit":  { "cross_below": [ { "var": "fast" }, { "var": "slow" } ] }
    }"#;

    const TWIN_GRID: &str = r#"{
      "name": "twin_grid",
      "label": "The twin, searched",
      "premise": "Same search as fast_cross, on the rule as data.",
      "interval": { "step": 1, "unit": "day" },
      "kind": { "kind": "grid", "rule": "twin_cross", "fixed": {}, "axes": { "fast": [5, 10], "slow": [20, 30] } }
    }"#;

    #[test]
    fn a_rule_file_is_offered_and_a_ruleset_may_name_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(SUBDIR)).expect("rules/");
        std::fs::create_dir_all(dir.path().join(crate::rulesets::SUBDIR)).expect("rulesets/");
        std::fs::write(dir.path().join(SUBDIR).join("twin_cross.json"), TWIN_CROSS).expect("write");
        std::fs::write(dir.path().join(SUBDIR).join("broken.json"), r#"{ "name": "broken" }"#).expect("write");
        std::fs::write(dir.path().join(crate::rulesets::SUBDIR).join("twin_grid.json"), TWIN_GRID).expect("write");

        let read = read_all(dir.path());
        assert_eq!(read.len(), 2);
        assert!(read[0].1.is_err(), "broken.json is named with its reason: {:?}", read[0].1);
        assert_eq!(read[1].1.as_ref().map(|rule| rule.name.as_str()), Ok("twin_cross"));

        // The tables behind the picker are process-wide and other tests
        // replace them, so each is set and read back at once.
        let twin = read[1].1.clone().expect("read");
        crate::research::set_project_rules(std::slice::from_ref(&twin));
        let rule = StrategyPlan::find("twin_cross").expect("the rule is offered under its own name");
        assert_eq!(rule.interval(), arvo_data::BarInterval::DAILY);
        assert_eq!(rule.definition().map(|definition| definition.name.as_str()), Some("twin_cross"));
        assert!(rule.fixed.iter().any(|(name, _)| *name == "trade_size"), "Arvo's sizing convention is supplied");
        assert_eq!(StrategyPlan::ruleset_version("twin_cross"), Some(twin.version()), "a finding on the bare rule is stamped with its hash");
        assert!(crate::rulesets::list_rules().iter().any(|listed| listed.name == "twin_cross"));

        // A ruleset may name it, and a finding on the ruleset carries the definition.
        let document: arvo_research::StrategyDocument = serde_json::from_str(TWIN_GRID).expect("a ruleset");
        crate::research::set_project_rules(std::slice::from_ref(&twin));
        assert_eq!(crate::research::offerable(&document).map(StrategyPlan::rule), Ok("twin_cross"));
        crate::research::set_project_rules(std::slice::from_ref(&twin));
        crate::research::set_contributed(&[("twin_grid".to_owned(), document)]);
        let grid = StrategyPlan::find("twin_grid").expect("a ruleset over the rule is offered");
        assert_eq!(grid.rule(), "twin_cross");
        assert_eq!(grid.definition().map(|definition| definition.version()), Some(twin.version()));
    }
}
