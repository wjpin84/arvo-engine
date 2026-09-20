//! What a result is worth after US capital-gains tax (#26).
//!
//! An overlay, beside the verdict rather than inside it — the same choice
//! ADR-0011 made for dividends. Tax depends on a bracket and an account type
//! this platform does not know, so the pre-tax figure stays the one judged and
//! this says how far it moves at stated rates.
//!
//! # What is modelled
//!
//! * **Holding period.** A gain is long-term when sold more than a year after
//!   it was bought, which the IRS counts from the day after purchase: bought
//!   1 March, sold the next 1 March is still short-term.
//! * **Netting, per calendar year.** Short-term against short-term, long-term
//!   against long-term, then a net loss on one side against a net gain on the
//!   other, the gain keeping its character.
//! * **Carryforward.** A net loss carries into the next year with its character.
//! * **Sold at the end.** Whatever is still held when the window closes is
//!   treated as sold on its last day, and buy-and-hold is treated the same, so
//!   the comparison is between two accounts that cashed out on one date.
//!
//! # What is not
//!
//! ponytail: federal rates only, flat, and paid out of the gain at the end
//! rather than out of the account each April — so tax never shrinks what the
//! rule compounds on, the $3,000 ordinary-income offset is ignored, and wash
//! sales are not detected. Each makes the after-tax figure slightly kinder to a
//! rule that trades often; model them when that rule is the one being chosen.

use std::collections::BTreeMap;

use chrono::{Datelike, Months, NaiveDateTime};

use crate::{EquityPoint, Trade};

/// Tax rates applied to realised gains.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaxRates {
    /// Held a year or less: taxed as ordinary income.
    pub short_term: f64,
    /// Held more than a year.
    pub long_term: f64,
}

/// The rates the overlay assumes: 22%, the federal bracket covering the middle
/// of single filers' taxable income, and the 15% long-term rate that goes with
/// it. Stated beside every figure they produce.
pub const ASSUMED: TaxRates = TaxRates {
    short_term: 0.22,
    long_term: 0.15,
};

/// One account's result, before and after tax.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AfterTax {
    pub tax: f64,
    /// Return on starting equity with the tax taken out.
    pub after_tax_return: f64,
}

/// A gain or loss realised on one date.
#[derive(Debug, Clone, Copy)]
struct Realised {
    year: i32,
    long_term: bool,
    gain: f64,
}

/// Whether a position bought at `opened` and sold at `closed` is long-term.
fn long_term(opened: NaiveDateTime, closed: NaiveDateTime) -> bool {
    opened
        .date()
        .checked_add_months(Months::new(12))
        .is_some_and(|anniversary| closed.date() > anniversary)
}

/// The strategy's after-tax result, from its ledger and its equity curve.
///
/// Closed trades are realised when they closed. Everything else the curve
/// gained — open positions, and income the ledger does not itemise — is
/// realised on the last day: long-term only if every position still open had
/// been held more than a year by then, and short-term otherwise, including when
/// nothing was open, since unitemised income is not a long-term gain.
///
/// `None` when the curve is too short to have a start and an end.
#[must_use]
pub fn strategy(ledger: &[Trade], curve: &[EquityPoint], rates: TaxRates) -> Option<AfterTax> {
    let (first, last) = (curve.first()?, curve.last()?);
    let mut realised: Vec<Realised> = Vec::new();
    let mut open = Vec::new();
    let mut closed_gain = 0.0;
    for trade in ledger {
        match trade.closed {
            Some(closed) => {
                closed_gain += trade.pnl;
                realised.push(Realised {
                    year: closed.year(),
                    long_term: long_term(trade.opened, closed),
                    gain: trade.pnl,
                });
            }
            None => open.push(trade.opened),
        }
    }
    realised.push(Realised {
        year: last.at.year(),
        long_term: !open.is_empty() && open.iter().all(|opened| long_term(*opened, last.at)),
        gain: last.equity - first.equity - closed_gain,
    });
    Some(settle(first.equity, last.equity - first.equity, &realised, rates))
}

/// Buy-and-hold's after-tax result: one purchase on the first day, one sale on
/// the last.
#[must_use]
pub fn buy_and_hold(curve: &[EquityPoint], rates: TaxRates) -> Option<AfterTax> {
    let (first, last) = (curve.first()?, curve.last()?);
    let gain = last.equity - first.equity;
    let realised = [Realised {
        year: last.at.year(),
        long_term: long_term(first.at, last.at),
        gain,
    }];
    Some(settle(first.equity, gain, &realised, rates))
}

fn settle(start: f64, gain: f64, realised: &[Realised], rates: TaxRates) -> AfterTax {
    let tax = tax(realised, rates);
    AfterTax {
        tax,
        after_tax_return: if start == 0.0 { 0.0 } else { (gain - tax) / start },
    }
}

