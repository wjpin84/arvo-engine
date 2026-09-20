//! Rulesets: strategy documents that live in the project.
//!
//! A ruleset is a `StrategyDocument` — a rule the engine implements, the
//! parameters it is fixed at, the axes a study searches — as a JSON file in
//! `<project>/rulesets/`. It is exactly what an extension contributes (#162),
//! read from the person's own folder instead: created from one of Arvo's
//! rules as a starting point, changed in the editor, and picked in the same
//! menu as everything else. A finding records the ruleset's name, so what was
//! run can be read back.
//!
//! Every file is read whenever the picker asks, so an edit applies to the
//! next study. A file the engine cannot run is not dropped silently: it is
//! listed with the reason, the same words `offerable` gives an extension.
//!
//! # Rules and rulesets
//!
//! A *rule* is what the engine implements — `sma_cross`, `momentum_breakout` —
//! and this build's rules are Rust. A *ruleset* is a rule with its numbers and
//! its search. Writing a new rule outside Rust is #125's question, which a
//! `rules` document can already state and nothing can yet run.

use std::path::Path;

use arvo_research::{StrategyDocument, StrategyKind};

use crate::research::StrategyPlan;
pub use arvo_api::{Rule, RulesetForm, Ruleset};

/// Where the files live, relative to the project folder.
pub const SUBDIR: &str = "rulesets";

/// Every ruleset file, readable or not, with what the picker makes of it.
fn read_all(root: &Path) -> Vec<(Ruleset, Option<StrategyDocument>)> {
    let dir = root.join(SUBDIR);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let relative = format!("{SUBDIR}/{}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()));
            match read_one(&path) {
                Ok(document) => {
                    let (rule, searches) = match &document.kind {
                        StrategyKind::Grid(grid) => (grid.rule.clone(), grid.configurations()),
                        StrategyKind::Rules(_) => ("rules".to_owned(), 0),
                    };
                    let problem = crate::research::offerable(&document).err();
                    let view = Ruleset {
                        path: relative,
                        name: document.name.clone(),
                        label: document.label.clone(),
                        rule,
                        interval: document.interval.to_string(),
                        searches: arvo_api::count(searches),
                        problem,
                    };
                    (view, Some(document))
                }
                Err(problem) => (
                    Ruleset {
                        path: relative,
                        name: String::new(),
                        label: String::new(),
                        rule: String::new(),
                        interval: String::new(),
                        searches: 0,
                        problem: Some(problem),
                    },
                    None,
                ),
            }
        })
        .collect();
    found.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    found
}

pub fn read_one(path: &Path) -> Result<StrategyDocument, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let document: StrategyDocument = serde_json::from_str(&text).map_err(|err| format!("not a ruleset: {err}"))?;
    if StrategyPlan::find_shipped(&document.name).is_some() {
        return Err(format!("{:?} is one of Arvo's own names; a ruleset needs its own", document.name));
    }
    Ok(document)
}

/// Puts what the project and the extensions contribute in front of the
/// picker, together. Either alone would replace the other. Handle-free, so
/// the engine's `list_strategies` sees the project's rulesets as well.
pub fn refresh() {
    match crate::project::remembered() {
        Some(root) => refresh_at(&root),
        None => crate::research::set_contributed(&crate::extensions::contributed()),
    }
}

/// As [`refresh`], for a root the caller names: the engine and the MCP
/// server run over whichever folder they were given.
pub fn refresh_at(root: &Path) {
    let mut documents = crate::extensions::contributed();
    documents.extend(
        read_all(root)
            .into_iter()
            .filter_map(|(view, document)| document.filter(|_| view.problem.is_none()).map(|d| (view.name, d))),
    );
    crate::research::set_contributed(&documents);
}

/// Every ruleset under `root`, with any reason it cannot be run.
#[must_use]
pub fn list(root: &Path) -> Vec<Ruleset> {
    read_all(root).into_iter().map(|(view, _)| view).collect()
}

