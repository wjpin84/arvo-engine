use super::*;
use crate::{InMemoryBars, IntervalUnit};

fn library() -> (tempfile::TempDir, CsvBars) {
    let dir = tempfile::tempdir().expect("tempdir");
    let bars = CsvBars::new(dir.path());
    (dir, bars)
}

fn window() -> (NaiveDate, NaiveDate) {
    (
        NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        NaiveDate::from_ymd_opt(2024, 12, 31).expect("valid"),
    )
}

fn paid(month: u32, amount: f64) -> Dividend {
    Dividend {
        ex_date: NaiveDate::from_ymd_opt(2024, month, 9).expect("valid"),
        amount,
    }
}

#[test]
fn what_is_written_is_what_is_read() {
    let (_dir, library) = library();
    let (from, to) = window();
    library
        .write_dividends("MSFT.RH", &[paid(2, 0.75), paid(5, 0.83)])
        .expect("write");

    let read = library.dividends("MSFT.RH", from, to).expect("read");
    assert_eq!(read, Some(vec![paid(2, 0.75), paid(5, 0.83)]));
}

#[test]
fn no_series_at_all_is_none_and_an_empty_one_is_some_nothing() {
    // The distinction the whole feature rests on. `None` means no source
    // ever supplied distributions; `Some(vec![])` means one did and the
    // instrument paid nothing. Collapsing them reports "no dividends" for
    // an instrument that pays them.
    let (_dir, library) = library();
    let (from, to) = window();
    assert_eq!(library.dividends("MSFT.RH", from, to).expect("read"), None);

    library.write_dividends("MSFT.RH", &[]).expect("write");
    assert_eq!(
        library.dividends("MSFT.RH", from, to).expect("read"),
        Some(Vec::new())
    );
}

