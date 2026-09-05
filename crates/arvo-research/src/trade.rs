//! The round trips a run actually made.
//!
//! A [`SimulationResult`] used to carry an equity curve and a count. That is
//! enough to say whether a strategy made money and nothing about *how*: a
//! curve that ends up 20% could be thirty small wins or one lucky trade
//! carried by a stopped-out crowd, and those are not the same finding.
//!
//! It is also the missing input for every cost question beyond a fill.
//! Holding period decides short- versus long-term tax treatment; per-trade
//! and per-share fees are charged against a count and a size, not a curve.
//! None of that is computable from an equity series, so the ledger is a
//! prerequisite rather than a nicety.
//!
//! [`SimulationResult`]: crate::SimulationResult

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// Which way a position was held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
}

/// Why a position ended.
///
/// Worth recording because it separates two things a win rate conflates: a
/// rule that exits on its own signal and a rule whose stop keeps saving it.
/// A strategy that only ever leaves through its stop has not been shown to
/// have an exit rule at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The strategy's own exit rule fired.
    Signal,
    /// A protective stop was hit.
    Stop,
    /// The run ended with the position still open.
    ///
    /// Not an exit at all, and kept distinct from one: a run that ends while
    /// holding has an unrealised result that the equity curve marks to market
    /// but the ledger has not realised.
    StillOpen,
}

/// One position, from opening fill to closing fill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub opened: NaiveDateTime,
    /// `None` while the position is still open at the end of the run.
    pub closed: Option<NaiveDateTime>,
    pub direction: Direction,
    /// Peak size held, in units of the instrument.
    pub quantity: f64,
    /// Average fill price in.
    pub entry: f64,
    /// Average fill price out. `None` while still open.
    pub exit: Option<f64>,
    /// Realised profit in account currency, *net* of the commission below —
    /// which is the number that actually landed in the account, not a gross
    /// figure a reader has to remember to subtract from.
    pub pnl: f64,
    /// Everything the venue charged for this round trip: proportional
    /// commission, flat per-order fees and per-share or sell-side charges.
    pub commission: f64,
    pub exit_reason: ExitReason,
}

impl Trade {
    /// How long the position was held. `None` while still open.
    #[must_use]
    pub fn holding_period(&self) -> Option<chrono::Duration> {
        self.closed.map(|closed| closed - self.opened)
    }

    /// Whether this round trip made money after costs.
    ///
    /// `None` for an open position: an unrealised gain is not a win yet, and
    /// counting it as one is how a losing run reports a good win rate.
    #[must_use]
    pub fn is_win(&self) -> Option<bool> {
        self.closed.map(|_| self.pnl > 0.0)
    }
}

/// What the ledger says, once counted.
///
/// Every field is derived from the same `[Trade]` slice, so nothing here can
/// disagree with the ledger it came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TradeStats {
    /// Round trips that actually completed. The denominator for everything
    /// below.
    pub closed: u32,
    /// Positions still open when the run ended. Counted separately rather
    /// than dropped, because a run holding its largest position at the end
    /// has a result the ledger cannot yet confirm.
    pub still_open: u32,
    pub wins: u32,
    pub losses: u32,
    /// Fraction of closed trades that made money. `None` when nothing closed.
    pub win_rate: Option<f64>,
    /// Gross profit over gross loss.
    ///
    /// `None` when there were no losing trades: dividing by zero would report
    /// infinity, which reads as a spectacular result rather than as too small
    /// a sample to have lost yet.
    pub profit_factor: Option<f64>,
    /// Mean profit of the winners, and mean loss of the losers as a positive
    /// number. Together with the win rate these say whether an edge is many
    /// small wins or a few large ones — which decides whether the average is
    /// robust or one trade away from vanishing.
    pub average_win: Option<f64>,
    pub average_loss: Option<f64>,
    pub largest_win: Option<f64>,
    pub largest_loss: Option<f64>,
    /// Mean holding period of closed trades, in seconds.
    ///
    /// Seconds rather than days because the same field has to describe a
    /// five-minute scalp and a two-year hold. It is also the number that
    /// decides short- versus long-term tax treatment.
    pub average_holding_secs: Option<f64>,
    /// Total commission and fees charged across every fill.
    ///
    /// Fees only. Slippage is *not* here and cannot be: it is charged inside
    /// the fill price, so it shows up as a worse entry and exit rather than
    /// as a line item. Calling this the cost of trading would understate the
    /// real one by whatever the spread assumption was.
    pub total_commission: f64,
    /// How the closed trades ended.
    pub signal_exits: u32,
    pub stop_exits: u32,
}