/// Tax owed on a set of realisations, netted per year with losses carried.
fn tax(realised: &[Realised], rates: TaxRates) -> f64 {
    let mut by_year: BTreeMap<i32, (f64, f64)> = BTreeMap::new();
    for item in realised {
        let (short, long) = by_year.entry(item.year).or_default();
        if item.long_term {
            *long += item.gain;
        } else {
            *short += item.gain;
        }
    }

    let (mut carried_short, mut carried_long) = (0.0, 0.0);
    let mut owed = 0.0;
    for (short, long) in by_year.into_values() {
        let (mut short, mut long) = (short + carried_short, long + carried_long);
        // A net loss on one side comes off a net gain on the other, and what
        // is left keeps the character of the side that was larger.
        if short < 0.0 && long > 0.0 {
            long += short;
            short = long.min(0.0);
            long = long.max(0.0);
        } else if long < 0.0 && short > 0.0 {
            short += long;
            long = short.min(0.0);
            short = short.max(0.0);
        }
        owed += short.max(0.0) * rates.short_term + long.max(0.0) * rates.long_term;
        carried_short = short.min(0.0);
        carried_long = long.min(0.0);
    }
    owed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(date: &str) -> NaiveDateTime {
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn point(date: &str, equity: f64) -> EquityPoint {
        EquityPoint { at: at(date), equity }
    }

    fn trade(opened: &str, closed: Option<&str>, pnl: f64) -> Trade {
        Trade {
            instrument: String::new(),
            opened: at(opened),
            closed: closed.map(at),
            direction: crate::Direction::Long,
            quantity: 1.0,
            entry: 1.0,
            exit: closed.map(|_| 1.0),
            pnl,
            commission: 0.0,
            exit_reason: crate::ExitReason::Signal,
            journal: None,
        }
    }

    #[test]
    fn a_year_to_the_day_is_still_short_term() {
        assert!(!long_term(at("2024-03-01"), at("2025-03-01")));
        assert!(long_term(at("2024-03-01"), at("2025-03-02")));
    }

    #[test]
    fn short_and_long_gains_are_taxed_at_their_own_rates() {
        let ledger = [
            trade("2024-01-02", Some("2024-06-01"), 1_000.0),
            trade("2024-01-02", Some("2025-06-01"), 1_000.0),
        ];
        let curve = [point("2024-01-02", 10_000.0), point("2025-06-01", 12_000.0)];
        let after = strategy(&ledger, &curve, ASSUMED).expect("curve");
        assert!((after.tax - (220.0 + 150.0)).abs() < 1e-9, "{after:?}");
        assert!((after.after_tax_return - (2_000.0 - 370.0) / 10_000.0).abs() < 1e-12);
    }

    #[test]
    fn a_loss_carries_into_the_next_year() {
        let ledger = [
            trade("2024-01-02", Some("2024-06-01"), -500.0),
            trade("2025-01-02", Some("2025-06-01"), 1_000.0),
        ];
        let curve = [point("2024-01-02", 10_000.0), point("2025-06-01", 10_500.0)];
        let after = strategy(&ledger, &curve, ASSUMED).expect("curve");
        assert!((after.tax - 500.0 * 0.22).abs() < 1e-9, "{after:?}");
    }

    #[test]
    fn a_long_term_loss_comes_off_a_short_term_gain_in_the_same_year() {
        let ledger = [
            trade("2024-01-02", Some("2024-06-01"), 1_000.0),
            trade("2023-01-02", Some("2024-06-01"), -400.0),
        ];
        let curve = [point("2023-01-02", 10_000.0), point("2024-06-01", 10_600.0)];
        let after = strategy(&ledger, &curve, ASSUMED).expect("curve");
        assert!((after.tax - 600.0 * 0.22).abs() < 1e-9, "{after:?}");
    }

    #[test]
    fn what_is_still_held_is_sold_on_the_last_day() {
        // Open for eighteen months with a $3,000 mark: long-term at the end.
        let ledger = [trade("2024-01-02", None, 0.0)];
        let curve = [point("2024-01-02", 10_000.0), point("2025-07-01", 13_000.0)];
        let after = strategy(&ledger, &curve, ASSUMED).expect("curve");
        assert!((after.tax - 3_000.0 * 0.15).abs() < 1e-9, "{after:?}");

        let holding = buy_and_hold(&curve, ASSUMED).expect("curve");
        assert_eq!(after, holding, "the same position, the same tax");
    }

    #[test]
    fn a_losing_run_owes_nothing() {
        let curve = [point("2024-01-02", 10_000.0), point("2024-12-31", 9_000.0)];
        let after = buy_and_hold(&curve, ASSUMED).expect("curve");
        assert!(after.tax.abs() < f64::EPSILON);
        assert!((after.after_tax_return + 0.1).abs() < 1e-12);
    }
}
