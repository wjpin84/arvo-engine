/// What a fetcher writes must be what the library reads. This is the seam
/// where a format mismatch costs nothing at write time and shows up later
/// as an instrument that exists on disk and holds no bars.
mod round_trip {
    use super::super::*;
    use chrono::NaiveTime;

    fn bars(interval: BarInterval) -> Vec<Bar> {
        (0..4)
            .map(|index| {
                let at = NaiveDate::from_ymd_opt(2026, 9, 3)
                    .expect("valid")
                    .and_hms_opt(13, 30, 0)
                    .expect("valid")
                    + interval.duration() * index;
                let close = 500.0 + f64::from(index) * 0.25;
                Bar {
                    at,
                    open: close - 0.1,
                    high: close + 0.5,
                    low: close - 0.5,
                    close,
                    volume: 1_000.0 + f64::from(index),
                }
            })
            .collect()
    }

    #[test]
    fn intraday_bars_survive_a_write_and_a_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = CsvBars::new(dir.path());
        let interval = BarInterval::new(5, IntervalUnit::Minute);
        let written = bars(interval);

        library
            .write("MSFT.NASDAQ", interval, &written)
            .expect("writes");
        let read = library
            .bars(
                "MSFT.NASDAQ",
                interval,
                NaiveDate::MIN,
                NaiveDate::MAX,
            )
            .expect("reads");

        assert_eq!(read.len(), written.len());
        for (read, written) in read.iter().zip(&written) {
            assert_eq!(read.at, written.at, "the timestamp is the whole instant");
            assert!((read.close - written.close).abs() < 1e-9);
            assert!((read.volume - written.volume).abs() < 1e-9);
        }
    }

    #[test]
    fn a_daily_file_keeps_the_bare_date_the_free_exports_use() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = CsvBars::new(dir.path());
        let daily: Vec<Bar> = bars(BarInterval::DAILY)
            .into_iter()
            .map(|bar| Bar {
                at: bar.at.date().and_time(NaiveTime::MIN),
                ..bar
            })
            .collect();

        let path = library
            .write("MSFT.NASDAQ", BarInterval::DAILY, &daily)
            .expect("writes");
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            text.contains("\n2026-09-03,"),
            "a daily row is a bare date: {text}"
        );

        let read = library
            .daily_bars("MSFT.NASDAQ", NaiveDate::MIN, NaiveDate::MAX)
            .expect("reads");
        assert_eq!(read, daily);
    }

    #[test]
    fn an_intraday_write_does_not_appear_as_a_new_instrument() {
        // Intraday files live in a subdirectory precisely so a five-minute
        // pull does not make `MSFT.NASDAQ.5minute` look like something you
        // could trade.
        let dir = tempfile::tempdir().expect("tempdir");
        let library = CsvBars::new(dir.path());
        let interval = BarInterval::new(5, IntervalUnit::Minute);

        library
            .write("MSFT.NASDAQ", interval, &bars(interval))
            .expect("writes");
        assert_eq!(
            library.instruments().expect("lists"),
            Vec::<String>::new(),
            "only daily files name instruments"
        );
    }

    #[test]
    fn writing_replaces_rather_than_appends() {
        // A file that is partly one fetch and partly another is not a
        // dataset anyone can name: its content hash would describe a state
        // no single request ever returned.
        let dir = tempfile::tempdir().expect("tempdir");
        let library = CsvBars::new(dir.path());
        let interval = BarInterval::new(5, IntervalUnit::Minute);

        library
            .write("MSFT.NASDAQ", interval, &bars(interval))
            .expect("writes");
        library
            .write("MSFT.NASDAQ", interval, &bars(interval)[..2])
            .expect("writes again");

        let read = library
            .bars("MSFT.NASDAQ", interval, NaiveDate::MIN, NaiveDate::MAX)
            .expect("reads");
        assert_eq!(read.len(), 2);
    }
}

use super::*;
use std::path::Path;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("test date is valid")
}

fn bar(day: u32, close: f64) -> Bar {
    Bar {
        at: date(2024, 1, day).and_time(chrono::NaiveTime::MIN),
        open: close,
        high: close,
        low: close,
        close,
        volume: 1_000.0,
    }
}

#[test]
fn in_memory_bars_come_back_sorted_and_windowed() {
    let source = InMemoryBars::new()
        .with_instrument("AAPL.NASDAQ", vec![bar(3, 3.0), bar(1, 1.0), bar(2, 2.0)]);

    let bars = source
        .daily_bars("AAPL.NASDAQ", date(2024, 1, 1), date(2024, 1, 2))
        .expect("instrument is known");

    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    assert_eq!(closes, vec![1.0, 2.0], "window is inclusive at both ends");
}

#[test]
fn an_unknown_instrument_is_distinct_from_an_empty_window() {
    let source = InMemoryBars::new().with_instrument("AAPL.NASDAQ", vec![bar(1, 1.0)]);

    let empty = source
        .daily_bars("AAPL.NASDAQ", date(2025, 1, 1), date(2025, 1, 2))
        .expect("known instrument, no bars in window");
    assert!(empty.is_empty());

    let err = source
        .daily_bars("MSFT.NASDAQ", date(2024, 1, 1), date(2024, 1, 2))
        .expect_err("never heard of it");
    assert!(matches!(err, DataError::UnknownInstrument(_)), "{err}");
}

