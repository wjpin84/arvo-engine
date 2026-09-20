//! What the missing dividend series does to the excess return, measured.
//!
//! # The bias
//!
//! Every source shipped today serves split-adjusted prices, which is right: raw
//! prices make a split look like a crash and a breakout rule would trade it. But
//! split-adjusted is not *total-return* adjusted. Dividends are absent from the
//! price series and nothing credits them as cash, so neither side of the
//! comparison receives them — and the two sides do not forgo the same amount.
//! The benchmark holds through every ex-date in the window; a rule in the market
//! 40% of the time holds through about 40% of them.
//!
//! So the reported excess return is overstated, systematically, in the
//! strategy's favour, on every dividend-paying instrument. It always points the
//! same way, which is what makes it worth a number rather than a caveat.
//!
//! # The same measurement means two different things
//!
//! On a total-return dataset ([`Adjustment::TotalReturn`]) that bias is not
//! there: the distribution is inside the return, so holding through an ex-date
//! captures it and being flat does not. Who held on each ex-date is still worth
//! knowing — it says how much of the margin is distribution rather than timing —
//! but it is a *description* of the excess return rather than a correction to
//! it, and subtracting it would report less than the account earned.
//!
//! So [`DividendGap`] carries the basis it was measured on and
//! [`DividendGap::corrected_excess`] refuses on the one where subtracting is
//! wrong. See [ADR-0013].
//!
//! [ADR-0013]: https://github.com/wjpin84/arvo-desktop/blob/master/https://github.com/wjpin84/arvo-adrs/blob/main/0013-dividends-arrive-as-reinvestment.md
//!
//! # Why this is measured rather than estimated
//!
//! It used to be a sentence: *treat the margin as smaller by roughly the yield
//! times the share of the window this rule sat out*. That is the right shape and
//! it is unusable — nobody knows the instrument's yield off-hand, "roughly" is
//! doing all the work, and a reader who wants the corrected number has to go and
//! find the dividend history themselves.
//!
//! With a distribution series in the library, the exact figure is available: for
//! each payment, was each side holding on the day entitlement was settled, and
//! how many shares. That is [`measure_dividend_gap`]. It returns cash, in account currency,
//! from the two ledgers the run already produced — not a yield, not an estimate,
//! and not a restatement of anything the metrics already contain.
//!
//! # What it deliberately does not do
//!
//! It does not correct the curves. Crediting a cash dividend into a backtest
//! changes position sizing on every subsequent bar and is a change to the engine,
//! not to a report. This measures the gap and says so; removing it is a separate
//! piece of work that this makes checkable.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

use arvo_data::source::Adjustment;

use crate::{DateRange, Direction, Trade};

/// The dividend gap between a strategy and its benchmark, in cash and in
/// return.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DividendGap {
    /// Distributions that went ex inside the window, on instruments either
    /// side held at some point.
    ///
    /// Zero with a series present is a real answer: the instruments pay
    /// nothing, so there is no bias and the excess return needs no correction.
    pub events: usize,
    /// Cash the strategy would have received, had dividends been credited.
    pub strategy_income: f64,
    /// Cash the benchmark would have received.
    ///
    /// Normally the larger of the two: buy-and-hold is in the market for every
    /// ex-date by construction.
    pub benchmark_income: f64,
    /// [`Self::benchmark_income`] minus [`Self::strategy_income`], as a
    /// fraction of starting capital.
    ///
    /// Directly subtractable from the reported excess return, which is in the
    /// same units. Positive is the ordinary case and means the excess return
    /// reads better than it was. Negative is possible and is not an error: a
    /// rule sized larger than the benchmark can hold more shares across an
    /// ex-date than buy-and-hold does, and then the bias runs the other way.
    pub overstatement: f64,
    /// Instruments whose distribution series was available.
    pub covered: usize,
    /// What the prices this was measured against were adjusted for.
    ///
    /// Decides whether the gap is a *correction* or a *description*, which is
    /// the whole reason it travels here — see [`Self::corrected_excess`].
    pub adjustment: Adjustment,
    /// Instruments the run held. When this exceeds [`Self::covered`] the figure
    /// is a floor rather than the answer.
    ///
    /// Recorded rather than assumed equal, because a book can hold a mix: one
    /// member fetched from a source that serves distributions and another from
    /// one that does not. Presenting a partial sum as *the* gap would be a
    /// plausible, precise, understated number — which is worse than no number,
    /// because it invites belief.
    pub instruments: usize,
}

impl DividendGap {
    /// Whether this is large enough to change how a result should be read.
    ///
    /// A tenth of a percent of starting capital. Below that the correction is
    /// smaller than the rounding in every figure it would be applied to, and a
    /// line about it would be noise that teaches a reader to skip the section.
    #[must_use]
    pub fn worth_saying(&self) -> bool {
        self.overstatement.abs() >= 0.001
    }

