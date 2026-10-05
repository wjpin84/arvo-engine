//! A finding is stale when the bars it read have changed, not when the series
//! has grown (ADR-0036), end to end through a real engine.
//!
//! An integration test for the reason `chart_alignment` is one: it runs
//! Nautilus, which claims the process-wide logger.
//!
//! The universes job gives every member a new bar each trading day. Before
//! this, that one bar changed the member's whole-series hash, every finding
//! on it was stale by morning, and a replay of yesterday's finding answered
//! "the data changed" about bars it had never read.

use arvo_data::{BarInterval, BarProvider as _, CsvBars};
use arvo_service::research::history::{live_version, replay_record};
use arvo_service::research::study::{run_panel_over, run_study};
use arvo_service::research::{version, ResearchService};

/// `days` daily bars that climb in a sawtooth, differently for each `phase`.
fn sawtooth(days: usize, phase: usize) -> Vec<arvo_data::Bar> {
    let mut day = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid");
    let mut bars = Vec::with_capacity(days);
    for index in 0..days {
        let step = ((index + phase) % 40) as f64;
        let cycle = if step < 20.0 { step } else { 40.0 - step };
        let close = 100.0 + index as f64 * 0.05 + cycle * 0.5;
        bars.push(arvo_data::Bar {
            at: day.and_time(chrono::NaiveTime::MIN),
            open: close,
            high: close + 0.5,
            low: close - 0.5,
            close,
            volume: 10_000.0,
        });
        day = day.succ_opt().expect("in range");
    }
    bars
}

fn project() -> (tempfile::TempDir, ResearchService, CsvBars) {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data/");
    let service = ResearchService::new(data.clone(), dir.path().join("evidence"));
    (dir, service, CsvBars::new(data))
}

fn summary_of(service: &ResearchService, id: &str) -> arvo_research::Summary {
    service.memory.open(id).expect("the finding opens").summary()
}

#[test]
fn a_study_stays_fresh_as_its_series_grows_and_goes_stale_when_a_bar_it_read_changes() {
    let (_dir, service, library) = project();
    library.write("AAA.YF", BarInterval::DAILY, &sawtooth(400, 0)).expect("written");

    let study = run_study(&service, "AAA.YF", None).expect("the study runs");
    let summary = summary_of(&service, &study.id);
    let (from, to) = version::span_of(&summary.dataset_version).expect("the version names the span it covers");
    assert_eq!((from.to_string(), to.to_string()), ("2024-01-01".to_owned(), "2025-02-03".to_owned()));
    assert_eq!(live_version(&service, &summary).as_deref(), Some(summary.dataset_version.as_str()), "fresh when recorded");
    assert_eq!(replay_record(&service, &study.id).expect("replays").outcome, "reproduced");

    // The next trading day arrives, as it does every day for a universe
    // member. Nothing the study read has changed.
    library.write("AAA.YF", BarInterval::DAILY, &sawtooth(401, 0)).expect("written");
    assert_ne!(
        library.fingerprint("AAA.YF", BarInterval::DAILY).expect("reads").as_deref(),
        Some(summary.dataset_version.as_str()),
        "the file as a whole has changed"
    );
    assert_eq!(
        live_version(&service, &summary).as_deref(),
        Some(summary.dataset_version.as_str()),
        "and the finding is not stale for it"
    );
    let replayed = replay_record(&service, &study.id).expect("replays");
    assert_eq!(replayed.outcome, "reproduced", "{}", replayed.detail);
    assert!(replayed.holds);

    // A deeper fetch adds history before anything the study read: the same.
    // (Dates before the span; the study's own bars are untouched.)
    let mut deeper = sawtooth(401, 0);
    let first = deeper[0];
    deeper.insert(0, arvo_data::Bar { at: first.at - chrono::Duration::days(1), ..first });
    library.write("AAA.YF", BarInterval::DAILY, &deeper).expect("written");
    assert_eq!(live_version(&service, &summary).as_deref(), Some(summary.dataset_version.as_str()));

    // The vendor revises one bar inside the span. That is what stale means.
    let mut revised = sawtooth(401, 0);
    revised[200].close += 1.0;
    revised[200].high += 1.0;
    library.write("AAA.YF", BarInterval::DAILY, &revised).expect("written");
    let now = live_version(&service, &summary).expect("the data is still there");
    assert_ne!(now, summary.dataset_version);
    assert_eq!(version::span_of(&now), Some((from, to)), "the same span, holding different bars");
    let replayed = replay_record(&service, &study.id).expect("replays");
    assert_eq!(replayed.outcome, "data-changed", "{}", replayed.detail);

    // And a series that is gone is gone.
    std::fs::remove_file(service.data_dir.join("AAA.YF.csv")).expect("removed");
    assert_eq!(live_version(&service, &summary), None);
}