impl TradeStats {
    /// Counts a ledger.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "trade counts are small")]
    pub fn from_ledger(ledger: &[Trade]) -> Self {
        let mut stats = Self {
            closed: 0,
            still_open: 0,
            wins: 0,
            losses: 0,
            win_rate: None,
            profit_factor: None,
            average_win: None,
            average_loss: None,
            largest_win: None,
            largest_loss: None,
            average_holding_secs: None,
            total_commission: ledger.iter().map(|trade| trade.commission).sum(),
            signal_exits: 0,
            stop_exits: 0,
        };

        let mut gross_win = 0.0;
        let mut gross_loss = 0.0;
        let mut holding_secs = 0.0;

        for trade in ledger {
            let Some(is_win) = trade.is_win() else {
                stats.still_open += 1;
                continue;
            };
            stats.closed += 1;
            match trade.exit_reason {
                ExitReason::Signal => stats.signal_exits += 1,
                ExitReason::Stop => stats.stop_exits += 1,
                // Unreachable while `is_win` gates on `closed`, but a match
                // that stays exhaustive is cheaper than one that stops being
                // when a fourth reason appears.
                ExitReason::StillOpen => {}
            }
            if is_win {
                stats.wins += 1;
                gross_win += trade.pnl;
                stats.largest_win = Some(stats.largest_win.map_or(trade.pnl, |m| f64::max(m, trade.pnl)));
            } else {
                stats.losses += 1;
                // A break-even trade counts as a loss above and contributes
                // nothing here, which is the conservative reading.
                gross_loss += -trade.pnl;
                stats.largest_loss = Some(stats.largest_loss.map_or(trade.pnl, |m| f64::min(m, trade.pnl)));
            }
            if let Some(held) = trade.holding_period() {
                holding_secs += held.num_seconds() as f64;
            }
        }

        if stats.closed > 0 {
            let closed = f64::from(stats.closed);
            stats.win_rate = Some(f64::from(stats.wins) / closed);
            stats.average_holding_secs = Some(holding_secs / closed);
        }
        if stats.wins > 0 {
            stats.average_win = Some(gross_win / f64::from(stats.wins));
        }
        if stats.losses > 0 {
            stats.average_loss = Some(gross_loss / f64::from(stats.losses));
        }
        if gross_loss > 0.0 {
            stats.profit_factor = Some(gross_win / gross_loss);
        }
        stats
    }

    /// The mean profit of a trade drawn at random from the closed ones.
    ///
    /// The number that decides whether a strategy is worth running at all:
    /// a high win rate with a negative expectancy is a strategy that wins
    /// often and loses money, which is the most common way a rule looks good.
    #[must_use]
    pub fn expectancy(&self) -> Option<f64> {
        let rate = self.win_rate?;
        let win = self.average_win.unwrap_or(0.0);
        let loss = self.average_loss.unwrap_or(0.0);
        Some(rate * win - (1.0 - rate) * loss)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn trade(open: u32, close: Option<u32>, pnl: f64, reason: ExitReason) -> Trade {
        Trade {
            opened: at(open),
            closed: close.map(at),
            direction: Direction::Long,
            quantity: 100.0,
            entry: 10.0,
            exit: close.map(|_| 11.0),
            pnl,
            commission: 1.0,
            exit_reason: reason,
        }
    }

    #[test]
    fn the_default_is_what_an_empty_ledger_counts_to() {
        // `Evaluation` stores these with `serde(default)`, so a finding saved
        // before the ledger existed reads back as this. It has to mean "no
        // trades recorded" and not some other zero.
        assert_eq!(TradeStats::default(), TradeStats::from_ledger(&[]));
    }

    #[test]
    fn an_empty_ledger_reports_absence_rather_than_zero() {
        let stats = TradeStats::from_ledger(&[]);
        assert_eq!(stats.closed, 0);
        assert_eq!(stats.win_rate, None, "no trades is not a 0% win rate");
        assert_eq!(stats.profit_factor, None);
        assert_eq!(stats.expectancy(), None);
    }

    #[test]
    fn a_win_rate_counts_only_closed_trades() {
        // Two closed, one win; plus an open position that must not be counted
        // as a win on its unrealised gain.
        let ledger = [
            trade(1, Some(2), 100.0, ExitReason::Signal),
            trade(3, Some(4), -50.0, ExitReason::Stop),
            trade(5, None, 0.0, ExitReason::StillOpen),
        ];
        let stats = TradeStats::from_ledger(&ledger);

        assert_eq!(stats.closed, 2);
        assert_eq!(stats.still_open, 1);
        assert_eq!(stats.win_rate, Some(0.5));
        assert_eq!(stats.profit_factor, Some(2.0));
        assert_eq!(stats.signal_exits, 1);
        assert_eq!(stats.stop_exits, 1);
        assert!((stats.total_commission - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_high_win_rate_can_still_have_negative_expectancy() {
        // The failure this stat exists to expose: nine small wins and one
        // large loss reads as a 90% win rate and loses money.
        let mut ledger: Vec<Trade> = (1..10)
            .map(|day| trade(day, Some(day + 1), 10.0, ExitReason::Signal))
            .collect();
        ledger.push(trade(20, Some(21), -200.0, ExitReason::Stop));

        let stats = TradeStats::from_ledger(&ledger);
        assert_eq!(stats.win_rate, Some(0.9));
        let expectancy = stats.expectancy().expect("ten closed trades");
        assert!(expectancy < 0.0, "90% winners, still losing: {expectancy}");
    }

    #[test]
    fn a_run_with_no_losers_reports_no_profit_factor_rather_than_infinity() {
        let ledger = [trade(1, Some(2), 100.0, ExitReason::Signal)];
        let stats = TradeStats::from_ledger(&ledger);
        assert_eq!(
            stats.profit_factor, None,
            "one winner is too small a sample to have lost yet, not infinite skill"
        );
    }

    #[test]
    fn holding_period_is_measured_from_the_ledger() {
        let ledger = [
            trade(1, Some(3), 10.0, ExitReason::Signal),
            trade(4, Some(5), 10.0, ExitReason::Signal),
        ];
        let stats = TradeStats::from_ledger(&ledger);
        let days = stats.average_holding_secs.expect("two closed") / 86_400.0;
        assert!((days - 1.5).abs() < 1e-9, "two and one day average: {days}");
    }
}