    /// Whether every instrument the run held had a distribution series.
    ///
    /// When false the measured gap is a floor: the uncovered members
    /// contributed nothing to either side, and their real contribution is
    /// almost certainly a further overstatement rather than an offset.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.covered >= self.instruments
    }

    /// The excess return with the gap taken out of it, where taking it out
    /// means anything.
    ///
    /// # Why this can refuse
    ///
    /// On a **split-adjusted** series the distribution is missing from both
    /// sides, so the gap is money neither side received and subtracting it
    /// corrects the margin. That is the case this was written for.
    ///
    /// On a **total-return** series it is already in the returns: a rule that
    /// held through an ex-date captured the dividend through the price, and the
    /// benchmark captured all of them. The excess return therefore *already*
    /// accounts for the difference, and subtracting the gap again would
    /// double-count — reporting a margin smaller than the account earned.
    ///
    /// So this returns `None` there, and the gap stays worth reporting as a
    /// *description*: how much of the margin is distribution rather than skill.
    /// A figure that is silently wrong under a basis nobody checked is exactly
    /// what [ADR-0011] exists to refuse.
    ///
    /// [ADR-0011]: https://github.com/wjpin84/arvo-desktop/blob/master/https://github.com/wjpin84/arvo-adrs/blob/main/0011-dividend-gap-beside-not-folded-in.md
    #[must_use]
    pub fn corrected_excess(&self, excess_return: f64) -> Option<f64> {
        match self.adjustment {
            Adjustment::Split => Some(excess_return - self.overstatement),
            Adjustment::TotalReturn => None,
        }
    }

    /// Whether the gap is money that went missing, or money already counted.
    ///
    /// The one thing a reader needs to know to tell which of the two numbers in
    /// front of them this is.
    #[must_use]
    pub const fn is_a_correction(&self) -> bool {
        matches!(self.adjustment, Adjustment::Split)
    }
}

/// Measures the gap from the two ledgers and the distributions.
///
/// `dividends` is keyed by instrument, as `SYMBOL.VENUE`. An instrument with no
/// entry contributes nothing — its distributions are unknown, not zero, and the
/// caller decides whether a partial series is worth reporting. See
/// [`crate::evaluation::Evaluation::dividend_gap`] for how that is handled.
///
/// # Shares, and the one assumption here
///
/// [`Trade::quantity`] is the *peak* size held over the round trip. For a rule
/// that enters once and exits once — which is every rule this platform ships —
/// peak size is the size held throughout, and the arithmetic is exact. For a
/// rule that scales in or out, peak size overstates what was held on some days,
/// so the strategy's income is an upper bound and the gap a lower one. The bias
/// therefore reads *smaller* than it is, which is the safe direction for a
/// figure whose whole purpose is to stop a result being believed too readily.
#[must_use]
pub fn measure_dividend_gap(
    window: DateRange,
    starting_cash: f64,
    adjustment: Adjustment,
    instruments: &[String],
    strategy_ledger: &[Trade],
    benchmark_ledger: &[Trade],
    dividends: &std::collections::HashMap<String, Vec<arvo_data::Dividend>>,
) -> DividendGap {
    let mut events = 0;
    let mut strategy_income = 0.0;
    let mut benchmark_income = 0.0;

    for (instrument, paid) in dividends {
        for dividend in paid {
            if dividend.ex_date < window.from || dividend.ex_date > window.to {
                continue;
            }
            events += 1;
            strategy_income += dividend.amount * shares_on(strategy_ledger, instrument, dividend.ex_date);
            benchmark_income +=
                dividend.amount * shares_on(benchmark_ledger, instrument, dividend.ex_date);
        }
    }

    // Guarded rather than assumed positive: a zero balance is not a runnable
    // experiment, but this must not produce an infinity if one ever reaches it.
    let overstatement = if starting_cash > 0.0 {
        (benchmark_income - strategy_income) / starting_cash
    } else {
        0.0
    };

    DividendGap {
        events,
        strategy_income,
        benchmark_income,
        overstatement,
        adjustment,
        covered: instruments
            .iter()
            .filter(|instrument| dividends.contains_key(*instrument))
            .count(),
        instruments: instruments.len(),
    }
}

/// Shares of `instrument` held when entitlement to an `ex_date` payment was
/// settled.
///
/// # The entitlement rule
///
/// A holder of record at the close *before* the ex-date receives the payment.
/// So someone who sells on the ex-date still receives it, and someone who buys
/// on the ex-date does not. The instant that separates the two is the start of
/// the ex-date: held strictly before it, and not yet closed at it.
///
/// Getting this backwards would credit the wrong side of every payment that
/// lands on an entry or exit day — a small error most of the time and exactly
/// the wrong one on a rule that trades around ex-dates.
///
/// A short position pays the dividend rather than receiving it, so it counts
/// negative. The platform is long-only today; the sign is here because a ledger
/// carries a direction and silently treating a short as a long would be a
/// wrong number rather than an unsupported one.
fn shares_on(ledger: &[Trade], instrument: &str, ex_date: NaiveDate) -> f64 {
    let at: NaiveDateTime = ex_date.and_time(NaiveTime::MIN);
    ledger
        .iter()
        .filter(|trade| matches(trade, instrument))
        .filter(|trade| trade.opened < at && trade.closed.is_none_or(|closed| closed >= at))
        .map(|trade| match trade.direction {
            Direction::Long => trade.quantity,
            Direction::Short => -trade.quantity,
        })
        .sum()
}

/// Whether a trade was in this instrument.
///
/// An empty instrument on a trade is a ledger from before trades carried one.
/// Those runs were all single-instrument, so the trade belongs to whatever
/// instrument is being asked about — the same reading the chart markers take.
/// Refusing to match would report zero income for every stored finding at once,
/// which looks like "this rule collected no dividends" and is not.
fn matches(trade: &Trade, instrument: &str) -> bool {
    trade.instrument.is_empty() || trade.instrument == instrument
}

#[cfg(test)]
mod tests {
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
}