#[test]
fn a_panel_over_a_universe_is_checked_against_its_own_members() {
    let (_dir, service, library) = project();
    for (name, phase) in [("AAA.YF", 0), ("BBB.YF", 13), ("CCC.YF", 27)] {
        library.write(name, BarInterval::DAILY, &sawtooth(400, phase)).expect("written");
    }
    // In the library and in nobody's universe: the old check hashed it in
    // anyway, which is why a universe's panel was stale the moment it was
    // recorded.
    library.write("OTHER.YF", BarInterval::DAILY, &sawtooth(300, 5)).expect("written");

    let universe = arvo_service::universes::Universe {
        name: "three".to_owned(),
        reason: "three made-up series, for a test".to_owned(),
        instruments: vec!["AAA.YF".to_owned(), "BBB.YF".to_owned(), "CCC.YF".to_owned()],
        interval: BarInterval::DAILY,
        since: None,
    };
    let panel = run_panel_over(&service, &universe, None).expect("the panel runs");
    let summary = summary_of(&service, &panel.id);
    assert_eq!(summary.instrument, None, "a panel has no one instrument");
    assert_eq!(summary.alongside, universe.instruments, "it has members, and the check needs to know them");
    assert_eq!(summary.interval, Some(BarInterval::DAILY));
    assert_eq!(live_version(&service, &summary).as_deref(), Some(summary.dataset_version.as_str()), "fresh when recorded");

    // A member gains a day, and so does a series the panel never read.
    library.write("BBB.YF", BarInterval::DAILY, &sawtooth(401, 13)).expect("written");
    library.write("OTHER.YF", BarInterval::DAILY, &sawtooth(301, 5)).expect("written");
    assert_eq!(live_version(&service, &summary).as_deref(), Some(summary.dataset_version.as_str()));

    // A member's bar is revised inside the span.
    let mut revised = sawtooth(401, 27);
    revised[150].close -= 2.0;
    revised[150].low -= 2.0;
    library.write("CCC.YF", BarInterval::DAILY, &revised).expect("written");
    assert_ne!(live_version(&service, &summary).as_deref(), Some(summary.dataset_version.as_str()));

    // A member that is gone takes the panel's data with it.
    std::fs::remove_file(service.data_dir.join("CCC.YF.csv")).expect("removed");
    assert_eq!(live_version(&service, &summary), None);
}

#[test]
fn a_finding_from_before_spans_is_checked_the_way_it_was_made() {
    let (_dir, service, library) = project();
    library.write("AAA.YF", BarInterval::DAILY, &sawtooth(400, 0)).expect("written");
    let study = run_study(&service, "AAA.YF", None).expect("the study runs");

    // As a build before this recorded it: the whole-series hash, untagged.
    let mut summary = summary_of(&service, &study.id);
    summary.dataset_version = library.fingerprint("AAA.YF", BarInterval::DAILY).expect("reads").expect("held");
    assert_eq!(version::span_of(&summary.dataset_version), None);
    assert_eq!(live_version(&service, &summary).as_deref(), Some(summary.dataset_version.as_str()));

    // It goes stale when its series grows, as it always did. Nothing is
    // migrated, and the two kinds of version are never compared.
    library.write("AAA.YF", BarInterval::DAILY, &sawtooth(401, 0)).expect("written");
    let now = live_version(&service, &summary).expect("still there");
    assert_ne!(now, summary.dataset_version);
    assert_eq!(version::span_of(&now), None, "checked with the old hash, not a new one");
}
