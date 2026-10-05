use super::*;
use crate::{Bar, BarInterval, Dividend};

/// A source that answers from a fixture, so `ingest` can be tested without
/// a network.
struct Canned {
    id: &'static str,
    venue: &'static str,
    basis: Basis,
    bars: Vec<Bar>,
    dividends: Option<Vec<Dividend>>,
}

#[async_trait::async_trait]
impl Source for Canned {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        "Canned"
    }
    fn venue(&self) -> &'static str {
        self.venue
    }
    fn basis(&self) -> Basis {
        self.basis
    }
    async fn bars(
        &self,
        _symbol: &str,
        _interval: BarInterval,
        _from: chrono::NaiveDate,
        _to: chrono::NaiveDate,
    ) -> Result<Fetched, SourceError> {
        Ok(Fetched {
            bars: self.bars.clone(),
            interpolated: 2,
        })
    }
    async fn dividends(
        &self,
        _symbol: &str,
        _from: chrono::NaiveDate,
        _to: chrono::NaiveDate,
    ) -> Result<Vec<Dividend>, SourceError> {
        self.dividends.clone().ok_or(SourceError::Unoffered {
            vendor: self.id,
            what: "dividends",
        })
    }
}

fn series(closes: &[f64]) -> Vec<Bar> {
    closes
        .iter()
        .enumerate()
        .map(|(index, close)| Bar {
            at: chrono::NaiveDate::from_ymd_opt(2024, 1, u32::try_from(index).unwrap() + 1)
                .unwrap()
                .and_time(chrono::NaiveTime::MIN),
            open: *close,
            high: *close,
            low: *close,
            close: *close,
            volume: 1_000.0,
        })
        .collect()
}

/// What both shipped sources declare, so a fixture is comparable by
/// default and only says otherwise when a test means it to.
fn consolidated_split() -> Basis {
    Basis {
        feed: Feed::Consolidated,
        adjustment: Adjustment::Split,
    }
}

fn canned(id: &'static str, venue: &'static str, closes: &[f64]) -> Canned {
    Canned {
        id,
        venue,
        basis: consolidated_split(),
        bars: series(closes),
        dividends: None,
    }
}

fn window() -> (chrono::NaiveDate, chrono::NaiveDate) {
    (
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
    )
}

