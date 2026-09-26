//! Tests for [`super`].
//!
//! Split out of `dividend.rs` on 2026-09-26 — it was 331 lines of
//! tests against 269 of code, which is the shape
//! `advice/` and `replay/` already moved out for.

use super::*;
use crate::ExitReason;

fn day(month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2024, month, day).expect("valid")
}

fn at(month: u32, d: u32) -> NaiveDateTime {
    day(month, d).and_time(NaiveTime::MIN)
}

fn window() -> DateRange {
    DateRange::new(day(1, 1), day(12, 31)).expect("valid")
}

fn trade(
    instrument: &str,
    opened: NaiveDateTime,
    closed: Option<NaiveDateTime>,
    quantity: f64,
) -> Trade {
    Trade {
        instrument: instrument.to_owned(),
        opened,
        closed,
        direction: Direction::Long,
        quantity,
        entry: 100.0,
        exit: closed.map(|_| 110.0),
        pnl: 0.0,
        commission: 0.0,
        exit_reason: ExitReason::Signal,
        journal: None,
    }
}

fn paid(instrument: &str, events: &[(u32, u32, f64)]) -> std::collections::HashMap<String, Vec<arvo_data::Dividend>> {
    std::collections::HashMap::from([(
        instrument.to_owned(),
        events
            .iter()
            .map(|(month, d, amount)| arvo_data::Dividend {
                ex_date: day(*month, *d),
                amount: *amount,
            })
            .collect(),
    )])
}

/// The instruments a single-instrument run held.
fn held() -> Vec<String> {
    vec!["MSFT.RH".to_owned()]
}

/// Buy-and-hold: in from the first bar, never out.
fn benchmark() -> Vec<Trade> {
    vec![trade("MSFT.RH", at(1, 2), None, 100.0)]
}

#[test]
fn a_rule_that_sat_out_an_ex_date_forgoes_what_the_benchmark_did_not() {
    // The whole bias, in one case. Both sides forgo the dividend because
    // nothing credits it — but only one of them was out of the market for
    // it, and that difference is what flatters the excess return.
    let strategy = vec![trade("MSFT.RH", at(6, 1), Some(at(7, 1)), 100.0)];
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &strategy,
        &benchmark(),
        &paid("MSFT.RH", &[(2, 9, 0.75), (5, 15, 0.75)]),
    );

    assert_eq!(gap.events, 2);
    assert!((gap.strategy_income - 0.0).abs() < 1e-9, "out for both");
    assert!((gap.benchmark_income - 150.0).abs() < 1e-9, "in for both");
    assert!((gap.overstatement - 0.0015).abs() < 1e-9);
}

#[test]
fn a_rule_in_the_market_throughout_is_biased_identically_and_the_gap_is_zero() {
    // The comparison is fair when both sides forgo the same payments. A
    // measurement that reported a bias here would be flagging a problem
    // that does not exist.
    let strategy = vec![trade("MSFT.RH", at(1, 2), None, 100.0)];
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &strategy,
        &benchmark(),
        &paid("MSFT.RH", &[(2, 9, 0.75), (5, 15, 0.75)]),
    );

    assert_eq!(gap.events, 2);
    assert!((gap.overstatement).abs() < 1e-12);
    assert!(!gap.worth_saying());
}

#[test]
fn an_instrument_that_pays_nothing_has_no_bias_to_correct() {
    // Distinct from having no series: this one was looked up and paid
    // nothing, so the excess return needs no correction at all.
    let strategy = vec![trade("MSFT.RH", at(6, 1), Some(at(7, 1)), 100.0)];
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &strategy,
        &benchmark(),
        &paid("MSFT.RH", &[]),
    );
    assert_eq!(gap.events, 0);
    assert!((gap.overstatement).abs() < 1e-12);
}

#[test]
fn selling_on_the_ex_date_still_collects_and_buying_on_it_does_not() {
    // Entitlement is settled at the close before the ex-date. Getting this
    // backwards credits the wrong side of every payment landing on a
    // trading day, which is exactly the wrong error on a rule that trades
    // around ex-dates.
    let seller = vec![trade("MSFT.RH", at(1, 2), Some(at(2, 9)), 100.0)];
    let buyer = vec![trade("MSFT.RH", at(2, 9), Some(at(3, 1)), 100.0)];
    let dividends = paid("MSFT.RH", &[(2, 9, 0.75)]);

    let sold = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &seller,
        &benchmark(),
        &dividends,
    );
    assert!(
        (sold.strategy_income - 75.0).abs() < 1e-9,
        "a seller on the ex-date was the holder of record"
    );

    let bought = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &buyer,
        &benchmark(),
        &dividends,
    );
    assert!(
        bought.strategy_income.abs() < 1e-9,
        "a buyer on the ex-date bought it without the dividend"
    );
}

#[test]
fn only_distributions_inside_the_window_are_counted() {
    let strategy = vec![trade("MSFT.RH", at(1, 2), None, 100.0)];
    let narrow = DateRange::new(day(3, 1), day(8, 31)).expect("valid");
    let gap = measure_dividend_gap(
        narrow,
        100_000.0,
        Adjustment::Split,
        &held(),
        &strategy,
        &benchmark(),
        &paid("MSFT.RH", &[(2, 9, 0.75), (5, 15, 0.75), (11, 9, 0.75)]),
    );
    assert_eq!(gap.events, 1, "only the May payment is in the window");
}