#[test]
fn a_dividend_file_is_not_listed_as_an_instrument() {
    // It used to be written as `MSFT.RH.dividends.csv` beside the bars,
    // and `instruments()` lists the root's file stems — so the library
    // grew a phantom instrument called `MSFT.RH.dividends` that nothing
    // put there on purpose. Hence the subdirectory.
    let (_dir, library) = library();
    library
        .write("MSFT.RH", BarInterval::DAILY, &[Bar {
            at: NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid").and_time(NaiveTime::MIN),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }])
        .expect("write bars");
    library.write_dividends("MSFT.RH", &[paid(2, 0.75)]).expect("write");

    assert_eq!(library.instruments().expect("list"), vec!["MSFT.RH"]);
}

#[test]
fn a_contract_reads_back_like_a_stock_and_is_not_listed_as_one() {
    let (_dir, library) = library();
    let bar = Bar {
        at: NaiveDate::from_ymd_opt(2024, 3, 1).expect("valid").and_time(NaiveTime::MIN),
        open: 1.75,
        high: 2.27,
        low: 1.7,
        close: 1.83,
        volume: 3812.0,
    };
    for name in ["SPY.AIEX", "SPY240315C00510000.AOPT"] {
        library.write(name, BarInterval::DAILY, &[bar]).expect("write");
    }
    let (from, to) = window();
    assert_eq!(
        library.bars("SPY240315C00510000.AOPT", BarInterval::DAILY, from, to).expect("read"),
        vec![bar]
    );
    assert!(library.fingerprint("SPY240315C00510000.AOPT", BarInterval::DAILY).expect("hash").is_some());
    assert_eq!(library.instruments().expect("list"), vec!["SPY.AIEX"], "a chain would bury the library");
}

#[test]
fn a_library_lists_the_contracts_it_holds_on_an_underlying() {
    let (_dir, library) = library();
    let bar = Bar {
        at: NaiveDate::from_ymd_opt(2024, 3, 1).expect("valid").and_time(NaiveTime::MIN),
        open: 1.0,
        high: 1.0,
        low: 1.0,
        close: 1.0,
        volume: 1.0,
    };
    library
        .write_contracts(
            BarInterval::DAILY,
            &BTreeMap::from([
                ("SPY240315P00500000.AOPT".to_owned(), vec![bar]),
                ("SPY240322P00500000.AOPT".to_owned(), vec![bar]),
                ("QQQ240315P00400000.AOPT".to_owned(), vec![bar]),
            ]),
        )
        .expect("write");
    let mut listed = library.option_contracts("SPY", BarInterval::DAILY).expect("list");
    listed.sort();
    assert_eq!(listed, ["SPY240315P00500000.AOPT", "SPY240322P00500000.AOPT"]);
    assert!(library.option_contracts("SPY", BarInterval::new(5, IntervalUnit::Minute)).expect("list").is_empty());
    assert!(library.option_contracts("IWM", BarInterval::DAILY).expect("list").is_empty());

    let before = library.option_chain_fingerprint("SPY", BarInterval::DAILY).expect("hash");
    assert!(before.is_some());
    assert_eq!(library.option_chain_fingerprint("IWM", BarInterval::DAILY).expect("hash"), None);
    let revised = Bar { close: 1.5, high: 1.5, ..bar };
    library.write("SPY240322P00500000.AOPT", BarInterval::DAILY, &[revised]).expect("write");
    assert_ne!(
        library.option_chain_fingerprint("SPY", BarInterval::DAILY).expect("hash"),
        before,
        "one contract's revision is a different chain"
    );
}

#[test]
fn contracts_share_their_expirations_file_and_rewriting_one_keeps_the_rest() {
    let (_dir, library) = library();
    let (from, to) = window();
    let bar = |day: u32, close: f64| Bar {
        at: NaiveDate::from_ymd_opt(2024, 3, day).expect("valid").and_time(NaiveTime::MIN),
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
    };
    let call = "SPY240315C00510000.AOPT";
    let put = "SPY240315P00510000.AOPT";
    let later = "SPY240322C00510000.AOPT";
    let paths = library
        .write_contracts(
            BarInterval::DAILY,
            &BTreeMap::from([
                (call.to_owned(), vec![bar(1, 1.0), bar(4, 1.5)]),
                (put.to_owned(), vec![bar(1, 2.0)]),
                (later.to_owned(), vec![bar(1, 3.0)]),
            ]),
        )
        .expect("write");
    assert_eq!(paths.len(), 2, "two expirations, two files");
    assert!(paths[0].ends_with(Path::new("options").join("SPY").join("1day").join("2024-03-15.AOPT.csv")), "{}", paths[0].display());

    library.write(call, BarInterval::DAILY, &[bar(5, 9.0)]).expect("rewrite one");
    assert_eq!(library.bars(call, BarInterval::DAILY, from, to).expect("read"), vec![bar(5, 9.0)], "replaced, not merged");
    assert_eq!(library.bars(put, BarInterval::DAILY, from, to).expect("read"), vec![bar(1, 2.0)], "its neighbour is untouched");
    assert_eq!(library.bars(later, BarInterval::DAILY, from, to).expect("read"), vec![bar(1, 3.0)]);

    assert!(matches!(
        library.bars("SPY240315C00999000.AOPT", BarInterval::DAILY, from, to),
        Err(DataError::UnknownInstrument(_))
    ), "a contract not in its expiration's file is unknown, not empty");
    let march = NaiveDate::from_ymd_opt(2024, 3, 20).expect("valid");
    assert_eq!(library.bars(put, BarInterval::DAILY, march, to).expect("known"), Vec::new(), "known but outside the window is empty");

    assert!(matches!(
        library.write_contracts(BarInterval::DAILY, &BTreeMap::from([("SPY.AIEX".to_owned(), vec![bar(1, 1.0)])])),
        Err(DataError::UnsafeInstrument(_))
    ));
}

#[test]
fn only_distributions_inside_the_window_come_back() {
    let (_dir, library) = library();
    library
        .write_dividends("MSFT.RH", &[paid(2, 0.75), paid(5, 0.83), paid(11, 0.91)])
        .expect("write");

    let read = library
        .dividends(
            "MSFT.RH",
            NaiveDate::from_ymd_opt(2024, 3, 1).expect("valid"),
            NaiveDate::from_ymd_opt(2024, 8, 31).expect("valid"),
        )
        .expect("read");
    assert_eq!(read, Some(vec![paid(5, 0.83)]));
}

#[test]
fn an_unreadable_amount_is_named_rather_than_credited_as_zero() {
    // A distribution silently read as zero is a cash credit that never
    // happens, which is the error this series exists to remove.
    let (dir, library) = library();
    let path = dir.path().join(DIVIDEND_SUBDIR);
    std::fs::create_dir_all(&path).expect("mkdir");
    std::fs::write(path.join("MSFT.RH.csv"), "ex_date,amount
2024-02-09,tuppence
")
        .expect("write");

    let (from, to) = window();
    assert!(matches!(
        library.dividends("MSFT.RH", from, to),
        Err(DataError::Malformed { .. })
    ));
}

#[test]
fn a_name_that_could_escape_the_root_is_refused_for_dividends_too() {
    // Two paths are built from an instrument name now, and a guard applied
    // to only one of them is not a guard.
    let (_dir, library) = library();
    assert!(matches!(
        library.write_dividends("../../etc/passwd", &[]),
        Err(DataError::UnsafeInstrument(_))
    ));
}

#[test]
fn a_provider_that_knows_nothing_of_dividends_says_so() {
    // The trait default. An in-memory fixture has no series, and `None` is
    // the honest answer rather than an empty list.
    let (from, to) = window();
    let fixture = InMemoryBars::new();
    assert_eq!(fixture.dividends("MSFT.RH", from, to).expect("read"), None);
}

/// A pair's id is spelled with a dash, not a slash, so the library can hold it.
///
/// `safe_name` refuses a slash, which is what stops an id walking out of the
/// data directory — so `BTC/USD` could never be a file here, and weakening that
/// guard to admit one would trade a path-traversal defence for a spelling. The
/// slash belongs in the request to the venue, not in the id.
#[test]
fn a_coin_pair_is_a_name_the_library_can_write_and_a_slashed_one_is_not() {
    let (_dir, library) = library();
    let (from, to) = window();
    let priced = |day: u32, close: f64| Bar {
        at: NaiveDate::from_ymd_opt(2024, 3, day).expect("valid").and_time(NaiveTime::MIN),
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
    };
    let bars = vec![priced(1, 2.4567), priced(2, 2.4581)];

    library
        .write("XRP-USD.ALPACA", BarInterval::DAILY, &bars)
        .expect("a dashed pair is a safe name");
    assert_eq!(
        library.bars("XRP-USD.ALPACA", BarInterval::DAILY, from, to).expect("written"),
        bars,
        "and reads back with its sub-cent prices"
    );

    assert!(matches!(
        library.write("XRP/USD.ALPACA", BarInterval::DAILY, &bars),
        Err(DataError::UnsafeInstrument(_))
    ), "a slash stays refused");
}
