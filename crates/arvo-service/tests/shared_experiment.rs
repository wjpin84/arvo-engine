//! A shared experiment, round-tripped through a real engine.
//!
//! An integration test for the same reason `chart_alignment` is one: it runs
//! Nautilus, which claims the process-wide logger.
//!
//! The claim under test is ADR-0014's second rule, end to end: a study shared
//! and run again on *the same data* scores its configurations identically, so
//! the only thing that can move its bar is the search it carried — and that
//! must move it up.

use arvo_research::{DatasetRef, DateRange, EvaluationCriteria};
use arvo_service::research::{study_for, study_view, StrategyPlan};

fn sawtooth(days: usize) -> Vec<arvo_data::Bar> {
    let mut start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid");
    let mut bars = Vec::with_capacity(days);
    for index in 0..days {
        let phase = (index % 40) as f64;
        let cycle = if phase < 20.0 { phase } else { 40.0 - phase };
        let close = 100.0 + index as f64 * 0.05 + cycle * 0.5;
        bars.push(arvo_data::Bar {
            at: start.and_time(chrono::NaiveTime::MIN),
            open: close,
            high: close + 0.5,
            low: close - 0.5,
            close,
            volume: 10_000.0,
        });
        start = start.succ_opt().expect("in range");
    }
    bars
}

#[test]
fn a_shared_study_run_again_is_held_to_its_whole_search() {
    let bars = sawtooth(400);
    let window = DateRange::new(
        bars.first().expect("non-empty").at.date(),
        bars.last().expect("non-empty").at.date(),
    )
    .expect("ordered");
    let library = arvo_data::InMemoryBars::new().with_instrument("AAPL.NASDAQ", bars);
    let simulation = arvo_nautilus::NautilusSimulation::new(library.clone());
    let plan = StrategyPlan::find("sma_cross").expect("the control is offered");
    let criteria = EvaluationCriteria::default();

    let original = arvo_research::run_family(
        &simulation,
        &study_for("AAPL.NASDAQ", plan, window, "fixture"),
        &criteria,
    )
    .expect("the fixture runs");

    let text =
        arvo_research::share::export(&arvo_research::Record::Study(Box::new(original.clone())))
            .expect("a fresh study shares")
            .to_json();
    let shared = arvo_research::share::import(&text, arvo_nautilus::STRATEGIES)
        .expect("this build reads its own file");

    let family = shared.family(
        "AAPL.NASDAQ",
        window,
        DatasetRef {
            id: "AAPL.NASDAQ".to_owned(),
            version: "fixture".to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
    );
    let again = arvo_research::run_family(&simulation, &family, &shared.criteria)
        .expect("the shared experiment runs");

    assert_eq!(
        again.selection.trials, original.selection.trials,
        "the same grid ran"
    );
    assert_eq!(
        again.selection.prior_trials, original.selection.trials,
        "and the original search came with it"
    );
    let (before, after) = (
        original.selection.expected_best_under_null.expect("a bar"),
        again.selection.expected_best_under_null.expect("a bar"),
    );
    assert!(after > before, "the bar rose: {before} -> {after}");

    let view = study_view(&again, &library, "test");
    assert_eq!(
        view.prior_trials, original.selection.trials,
        "and the report says so"
    );
}