fn write_csv(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("fixture should write");
}

#[test]
fn csv_bars_parse_and_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_csv(
        dir.path(),
        "AAPL.NASDAQ.csv",
        "date,open,high,low,close,volume\n\
             2024-01-02,185.0,186.0,184.0,185.5,1000\n\
             2024-01-03,185.5,187.0,185.0,186.5,1100\n\
             2024-01-04,186.5,188.0,186.0,187.5,1200\n",
    );

    let bars = CsvBars::new(dir.path())
        .daily_bars("AAPL.NASDAQ", date(2024, 1, 3), date(2024, 1, 4))
        .expect("fixture parses");

    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].at.date(), date(2024, 1, 3));
    assert!((bars[1].close - 187.5).abs() < f64::EPSILON);
}

#[test]
fn a_corrupt_row_is_an_error_naming_the_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_csv(
        dir.path(),
        "BAD.X.csv",
        "date,open,high,low,close,volume\n\
             2024-01-02,185.0,186.0,184.0,185.5,1000\n\
             2024-01-03,185.5,180.0,185.0,186.5,1100\n",
    );

    let err = CsvBars::new(dir.path())
        .daily_bars("BAD.X", date(2024, 1, 1), date(2024, 12, 31))
        .expect_err("high below low is corrupt");

    match err {
        DataError::Malformed {
            line, ref reason, ..
        } => {
            assert_eq!(line, 3, "should name the offending line");
            assert!(reason.contains("below low"), "{reason}");
        }
        other => panic!("expected a malformed-row error, got {other}"),
    }
}

#[test]
fn coverage_reports_the_real_extent_of_the_data() {
    let source = InMemoryBars::new()
        .with_instrument("AAPL.NASDAQ", vec![bar(3, 3.0), bar(1, 1.0), bar(2, 2.0)]);

    let (first, last) = source
        .coverage("AAPL.NASDAQ", BarInterval::DAILY)
        .expect("known instrument")
        .expect("it holds bars");
    assert_eq!(first, date(2024, 1, 1));
    assert_eq!(last, date(2024, 1, 3));

    let empty = InMemoryBars::new().with_instrument("EMPTY.X", vec![]);
    assert_eq!(
        empty
            .coverage("EMPTY.X", BarInterval::DAILY)
            .expect("known"),
        None
    );
}

#[test]
fn a_fingerprint_changes_when_a_price_changes_and_not_otherwise() {
    let original = InMemoryBars::new()
        .with_instrument("AAPL.NASDAQ", vec![bar(1, 1.0), bar(2, 2.0), bar(3, 3.0)]);
    let reordered = InMemoryBars::new()
        .with_instrument("AAPL.NASDAQ", vec![bar(3, 3.0), bar(1, 1.0), bar(2, 2.0)]);
    let edited = InMemoryBars::new()
        .with_instrument("AAPL.NASDAQ", vec![bar(1, 1.0), bar(2, 2.5), bar(3, 3.0)]);

    let of = |source: &InMemoryBars| {
        source
            .fingerprint("AAPL.NASDAQ", BarInterval::DAILY)
            .expect("known instrument")
            .expect("holds bars")
    };

    assert_eq!(
        of(&original),
        of(&reordered),
        "the same bars in a different input order are the same dataset"
    );
    assert_ne!(
        of(&original),
        of(&edited),
        "one changed price must invalidate every result that used it"
    );
}

#[test]
fn an_empty_instrument_has_no_fingerprint_to_report() {
    let empty = InMemoryBars::new().with_instrument("EMPTY.X", vec![]);
    assert_eq!(
        empty
            .fingerprint("EMPTY.X", BarInterval::DAILY)
            .expect("known"),
        None
    );

    let err = empty
        .fingerprint("NEVER.HEARD", BarInterval::DAILY)
        .expect_err("unknown instrument");
    assert!(matches!(err, DataError::UnknownInstrument(_)), "{err}");
}

#[test]
fn an_absent_data_directory_is_an_empty_library_not_an_error() {
    let missing = CsvBars::new("/no/such/directory/anywhere");
    assert_eq!(
        missing.instruments().expect("absence is not failure"),
        Vec::<String>::new()
    );
}

#[test]
fn instruments_come_from_the_csv_file_names_sorted() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_csv(
        dir.path(),
        "MSFT.NASDAQ.csv",
        "date,open,high,low,close,volume
",
    );
    write_csv(
        dir.path(),
        "AAPL.NASDAQ.csv",
        "date,open,high,low,close,volume
",
    );
    write_csv(dir.path(), "notes.txt", "ignore me");

    assert_eq!(
        CsvBars::new(dir.path()).instruments().expect("readable"),
        vec!["AAPL.NASDAQ".to_owned(), "MSFT.NASDAQ".to_owned()]
    );
}

#[test]
fn a_traversing_instrument_name_never_reaches_the_filesystem() {
    let source = CsvBars::new("/does/not/matter");
    for attempt in ["../../secrets", "..", "a/b", ""] {
        let err = source
            .daily_bars(attempt, date(2024, 1, 1), date(2024, 1, 2))
            .expect_err("should refuse to build a path");
        assert!(
            matches!(err, DataError::UnsafeInstrument(_)),
            "{attempt:?}: {err}"
        );
    }
}
