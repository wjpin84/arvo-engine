//! Trade markers must land on candles that actually exist.
//!
//! # Why this is not a unit test
//!
//! It runs a real backtest, and a backtest installs Nautilus's process-wide
//! logger. `arvo-runtime` also carries a guard test asserting that
//! `init_tracing` leaves the `log` global free — a bug that shipped once and
//! killed every backtest in the app while all tests passed. Both care about
//! the same global, so whichever ran first won and the other failed. An
//! integration test gets its own process, which is the actual fix rather than
//! weakening either check.
//!
//! # What it proves that a unit test cannot
//!
//! A fill is stamped at the *close* of the bar the signal was read from; a
//! candle is stamped at its *open*. The view shifts markers back one interval
//! to line them up. The unit tests assert the shift happens; this asserts it
//! is the right shift, against the engine rather than against a model of it.
//! If Nautilus ever changed when a fill is stamped, every marker would slide
//! by one bar, the chart would render perfectly, and nothing else would
//! notice.

use arvo_research::{DateRange, EvaluationCriteria};
use arvo_runtime_lib::research::{study_for, study_view, StrategyPlan};

/// A price path that crosses in both directions, so the control strategy has
/// something to react to.
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

fn report() -> (
    arvo_research::FamilyEvidence,
    arvo_runtime_lib::research::StudyView,
) {
    let bars = sawtooth(400);
    let window = DateRange::new(
        bars.first().expect("non-empty").at.date(),
        bars.last().expect("non-empty").at.date(),
    )
    .expect("ordered");

    let library = arvo_data::InMemoryBars::new().with_instrument("AAPL.NASDAQ", bars);
    let simulation = arvo_nautilus::NautilusSimulation::new(library.clone());
    let plan = StrategyPlan::find("sma_cross").expect("the control is offered");
    let family = study_for("AAPL.NASDAQ", plan, window, "fixture");

    let found = arvo_research::run_family(&simulation, &family, &EvaluationCriteria::default())
        .expect("the fixture runs");
    let view = study_view(&found, &library, "test");
    (found, view)
}

#[test]
fn every_marker_lands_on_a_candle_that_exists() {
    let (found, view) = report();
    assert!(!view.price.is_empty(), "the report has bars to draw");
    assert!(!view.markers.is_empty(), "the fixture trades");

    let candles: std::collections::HashSet<i64> =
        view.price.iter().map(|candle| candle.time).collect();
    for marker in &view.markers {
        assert!(
            candles.contains(&marker.time),
            "marker at {} ({}) is on no candle — every entry would be drawn one bar from where \
             it happened",
            marker.time,
            marker.kind
        );
    }

    // And they account for the ledger exactly: two per closed round trip, one
    // for a position left open at the end.
    let ledger = &found.out_of_sample_evidence.evaluation.strategy_ledger;
    let closed = ledger.iter().filter(|trade| trade.closed.is_some()).count();
    assert_eq!(view.markers.len(), ledger.len() + closed);
}

#[test]
fn the_price_chart_covers_the_period_that_was_judged() {
    // Not the whole span. A chart showing the selection period beside the
    // judged one, with trades only on the second half, invites exactly the
    // confusion the split exists to prevent.
    let (found, view) = report();
    let judged_from = found
        .out_of_sample
        .from
        .and_time(chrono::NaiveTime::MIN)
        .and_utc()
        .timestamp();
    assert_eq!(view.price.first().expect("non-empty").time, judged_from);
}

#[test]
fn the_search_surface_is_the_whole_grid_not_only_its_winner() {
    // Reporting the maximum alone shows a broad good region and a single lucky
    // cell identically. The surface is what tells them apart, and it has to
    // come out of a real search rather than a fixture to prove the scores
    // survive selection.
    let (found, view) = report();
    let surface = view.surface.expect("the workbench grid varies fast and slow");

    assert_eq!(
        surface.cells.len(),
        found.selection.trials,
        "every configuration that ran has a cell"
    );
    assert_eq!(surface.x_values.len() * surface.y_values.len(), 9);
    assert_eq!(
        surface.cells.iter().filter(|cell| cell.selected).count(),
        1,
        "exactly one cell is the winner"
    );

    // And the shading anchor is the deflation bar the verdict already used, so
    // the picture and the verdict cannot disagree about what counts as good.
    assert_eq!(surface.null_bar, found.selection.expected_best_under_null);
    let best = surface
        .cells
        .iter()
        .find(|cell| cell.selected)
        .expect("a winner");
    assert_eq!(best.above_null, found.selection.survived_deflation);
}