#[tokio::test]
async fn ingest_files_under_the_sources_own_venue() {
    // The property that keeps two vendors' copies of one ticker apart. If
    // both landed on one name, a study would silently run on whichever was
    // fetched last.
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let report = ingest(
        root.path(),
        &canned("acme", "AC", &[1.0, 2.0, 3.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();

    assert_eq!(report.instrument, "MSFT.AC");
    assert_eq!(report.source, "acme");
    assert_eq!(report.bars, 3);
    assert_eq!(report.interpolated, 2, "carried through, not swallowed");
    assert!(report.path.exists());
}

#[tokio::test]
async fn an_id_that_already_carries_a_venue_is_refiled_not_double_suffixed() {
    // Re-fetching `MSFT.YF` from another source must land on that source's
    // own venue, not produce `MSFT.YF.AC`.
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let report = ingest(
        root.path(),
        &canned("acme", "AC", &[1.0, 2.0]),
        "MSFT.YF",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();
    assert_eq!(report.instrument, "MSFT.AC");
}

#[tokio::test]
async fn a_first_fetch_has_nothing_to_compare_against() {
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let report = ingest(
        root.path(),
        &canned("acme", "AC", &[1.0, 2.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();
    assert!(report.revision.is_none());
}

#[tokio::test]
async fn a_refetch_compares_against_what_was_held_not_against_itself() {
    // The ordering bug this exists to prevent: reading after the write
    // compares the new series against a copy of itself and always agrees.
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    ingest(
        root.path(),
        &canned("acme", "AC", &[10.0, 20.0, 30.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();

    let report = ingest(
        root.path(),
        &canned("acme", "AC", &[10.0, 20.0, 44.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();

    assert!(
        matches!(
            report.revision,
            Some(crate::agreement::Agreement::Diverged { .. })
        ),
        "a rewritten history must read as a revision, got {:?}",
        report.revision
    );
}

/// Every fetch leaves a line, whoever asked for it (ADR-0036). The report
/// goes back to a caller who may drop it; the log is what is left.
#[tokio::test]
async fn every_fetch_leaves_a_record_of_what_it_changed() {
    use crate::fetches::Change;
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    for closes in [
        [10.0, 20.0, 30.0], // nothing held: a first fetch
        [10.0, 20.0, 30.0], // the same again
        [5.0, 10.0, 15.0],  // every price halved: a split re-adjustment
        [5.0, 10.0, 22.0],  // one price changed: the vendor revised it
    ] {
        ingest(root.path(), &canned("acme", "AC", &closes), "MSFT", BarInterval::DAILY, from, to).await.unwrap();
    }

    let log = crate::fetches::read(root.path());
    assert_eq!(log.len(), 4);
    assert!(log.iter().all(|fetch| fetch.source == "acme" && fetch.instrument == "MSFT.AC" && fetch.interval == "1day"));
    assert!(log.iter().all(|fetch| (fetch.asked_from, fetch.asked_to, fetch.bars) == (from, to, 3)));

    assert_eq!(log[0].change, Change::First);
    assert_eq!(log[0].before, None, "nothing was held");
    assert!(matches!(log[1].change, Change::Aligned { compared: 3 }), "{:?}", log[1].change);
    assert_eq!(log[1].before, log[1].after, "the same bars are the same series");
    assert!(matches!(log[2].change, Change::Rescaled { compared: 3, .. }), "{:?}", log[2].change);
    assert!(matches!(log[3].change, Change::Diverged { disagreeing: 1, .. }), "{:?}", log[3].change);

    // Each fetch's `before` is the one before it's `after`: the log is a
    // chain of what the series was.
    for pair in log.windows(2) {
        assert_eq!(pair[1].before, pair[0].after);
    }
    assert_ne!(log[2].after, log[1].after, "a rescaled series is a different series");
}

#[tokio::test]
async fn an_empty_series_is_an_error_rather_than_an_empty_file() {
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let err = ingest(
        root.path(),
        &canned("acme", "AC", &[]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SourceError::Empty { .. }));
}

#[tokio::test]
async fn a_source_with_no_dividends_reports_absence_not_zero() {
    // `None` means the vendor does not serve them; `Some(0)` means it
    // looked and the instrument paid none. Collapsing the two would report
    // "no dividends" for an instrument that pays them.
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let report = ingest(
        root.path(),
        &canned("acme", "AC", &[1.0, 2.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();
    assert_eq!(report.dividends, None);
}

#[tokio::test]
async fn dividends_are_written_beside_the_bars_not_into_them() {
    // A column on the bar file would change the content hash of every
    // series in the library and stale every stored finding at once — and
    // a sibling file in the root would be listed as an instrument, which
    // is why `arvo_data` puts them in their own directory.
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let source = Canned {
        id: "acme",
        venue: "AC",
        basis: consolidated_split(),
        bars: series(&[1.0, 2.0]),
        dividends: Some(vec![
            Dividend {
                ex_date: chrono::NaiveDate::from_ymd_opt(2024, 2, 9).unwrap(),
                amount: 0.24,
            },
            Dividend {
                ex_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
                amount: 0.25,
            },
        ]),
    };

    let report = ingest(root.path(), &source, "MSFT", BarInterval::DAILY, from, to)
        .await
        .unwrap();

    assert_eq!(report.dividends, Some(2));
    let text =
        std::fs::read_to_string(root.path().join("dividends").join("MSFT.AC.csv")).unwrap();
    assert_eq!(text, "ex_date,amount\n2024-02-09,0.24\n2024-05-10,0.25\n");

    let bars = std::fs::read_to_string(&report.path).unwrap();
    assert!(
        !bars.contains("dividend"),
        "the bar file must be untouched by this"
    );
    assert_eq!(
        crate::CsvBars::new(root.path()).instruments().unwrap(),
        vec!["MSFT.AC"],
        "the distribution file must not read as an instrument"
    );
}

#[tokio::test]
async fn comparing_two_sources_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (from, to) = window();
    let outcome = compare(
        &canned("acme", "AC", &[1.0, 2.0, 3.0]),
        &canned("beta", "BT", &[1.0, 2.0, 3.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();

    assert!(matches!(
        outcome.agreement,
        crate::agreement::Agreement::Aligned { compared: 3 }
    ));
    assert_eq!(outcome.first, "acme");
    assert_eq!(outcome.second, "beta");
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        0,
        "a comparison asks a question; it does not change the library"
    );
}

#[tokio::test]
async fn two_vendors_that_disagree_are_reported_as_disagreeing() {
    // The whole point of a second source: an internal check cannot catch a
    // close that is merely wrong.
    let (from, to) = window();
    let outcome = compare(
        &canned("acme", "AC", &[10.0, 20.0, 30.0]),
        &canned("beta", "BT", &[10.0, 20.0, 44.0]),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome.agreement,
        crate::agreement::Agreement::Diverged { .. }
    ));
}

fn on(feed: Feed, adjustment: Adjustment) -> Basis {
    Basis { feed, adjustment }
}

/// A source declaring whatever basis a test needs.
fn declaring(id: &'static str, venue: &'static str, basis: Basis) -> Canned {
    Canned {
        id,
        venue,
        basis,
        bars: series(&[10.0, 20.0, 30.0]),
        dividends: None,
    }
}

#[tokio::test]
async fn two_sources_on_the_same_basis_report_no_mismatch() {
    let (from, to) = window();
    let outcome = compare(
        &declaring("a", "A", on(Feed::Consolidated, Adjustment::Split)),
        &declaring("b", "B", on(Feed::Consolidated, Adjustment::Split)),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();
    assert_eq!(outcome.basis_mismatch, None);
}

#[tokio::test]
async fn a_thin_feed_is_named_even_though_the_prices_agree() {
    // The finding this whole type exists for. The prices are identical, so
    // `agreement` says Aligned and means it — and one side is a fraction of
    // the tape, which nothing in the bars can reveal.
    let (from, to) = window();
    let outcome = compare(
        &declaring("broker", "BR", on(Feed::Consolidated, Adjustment::Split)),
        &declaring("thin", "TH", on(Feed::SingleVenue("IEX"), Adjustment::Split)),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();

    assert!(
        matches!(
            outcome.agreement,
            crate::agreement::Agreement::Aligned { .. }
        ),
        "the prices genuinely agree, which is exactly the trap"
    );
    let why = outcome
        .basis_mismatch
        .expect("one side is a single venue and that has to be said");
    assert!(why.contains("IEX"), "{why}");
    assert!(
        why.contains("VWAP"),
        "it should name what breaks, not just that something does: {why}"
    );
}

#[tokio::test]
async fn a_different_adjustment_is_named_as_an_adjustment() {
    // Two bases differ by a compounding factor, which `agreement` correctly
    // calls a rescaling — neither side wrong. Undeclared, that fires on
    // every instrument forever, and a check that always fires is a check
    // nobody reads.
    let (from, to) = window();
    let outcome = compare(
        &declaring("split", "SP", on(Feed::Consolidated, Adjustment::Split)),
        &declaring("total", "TR", on(Feed::Consolidated, Adjustment::TotalReturn)),
        "MSFT",
        BarInterval::DAILY,
        from,
        to,
    )
    .await
    .unwrap();

    assert!(outcome.adjustments_differ);
    let why = outcome.basis_mismatch.expect("different bases");
    assert!(why.contains("split-adjusted"), "{why}");
    assert!(why.contains("total-return adjusted"), "{why}");
}

#[test]
fn the_adjustment_is_reported_ahead_of_the_feed() {
    // Both wrong is possible. The adjustment is the one that makes every
    // bar differ, so it is the one to fix first — reporting the feed
    // instead would send a reader after the smaller problem.
    let split_thin = on(Feed::SingleVenue("IEX"), Adjustment::Split);
    let total_wide = on(Feed::Consolidated, Adjustment::TotalReturn);
    let why = split_thin.mismatch_with(total_wide).expect("both differ");
    assert!(why.contains("total-return adjusted"), "{why}");
}

#[test]
fn the_same_single_venue_on_both_sides_is_comparable() {
    // Two sources reading the same thin feed agree about volume as well as
    // price. The comparison is narrow, and it is not mismatched.
    let iex = on(Feed::SingleVenue("IEX"), Adjustment::Split);
    assert!(iex.comparable_with(iex));
    assert_eq!(iex.mismatch_with(iex), None);
}

#[test]
fn only_a_dead_session_asks_for_a_sign_in() {
    // The three failures that all mean "sign in again" are collapsed at
    // the boundary; a refresh that could not reach the network is not one
    // of them and must not sign anyone out.
    assert!(SourceError::NoSession { vendor: "rh" }.needs_sign_in());
    assert!(!SourceError::Unsupported("3-minute bars".into()).needs_sign_in());
    assert!(!SourceError::Transport {
        vendor: "rh",
        detail: "connection reset".into(),
    }
    .needs_sign_in());
}