/// Writes a ruleset from its parts — one of Arvo's rules, the parameters
/// fixed, the axes searched — and answers with what the picker makes of it.
/// Refuses, and writes nothing, when the engine would not run it: the reason
/// comes back in the same words the Rulesets view shows.
///
/// An existing file of that name is replaced. It is the caller's ruleset;
/// a finding already made from it keeps what it ran.
///
/// # Errors
///
/// A bad name, a rule Arvo does not implement, or a document `offerable`
/// refuses.
pub fn write(
    root: &Path,
    name: &str,
    rule: &str,
    fixed: std::collections::BTreeMap<String, f64>,
    axes: std::collections::BTreeMap<String, Vec<f64>>,
    label: Option<String>,
    premise: Option<String>,
) -> Result<Ruleset, String> {
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("a ruleset name is letters, digits, _ and -".to_owned());
    }
    if StrategyPlan::find_shipped(name).is_some() {
        return Err(format!("{name:?} is one of Arvo's own names; choose another"));
    }
    let plan = StrategyPlan::find_shipped(rule)
        .ok_or_else(|| format!("no rule called {rule:?}; list_strategies says what there is"))?;
    let mut document = template(plan, name);
    if let Some(label) = label {
        document.label = label;
    }
    if let Some(premise) = premise {
        document.premise = premise;
    }
    let searches = {
        let StrategyKind::Grid(grid) = &mut document.kind else { unreachable!("a template is a grid") };
        grid.fixed = fixed;
        grid.axes = axes;
        grid.configurations()
    };
    crate::research::offerable(&document)?;
    let dir = root.join(SUBDIR);
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let path = dir.join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(&document).map_err(|err| err.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|err| format!("{}: {err}", path.display()))?;
    refresh_at(root);
    Ok(Ruleset {
        path: format!("{SUBDIR}/{name}.json"),
        name: name.to_owned(),
        label: document.label,
        rule: rule.to_owned(),
        interval: document.interval.to_string(),
        searches: arvo_api::count(searches),
        problem: None,
    })
}

/// The document a shipped rule would be, for a person to start from: its
/// fixed parameters and the grid a workbench study searches.
pub fn template(plan: &StrategyPlan, name: &str) -> StrategyDocument {
    StrategyDocument {
        name: name.to_owned(),
        label: format!("{} ({name})", plan.label),
        premise: plan.premise.to_owned(),
        interval: plan.interval(),
        kind: StrategyKind::Grid(arvo_research::Grid {
            rule: plan.name().to_owned(),
            fixed: plan.fixed.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect(),
            axes: plan.axes.iter().map(|(key, values)| ((*key).to_owned(), values.to_vec())).collect(),
        }),
    }
}

/// Arvo's own rules, with the numbers each takes: what a ruleset starts from.
/// A ruleset's parts, for the form. A `rules` document (#160) has no grid
/// to show and is left to the editor.
///
/// # Errors
///
/// A path outside the rulesets folder, or a file that cannot be read as a
/// grid ruleset.
pub fn read_form(root: &Path, relative: &str) -> Result<RulesetForm, String> {
    let file = root.join(relative);
    if !file.starts_with(root.join(SUBDIR)) {
        return Err(format!("{relative} is not a ruleset file"));
    }
    let document = read_one(&file)?;
    let StrategyKind::Grid(grid) = document.kind else {
        return Err("a rules document has no grid to edit here; open the file".to_owned());
    };
    let mut params: Vec<arvo_api::Param> =
        grid.fixed.into_iter().map(|(name, value)| arvo_api::Param { name, values: vec![value] }).collect();
    params.extend(grid.axes.into_iter().map(|(name, values)| arvo_api::Param { name, values }));
    params.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(RulesetForm { name: document.name, rule: grid.rule, label: document.label, premise: document.premise, params })
}

/// Writes a ruleset from the form's parts and answers with what the picker
/// makes of it. A parameter with one value is fixed; with several, searched.
///
/// # Errors
///
/// A parameter with no value, or as [`write`].
pub fn write_form(root: &Path, form: RulesetForm) -> Result<Ruleset, String> {
    let mut fixed = std::collections::BTreeMap::new();
    let mut axes = std::collections::BTreeMap::new();
    for arvo_api::Param { name: key, values } in form.params {
        match values.as_slice() {
            [] => return Err(format!("{key} has no value; give it one to fix it or several to search")),
            [one] => {
                fixed.insert(key, *one);
            }
            _ => {
                axes.insert(key, values);
            }
        }
    }
    let optional = |text: String| if text.trim().is_empty() { None } else { Some(text) };
    write(root, &form.name, &form.rule, fixed, axes, optional(form.label), optional(form.premise))
}

