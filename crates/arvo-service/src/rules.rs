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

/// Every rule file as the picker sees it (#225): what it says, or why it
/// cannot run. The listing a person or an agent reads before writing one.
#[must_use]
pub fn list(root: &Path) -> Vec<arvo_api::research::RuleFile> {
    read_all(root)
        .into_iter()
        .map(|(path, rule)| match rule {
            Ok(rule) => arvo_api::research::RuleFile {
                path,
                name: rule.name.clone(),
                label: rule.label.clone(),
                premise: rule.premise.clone(),
                interval: rule.interval.to_string(),
                indicators: rule.indicators.keys().cloned().collect(),
                params: rule
                    .params
                    .iter()
                    .map(|(name, value)| arvo_api::Fixed { name: name.clone(), value: *value })
                    .collect(),
                entry: rule.entry.describe(),
                exit: rule.exit.as_ref().map(arvo_research::rule::Condition::describe).unwrap_or_default(),
                version: rule.version(),
                problem: None,
            },
            Err(problem) => arvo_api::research::RuleFile { path, problem: Some(problem), ..Default::default() },
        })
        .collect()
}

/// Writes `rules/<name>.json` from a definition, refusing anything the engine
/// would not run and saying why (#225).
///
/// The name is the definition's own and must not be one of Arvo's: a rule
/// that shadowed `sma_cross` would make every finding on it ambiguous.
/// Replaces a rule of the same name, and the picker re-reads at once, so a
/// ruleset written next may name it.
///
/// # Errors
///
/// A bad name, a name Arvo already uses, a definition whose defaults cannot
/// run, or a file that cannot be written.
pub fn write(root: &Path, rule: &arvo_research::rule::RuleDefinition) -> Result<arvo_api::research::RuleFile, String> {
    let name = rule.name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("a rule name is letters, digits, _ and -".to_owned());
    }
    if crate::research::StrategyPlan::find_shipped(name).is_some() {
        return Err(format!("{name:?} is one of Arvo's own rules; choose another name"));
    }
    // What the engine will be handed, checked before anything is written.
    rule.resolve(&BTreeMap::new()).map_err(|err| err.to_string())?;
    let dir = root.join(SUBDIR);
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let path = dir.join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(rule).map_err(|err| err.to_string())?;
    std::fs::write(&path, text + "
").map_err(|err| format!("{}: {err}", path.display()))?;
    // The picker re-reads, so a ruleset written next may name this rule.
    refresh_at(root);
    let relative = format!("{SUBDIR}/{name}.json");
    list(root)
        .into_iter()
        .find(|listed| listed.path == relative)
        .ok_or_else(|| format!("{relative} was written but cannot be read back"))
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
    fn a_rule_is_written_checked_and_listed_and_a_bad_one_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let twin: RuleDefinition = serde_json::from_str(TWIN_CROSS).expect("parses");

        let written = write(dir.path(), &twin).expect("writes");
        assert_eq!(written.path, "rules/twin_cross.json");
        assert_eq!(written.version, twin.version());
        assert_eq!(written.entry, "fast crossed above slow");
        assert_eq!(written.exit, "fast crossed below slow");
        assert_eq!(written.indicators, ["fast", "slow"]);
        assert_eq!(written.problem, None);
        assert_eq!(read_one(&dir.path().join(SUBDIR).join("twin_cross.json")).expect("reads back"), twin);
        // Written, so the picker offers it and a ruleset may name it.
        assert!(StrategyPlan::find("twin_cross").is_some());

        // One of Arvo's own names would make every finding on it ambiguous.
        let mut shadow = twin.clone();
        shadow.name = "sma_cross".to_owned();
        assert!(write(dir.path(), &shadow).expect_err("refused").contains("one of Arvo's own"));
        let mut bad = twin.clone();
        bad.name = "not a name".to_owned();
        assert!(write(dir.path(), &bad).expect_err("refused").contains("letters, digits"));
        // A definition whose defaults cannot run is refused before writing.
        let mut broken = twin.clone();
        broken.name = "broken".to_owned();
        broken.params.remove("slow");
        assert!(write(dir.path(), &broken).expect_err("refused").contains("slow"));
        assert!(!dir.path().join(SUBDIR).join("broken.json").exists(), "nothing was written");

        // A file that cannot run is listed with its reason, not dropped.
        std::fs::write(dir.path().join(SUBDIR).join("torn.json"), r#"{ "name": "torn" }"#).expect("write");
        let listed = list(dir.path());
        assert_eq!(listed.len(), 2);
        let torn = listed.iter().find(|file| file.path.ends_with("torn.json")).expect("listed");
        assert!(torn.problem.as_deref().is_some_and(|why| why.contains("torn.json")), "{:?}", torn.problem);
        assert!(torn.name.is_empty(), "a file that cannot be read says nothing else about itself");
    }

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
