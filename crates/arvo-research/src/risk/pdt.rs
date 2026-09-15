//! FINRA's pattern-day-trader rule, as a count over a ledger.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::Trade;

/// Whether the account is subject to FINRA's pattern-day-trader rule.
///
/// # Why this belongs in `RiskModel` and therefore in every experiment
///
/// Because a backtest that ignores it is backtesting a system that cannot
/// legally be run. A day-trading rule on a $2,000 margin account gets three
/// round trips per five business days in reality and unlimited ones in a
/// simulation that does not model the rule — so the simulation's trade count,
/// its return, and the verdict drawn from them all describe an account nobody
/// can open.
///
/// Pinned into the experiment like every other risk decision, so a stored
/// finding says which constraint it ran under rather than leaving a reader to
/// assume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DayTradingRule {
    /// No day-trading constraint modelled. Every finding recorded before this
    /// existed, and the honest description of them.
    #[default]
    Unconstrained,
    /// FINRA Rule 4210 as it applies to a margin account: four day trades in
    /// five rolling business days flags the account, and a flagged account must
    /// hold [`PDT_EQUITY_FLOOR`] to keep day trading.
    ///
    /// A *cash* account is not subject to this and is deliberately not a
    /// variant here. It has its own constraint — proceeds settle T+1 and
    /// spending them early is a good-faith violation — and adding a `Cash`
    /// variant before settlement is modelled would be a variant that claims a
    /// constraint it does not enforce.
    PatternDayTrader,
}

/// Equity below which the pattern-day-trader rule bites.
pub const PDT_EQUITY_FLOOR: f64 = 25_000.0;

/// Day trades allowed in the window before the next one flags the account.
///
/// Three. The rule flags on the *fourth*, so three is what you may use.
pub const PDT_DAY_TRADES: usize = 3;

/// How many business days the count rolls over.
pub const PDT_WINDOW_DAYS: i64 = 5;

/// Day trades in the trailing window, from a ledger.
///
/// A day trade is a round trip opened and closed on the same day. Shared rather
/// than counted separately by each caller, for the reason every other shared
/// policy here exists: a live session and a backtest counting differently would
/// be two systems, and the stored finding would describe neither.
///
/// ponytail: business days are weekdays — market holidays are not excluded,
/// because there is no exchange calendar in this codebase. The effect is a
/// window that occasionally reaches one day further back than the rule does,
/// which refuses slightly more often than the broker would. Erring toward
/// refusing is the safe direction; add a calendar when one exists for another
/// reason.
#[must_use]
pub fn day_trades_in_window(ledger: &[Trade], now: NaiveDate, business_days: i64) -> usize {
    let earliest = business_days_before(now, business_days);
    ledger
        .iter()
        .filter(|trade| {
            let Some(closed) = trade.closed else {
                // Still open, so not yet a round trip at all.
                return false;
            };
            trade.opened.date() == closed.date()
                && closed.date() >= earliest
                && closed.date() <= now
        })
        .count()
}

/// The date `business_days` weekdays before `from`, counting `from` as one.
///
/// Public because the backtest engine counts the same window from the engine's
/// own ledger, and two definitions of "five business days" would be two rules.
#[must_use]
pub fn business_days_before(from: NaiveDate, business_days: i64) -> NaiveDate {
    let mut counted = 1;
    let mut at = from;
    while counted < business_days.max(1) {
        at = at.pred_opt().unwrap_or(at);
        if !matches!(at.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
            counted += 1;
        }
    }
    at
}
