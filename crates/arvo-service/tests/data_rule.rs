//! A ruleset over a rule written as data (#225) runs the same search as the
//! same ruleset over the compiled control, and selects the same answer.
//!
//! The family runner derives one experiment per grid point from the ruleset's
//! template; the definition has to ride along on every one of them, or every
//! trial fails as an unknown strategy and the study says nothing.

use arvo_research::DateRange;
use arvo_service::research::{offerable, set_contributed, set_project_rules, study_for, StrategyPlan};

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

fn grid_over(rule: &str) -> arvo_research::StrategyDocument {
    serde_json::from_str(&format!(
        r#"{{
      "name": "grid_over_{rule}",
      "label": "the search",
      "premise": "the same search on both",
      "interval": {{ "step": 1, "unit": "day" }},
      "kind": {{ "kind": "grid", "rule": "{rule}", "fixed": {{}}, "axes": {{ "fast": [5, 10], "slow": [20, 30] }} }}
    }}"#
    ))
    .expect("a ruleset")
}

fn daily(days: usize) -> Vec<arvo_data::Bar> {
    let start = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).expect("a real date");
    let level = |t: f64| 100.0 * (0.004f64).mul_add(t, 1.0) * (0.06f64).mul_add((t / 2.0).sin(), 1.0);
    (0..days)
        .map(|i| {
            let t = i as f64;
            let (open, close) = (level((t - 1.0).max(0.0)), level(t));
            arvo_data::Bar {
                at: start.checked_add_days(chrono::Days::new(i as u64)).expect("in range").and_time(chrono::NaiveTime::MIN),
                open,
                high: open.max(close) * 1.004,
                low: open.min(close) * 0.996,
                close,
                volume: 1_000_000.0,
            }
        })
        .collect()
}

#[test]
fn a_ruleset_over_a_data_rule_searches_and_selects_like_the_compiled_control() {
    let twin: arvo_research::rule::RuleDefinition = serde_json::from_str(TWIN_CROSS).expect("parses");
    // The tables behind the picker are process-wide; set and read at once.
    set_project_rules(std::slice::from_ref(&twin));
    let documents = [("grid_over_twin_cross".to_owned(), grid_over("twin_cross")), ("grid_over_sma_cross".to_owned(), grid_over("sma_cross"))];
    assert_eq!(offerable(&documents[0].1).map(StrategyPlan::rule), Ok("twin_cross"));
    set_contributed(&documents);
    let over_data = StrategyPlan::find("grid_over_twin_cross").expect("offered");
    let over_compiled = StrategyPlan::find("grid_over_sma_cross").expect("offered");

    let window = DateRange::new(
        chrono::NaiveDate::from_ymd_opt(2019, 12, 1).expect("a real date"),
        chrono::NaiveDate::from_ymd_opt(2022, 12, 31).expect("a real date"),
    )
    .expect("ordered");
    let data = study_for("A.SIM", over_data, window, "fixture");
    let compiled = study_for("A.SIM", over_compiled, window, "fixture");
    assert!(data.template.strategy.rule.is_some(), "the template carries the definition");
    assert!(compiled.template.strategy.rule.is_none());
    assert_eq!(data.grid.combinations(), compiled.grid.combinations(), "the same search");

    let library = arvo_data::InMemoryBars::new().with_interval("A.SIM", arvo_data::BarInterval::DAILY, daily(1200));
    let provider = arvo_nautilus::NautilusSimulation::new(library);
    let criteria = arvo_research::EvaluationCriteria::default();
    let found_data = arvo_research::run_family(&provider, &data, &criteria).expect("every trial of the data rule runs");
    let found_compiled = arvo_research::run_family(&provider, &compiled, &criteria).expect("the control runs");

    assert_eq!(found_data.selected.strategy.params, found_compiled.selected.strategy.params, "the same parameters win");
    assert!(found_data.selected.strategy.rule.is_some(), "the finding carries the definition");
    let (ours, theirs) = (&found_data.out_of_sample_evidence.evaluation, &found_compiled.out_of_sample_evidence.evaluation);
    assert_eq!(ours.strategy_ledger.len(), theirs.strategy_ledger.len());
    assert!(!ours.strategy_ledger.is_empty(), "a crossing path trades");
    assert!((ours.strategy.total_return - theirs.strategy.total_return).abs() < 1e-9);
    assert!((ours.strategy.max_drawdown - theirs.strategy.max_drawdown).abs() < 1e-9);
    assert_eq!(found_data.verdict, found_compiled.verdict);
}