#[test]
fn a_book_sums_the_gap_across_its_members() {
    let strategy = vec![
        trade("MSFT.RH", at(1, 2), None, 100.0),
        trade("KO.RH", at(6, 1), Some(at(7, 1)), 200.0),
    ];
    let bench = vec![
        trade("MSFT.RH", at(1, 2), None, 100.0),
        trade("KO.RH", at(1, 2), None, 200.0),
    ];
    let mut dividends = paid("MSFT.RH", &[(2, 9, 0.75)]);
    dividends.extend(paid("KO.RH", &[(3, 14, 0.48)]));

    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &["MSFT.RH".to_owned(), "KO.RH".to_owned()],
        &strategy,
        &bench,
        &dividends,
    );
    assert_eq!(gap.events, 2);
    // MSFT: both held, no gap. KO: benchmark held 200 shares at $0.48.
    assert!((gap.benchmark_income - gap.strategy_income - 96.0).abs() < 1e-9);
}

#[test]
fn a_book_with_only_some_series_reports_a_floor_not_an_answer() {
    // One member fetched from a source that serves distributions and one
    // from a source that does not. The sum is real but partial, and
    // presenting it as the gap would be a precise understated number —
    // worse than none, because it invites belief.
    let strategy = vec![trade("MSFT.RH", at(6, 1), Some(at(7, 1)), 100.0)];
    let bench = vec![
        trade("MSFT.RH", at(1, 2), None, 100.0),
        trade("KO.RH", at(1, 2), None, 200.0),
    ];
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &["MSFT.RH".to_owned(), "KO.RH".to_owned()],
        &strategy,
        &bench,
        &paid("MSFT.RH", &[(2, 9, 0.75)]),
    );

    assert_eq!((gap.covered, gap.instruments), (1, 2));
    assert!(!gap.complete(), "KO's distributions are unknown, not zero");
}

#[test]
fn a_trade_in_another_instrument_does_not_collect_this_ones_dividend() {
    // A book's ledger holds every member's trades, and crediting one
    // member's payment to another's position would be a cash credit that
    // never happened.
    let strategy = vec![trade("AAPL.RH", at(1, 2), None, 100.0)];
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &strategy,
        &benchmark(),
        &paid("MSFT.RH", &[(2, 9, 0.75)]),
    );
    assert!(gap.strategy_income.abs() < 1e-9);
}

#[test]
fn a_ledger_from_before_instruments_were_named_still_collects() {
    // Those runs were all single-instrument. Refusing to match would report
    // zero income for every stored finding at once, which reads as "this
    // rule collected no dividends" and is not true.
    let strategy = vec![trade("", at(1, 2), None, 100.0)];
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &strategy,
        &benchmark(),
        &paid("MSFT.RH", &[(2, 9, 0.75)]),
    );
    assert!((gap.strategy_income - 75.0).abs() < 1e-9);
}

#[test]
fn a_short_pays_the_dividend_rather_than_receiving_it() {
    let mut short = trade("MSFT.RH", at(1, 2), None, 100.0);
    short.direction = Direction::Short;
    let gap = measure_dividend_gap(
        window(),
        100_000.0,
        Adjustment::Split,
        &held(),
        &[short],
        &benchmark(),
        &paid("MSFT.RH", &[(2, 9, 0.75)]),
    );
    assert!((gap.strategy_income + 75.0).abs() < 1e-9);
}

#[test]
fn the_correction_comes_off_the_excess_return_in_the_same_units() {
    let gap = DividendGap {
        events: 4,
        strategy_income: 0.0,
        benchmark_income: 2_000.0,
        overstatement: 0.02,
        adjustment: Adjustment::Split,
        covered: 1,
        instruments: 1,
    };
    let corrected = gap.corrected_excess(0.05).expect("split-adjusted corrects");
    assert!((corrected - 0.03).abs() < 1e-12);
    assert!(gap.is_a_correction());
    assert!(gap.worth_saying());
}

#[test]
fn on_a_total_return_series_the_gap_refuses_to_correct_rather_than_double_counting() {
    // The dividend is already in the returns there, so the excess return
    // has it. Subtracting the gap again would report a margin smaller than
    // the account earned — the reason ADR-0013 made this basis-aware.
    let gap = DividendGap {
        events: 4,
        strategy_income: 0.0,
        benchmark_income: 2_000.0,
        overstatement: 0.02,
        adjustment: Adjustment::TotalReturn,
        covered: 1,
        instruments: 1,
    };
    assert_eq!(gap.corrected_excess(0.05), None);
    assert!(!gap.is_a_correction());
    // Still measured, still worth reporting — as composition rather than
    // as a correction.
    assert!(gap.worth_saying());
}

#[test]
fn a_gap_smaller_than_the_rounding_in_what_it_corrects_is_not_worth_a_line() {
    let gap = DividendGap {
        events: 1,
        strategy_income: 10.0,
        benchmark_income: 20.0,
        overstatement: 0.0001,
        adjustment: Adjustment::Split,
        covered: 1,
        instruments: 1,
    };
    assert!(!gap.worth_saying());
}