pub fn list_rules() -> Vec<Rule> {
    StrategyPlan::shipped()
        .iter()
        .map(|plan| Rule {
            name: plan.name().to_owned(),
            label: plan.label.to_owned(),
            premise: plan.premise.to_owned(),
            interval: plan.interval().to_string(),
            fixed: plan.fixed.iter().map(|(name, value)| arvo_api::Fixed { name: (*name).to_owned(), value: *value }).collect(),
            axes: plan
                .axes
                .iter()
                .map(|(name, values)| arvo_api::Param { name: (*name).to_owned(), values: values.to_vec() })
                .collect(),
            ranks_a_set: plan.ranks_a_set(),
            trades_options: plan.trades_options(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_from_a_shipped_rule_is_offerable_and_a_bad_file_says_why() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = StrategyPlan::find_shipped("sma_cross").expect("shipped");
        let document = template(plan, "my_cross");
        assert!(crate::research::offerable(&document).is_ok(), "a copy of a shipped rule runs");

        std::fs::create_dir_all(dir.path().join(SUBDIR)).expect("mkdir");
        std::fs::write(
            dir.path().join(SUBDIR).join("my_cross.json"),
            serde_json::to_string(&document).expect("json"),
        )
        .expect("write");
        std::fs::write(dir.path().join(SUBDIR).join("broken.json"), "{").expect("write");
        let mut stolen = template(plan, "sma_cross");
        stolen.name = "sma_cross".to_owned();
        std::fs::write(
            dir.path().join(SUBDIR).join("stolen.json"),
            serde_json::to_string(&stolen).expect("json"),
        )
        .expect("write");

        let found = read_all(dir.path());
        let by_path: std::collections::BTreeMap<_, _> = found.iter().map(|(v, _)| (v.path.as_str(), v)).collect();
        assert!(by_path["rulesets/my_cross.json"].problem.is_none());
        assert_eq!(by_path["rulesets/my_cross.json"].rule, "sma_cross");
        assert!(by_path["rulesets/my_cross.json"].searches > 1, "the shipped grid is a search");
        assert!(by_path["rulesets/broken.json"].problem.as_deref().is_some_and(|p| p.contains("not a ruleset")));
        assert!(by_path["rulesets/stolen.json"].problem.as_deref().is_some_and(|p| p.contains("Arvo's own")));
    }

    #[test]
    fn an_agent_writes_a_ruleset_only_if_the_engine_would_run_it() {
        use std::collections::BTreeMap;
        let dir = tempfile::tempdir().expect("tempdir");
        let axes = BTreeMap::from([("fast".to_owned(), vec![5.0, 10.0]), ("slow".to_owned(), vec![50.0])]);
        let fixed = BTreeMap::from([("trade_size".to_owned(), 10.0)]);

        let written = write(dir.path(), "agent_cross", "sma_cross", fixed.clone(), axes.clone(), None, None)
            .expect("a runnable grid is written");
        assert_eq!(written.searches, 2);
        assert!(dir.path().join("rulesets/agent_cross.json").exists());
        assert!(StrategyPlan::find("agent_cross").is_some(), "and it is in the picker");

        let empty = BTreeMap::from([("fast".to_owned(), vec![])]);
        let refused = write(dir.path(), "nothing", "sma_cross", fixed.clone(), empty, None, None)
            .expect_err("an empty axis searches nothing");
        assert!(refused.contains("searches nothing"), "{refused}");
        assert!(!dir.path().join("rulesets/nothing.json").exists(), "nothing was written");

        assert!(write(dir.path(), "x", "not_a_rule", fixed, axes, None, None).expect_err("no such rule").contains("no rule"));
    }
}
