//! The round trips a run actually made.
//!
//! A `SimulationResult` used to carry an equity curve and a count. That is
//! enough to say whether a strategy made money and nothing about *how*: a
//! curve that ends up 20% could be thirty small wins or one lucky trade
//! carried by a stopped-out crowd, and those are not the same finding.
//!
//! It is also the missing input for every cost question beyond a fill.
//! Holding period decides short- versus long-term tax treatment; per-trade
//! and per-share fees are charged against a count and a size, not a curve.
//! None of that is computable from an equity series, so the ledger is a
//! prerequisite rather than a nicety.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::EquityPoint;

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
    /// The account's drawdown limit was reached, so the rule was stopped and
    /// whatever it held was closed.
    ///
    /// Not a stop and not a signal: the position did not fail on its own
    /// terms, the *account* did. Counting it as either would misdescribe both
    /// the trade and the run — and a run that ended early is a different claim
    /// from one that ran its window out.
    Halted,
    /// The contract reached expiration and was settled rather than traded out
    /// of (#84): in cash at its intrinsic value against the underlying's close
    /// on the expiration date, which is nothing when it expired out of the
    /// money. Charges no commission.
    Expired,
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
    /// Which instrument this round trip was in, as `SYMBOL.VENUE`.
    ///
    /// Empty for a single-instrument run recorded before a run could hold more
    /// than one, and for one where the ledger's instrument is the experiment's
    /// and saying so twice would be the only thing it added.
    ///
    /// `default` because this is a persisted format: a finding stored before
    /// the ledger named instruments still loads, as one whose trades do not
    /// say which instrument they were in — which is the honest description of
    /// a record from when every trade was in the same one.
    #[serde(default)]
    pub instrument: String,
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
    /// Option contracts that reached expiration and were settled (#84).
    #[serde(default)]
    pub expired_exits: u32,
    /// Whether the run stopped early because the account's drawdown limit was
    /// reached.
    ///
    /// A material fact about what every other number means: a halted run
    /// reports the return it had when it stopped, over a window it did not
    /// finish.
    #[serde(default)]
    pub halted: bool,
}

/// A ledger as positions: option legs taken on and off together as one (#86).
///
/// A put spread is two contracts, and the engine records each as its own round
/// trip. Counted that way, one spread is two trades — which halves the trade
/// count the evaluation requires before it will read a result, and makes every
/// spread one win and one loss whatever it did. Found when twelve SPY spreads
/// reported "24 trades, 50% won".
///
/// Legs are one position when they are option contracts on the same underlying
/// and expiration, opened at the same instant and closed at the same instant
/// (or both still open). Anything else — a stock, a leg unwound on its own, two
/// members of a book that happened to trade together — is left as it was.
///
/// The combined trade nets its legs per share: entry is the credit (or debit)
/// taken, exit what it cost to close, quantity the largest leg, direction short
/// when a short leg is the dearer, and profit and commission their sums. So its
/// profit is still what its own entry, exit and size say it is.
#[must_use]
pub fn positions(ledger: &[Trade]) -> Vec<Trade> {
    use arvo_data::option::OptionContract;

    let key = |trade: &Trade| {
        OptionContract::parse(&trade.instrument).map(|contract| {
            (contract.underlying, contract.expiration, trade.opened, trade.closed)
        })
    };
    let mut out: Vec<Trade> = Vec::with_capacity(ledger.len());
    let mut taken = vec![false; ledger.len()];
    for (index, trade) in ledger.iter().enumerate() {
        if taken[index] {
            continue;
        }
        let Some(group_key) = key(trade) else {
            out.push(trade.clone());
            continue;
        };
        let legs: Vec<usize> = (index..ledger.len())
            .filter(|&other| !taken[other] && key(&ledger[other]).as_ref() == Some(&group_key))
            .collect();
        if legs.len() < 2 {
            out.push(trade.clone());
            continue;
        }
        let sign = |leg: &Trade| match leg.direction {
            Direction::Long => -1.0,
            Direction::Short => 1.0,
        };
        let mut combined = trade.clone();
        combined.instrument = legs
            .iter()
            .map(|&leg| ledger[leg].instrument.as_str())
            .collect::<Vec<_>>()
            .join("+");
        // Credit taken per share: shorts sold add, longs bought subtract.
        let credit: f64 = legs.iter().map(|&leg| sign(&ledger[leg]) * ledger[leg].entry).sum();
        let debit: Option<f64> = legs
            .iter()
            .map(|&leg| ledger[leg].exit.map(|exit| sign(&ledger[leg]) * exit))
            .sum();
        combined.direction = if credit >= 0.0 { Direction::Short } else { Direction::Long };
        let flip = match combined.direction {
            Direction::Short => 1.0,
            Direction::Long => -1.0,
        };
        combined.entry = flip * credit;
        combined.exit = debit.map(|debit| flip * debit);
        combined.quantity = legs.iter().map(|&leg| ledger[leg].quantity).fold(0.0, f64::max);
        combined.pnl = legs.iter().map(|&leg| ledger[leg].pnl).sum();
        combined.commission = legs.iter().map(|&leg| ledger[leg].commission).sum();
        combined.exit_reason = legs
            .iter()
            .map(|&leg| &ledger[leg])
            .find(|leg| leg.direction == Direction::Short)
            .unwrap_or(trade)
            .exit_reason;
        for &leg in &legs {
            taken[leg] = true;
        }
        out.push(combined);
    }
    out
}

impl TradeStats {
    /// Counts a ledger, as positions — see [`positions`].
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "trade counts are small")]
    pub fn from_ledger(ledger: &[Trade]) -> Self {
        let combined = positions(ledger);
        let ledger = combined.as_slice();
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
            expired_exits: 0,
            halted: false,
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
                ExitReason::Expired => stats.expired_exits += 1,
                ExitReason::Halted => {
                    stats.stop_exits += 1;
                    stats.halted = true;
                }
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

/// Account equity at the close of every bar, from the ledger and the prices.
///
/// # Why this is not the engine's own returns series
///
/// Nautilus reports `returns_series`, and using it was wrong in a way that
/// took a real strategy to expose. It is the *day-over-day change in the
/// account's cash balance*, keeping one balance per day — and on a cash
/// account `balance.total` is cash, which excludes the market value of
/// anything held. So buying reads as a catastrophic loss, selling reads as an
/// enormous gain, and the size of both is the position's notional rather than
/// its profit.
///
/// It survived undetected because on daily bars with a small position the
/// distortion was a plausible-looking wobble, and because the endpoints happen
/// to agree once everything is closed. An intraday run that put most of the
/// account into one trade reported +98% on two losing trades.
///
/// Two things follow from computing it here instead:
///
/// * it reconciles with the ledger by construction — the same realised P&L
///   produces both, so they cannot disagree;
/// * there is one point per bar rather than one per day, which is what the
///   annualisation factor already assumed. Annualising a daily series by the
///   five-minute factor was inflating volatility by about nine times.
///
/// Open positions are marked to market at each bar's close, so a drawdown
/// while holding is visible. That matters: a curve built from realised profit
/// alone is a step function, and a position that halves and recovers would
/// show no drawdown at all.
///
/// One approximation, stated: commission is charged in [`Trade::pnl`] at the
/// close, so while a position is open its entry commission is not yet
/// subtracted. It is a fee's worth of optimism for the length of one trade,
/// and it is gone by the time the trade lands in the curve.
#[must_use]
pub fn equity_curve(
    starting_cash: f64,
    bars: &[(String, Vec<arvo_data::Bar>)],
    interval: arvo_data::BarInterval,
    ledger: &[Trade],
) -> Vec<EquityPoint> {
    // Every instant any instrument reported. A book of instruments on
    // different calendars is measured on all of their bars rather than only
    // the ones they share, and a single-instrument run is unchanged: the union
    // of one series is that series.
    let mut instants: Vec<NaiveDateTime> = bars
        .iter()
        .flat_map(|(_, series)| series.iter().map(|bar| bar.at))
        .collect();
    instants.sort_unstable();
    instants.dedup();

    let mut curve = Vec::with_capacity(instants.len() + 1);
    let Some(first) = instants.first().copied() else {
        return curve;
    };

    // The account before anything happened, timestamped at the first bar's
    // open. Without it a curve of one closed trade has a single point and no
    // return can be computed from it.
    curve.push(EquityPoint {
        at: first,
        equity: starting_cash,
    });

    // Last close seen per instrument, so a position in an instrument that did
    // not print on this instant is marked at its most recent price rather than
    // vanishing from the account for a bar.
    let mut last_close: std::collections::HashMap<&str, f64> =
        std::collections::HashMap::with_capacity(bars.len());
    let mut cursor: Vec<usize> = vec![0; bars.len()];

    for instant in instants {
        // A bar is only knowable once its period has closed — the same
        // convention the engine boundary timestamps bars with, so trade
        // instants and curve instants are on the same clock.
        let at = instant + interval.duration();

        for (index, (instrument, series)) in bars.iter().enumerate() {
            while cursor[index] < series.len() && series[cursor[index]].at <= instant {
                last_close.insert(instrument.as_str(), series[cursor[index]].close);
                cursor[index] += 1;
            }
        }

        let mut equity = starting_cash;
        for trade in ledger {
            match trade.closed {
                Some(closed) if closed <= at => equity += trade.pnl,
                // Held right now: mark it to the latest close of the
                // instrument it is in. A short gains what the price loses.
                _ if trade.opened <= at => {
                    // An empty instrument is a ledger from before trades named
                    // one, which only ever happens on a single-instrument run
                    // — so the one series present is the right one to mark
                    // against.
                    let close = if trade.instrument.is_empty() {
                        bars.first().and_then(|(name, _)| last_close.get(name.as_str()))
                    } else {
                        last_close.get(trade.instrument.as_str())
                    };
                    if let Some(close) = close {
                        let sign = match trade.direction {
                            Direction::Long => 1.0,
                            Direction::Short => -1.0,
                        };
                        equity += sign * trade.quantity * (close - trade.entry);
                    }
                }
                _ => {}
            }
        }

        curve.push(EquityPoint { at, equity });
    }
    curve
}

impl TradeStats {
    /// Adds several runs' statistics together as if they were one record.
    ///
    /// Every field is additive or reconstructible from additive parts, which
    /// is why this is exact rather than an approximation: gross profit is
    /// `average_win × wins`, and summing those across folds gives the same
    /// number a single pass over the concatenated ledgers would.
    #[must_use]
    pub fn combine<'a>(parts: impl Iterator<Item = &'a Self>) -> Self {
        let mut out = Self::default();
        let mut gross_win = 0.0;
        let mut gross_loss = 0.0;
        let mut holding = 0.0;

        for part in parts {
            out.closed += part.closed;
            out.still_open += part.still_open;
            out.wins += part.wins;
            out.losses += part.losses;
            out.total_commission += part.total_commission;
            out.signal_exits += part.signal_exits;
            out.stop_exits += part.stop_exits;
            // Any fold halting is the whole record halting: the stitched run
            // has a hole in it wherever that fold stopped early.
            out.halted |= part.halted;
            gross_win += part.average_win.unwrap_or_default() * f64::from(part.wins);
            gross_loss += part.average_loss.unwrap_or_default() * f64::from(part.losses);
            holding += part.average_holding_secs.unwrap_or_default() * f64::from(part.closed);
            out.largest_win = pick(out.largest_win, part.largest_win, f64::max);
            out.largest_loss = pick(out.largest_loss, part.largest_loss, f64::min);
        }

        if out.closed > 0 {
            let closed = f64::from(out.closed);
            out.win_rate = Some(f64::from(out.wins) / closed);
            out.average_holding_secs = Some(holding / closed);
        }
        if out.wins > 0 {
            out.average_win = Some(gross_win / f64::from(out.wins));
        }
        if out.losses > 0 {
            out.average_loss = Some(gross_loss / f64::from(out.losses));
        }
        if gross_loss > 0.0 {
            out.profit_factor = Some(gross_win / gross_loss);
        }
        out
    }
}

fn pick(left: Option<f64>, right: Option<f64>, choose: fn(f64, f64) -> f64) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(choose(left, right)),
        (found, None) | (None, found) => found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(instrument: &str, direction: Direction, entry: f64, exit: f64, close_day: u32) -> Trade {
        let sign = match direction {
            Direction::Long => 1.0,
            Direction::Short => -1.0,
        };
        Trade {
            instrument: instrument.to_owned(),
            opened: at(2),
            closed: Some(at(close_day)),
            direction,
            quantity: 100.0,
            entry,
            exit: Some(exit),
            pnl: sign * (exit - entry) * 100.0 - 1.0,
            commission: 1.0,
            exit_reason: ExitReason::Signal,
        }
    }

    #[test]
    fn a_spread_is_one_position_whose_profit_is_its_own_prices() {
        // Sold the 500 put at 2.00, bought the 495 at 1.20: a 0.80 credit.
        // Closed for 0.30 (0.50 and 0.20). Kept 0.50 a share.
        let short = leg("SPY240315P00500000.AOPT", Direction::Short, 2.00, 0.50, 9);
        let long = leg("SPY240315P00495000.AOPT", Direction::Long, 1.20, 0.20, 9);
        let combined = positions(&[short, long]);
        let [spread] = combined.as_slice() else {
            panic!("{combined:?}");
        };
        assert_eq!(spread.direction, Direction::Short);
        assert!((spread.entry - 0.80).abs() < 1e-9 && (spread.exit.expect("closed") - 0.30).abs() < 1e-9);
        assert!((spread.pnl - (48.0)).abs() < 1e-9, "{}", spread.pnl);
        assert!((spread.pnl - ((spread.entry - spread.exit.expect("closed")) * spread.quantity - spread.commission)).abs() < 1e-9);

        let stats = TradeStats::from_ledger(&[
            leg("SPY240315P00500000.AOPT", Direction::Short, 2.00, 0.50, 9),
            leg("SPY240315P00495000.AOPT", Direction::Long, 1.20, 0.20, 9),
        ]);
        assert_eq!((stats.closed, stats.wins, stats.losses), (1, 1, 0), "one spread, one win");
    }

    #[test]
    fn only_legs_taken_on_and_off_together_are_combined() {
        let stock = Trade { instrument: "SPY.AIEX".to_owned(), ..leg("x", Direction::Long, 1.0, 2.0, 9) };
        let other_expiry = leg("SPY240419P00495000.AOPT", Direction::Long, 1.2, 0.2, 9);
        let unwound_alone = leg("SPY240315P00490000.AOPT", Direction::Long, 1.0, 0.9, 3);
        let short = leg("SPY240315P00500000.AOPT", Direction::Short, 2.0, 0.5, 9);
        let ledger = [stock, other_expiry, unwound_alone, short];
        assert_eq!(positions(&ledger).len(), 4, "nothing here shares a key");
    }

    fn at(day: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn trade(open: u32, close: Option<u32>, pnl: f64, reason: ExitReason) -> Trade {
        Trade {
            instrument: String::new(),
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

    /// One instrument's series, in the shape the curve now takes.
    fn one(instrument: &str, bars: Vec<arvo_data::Bar>) -> Vec<(String, Vec<arvo_data::Bar>)> {
        vec![(instrument.to_owned(), bars)]
    }

    fn bar(day: u32, close: f64) -> arvo_data::Bar {
        arvo_data::Bar {
            at: at(day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1_000.0,
        }
    }

    #[test]
    fn a_curve_ends_where_the_ledger_says_it_should() {
        // The property the engine's own returns series did not have: the
        // curve and the ledger are the same statement about the same run.
        let bars: Vec<_> = (1..=5).map(|day| bar(day, 100.0)).collect();
        let ledger = [Trade {
            instrument: String::new(),
            opened: at(1),
            closed: Some(at(3)),
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: Some(110.0),
            pnl: 95.0,
            commission: 5.0,
            exit_reason: ExitReason::Signal,
        }];

        let curve = equity_curve(1_000.0, &one("X.SIM", bars.clone()), arvo_data::BarInterval::DAILY, &ledger);
        let last = curve.last().expect("non-empty").equity;
        assert!((last - 1_095.0).abs() < 1e-9, "{last}");
    }

    #[test]
    fn an_open_position_is_marked_to_market_rather_than_ignored() {
        // A curve built from realised profit alone is a step function: a
        // position that halves and recovers would show no drawdown, and
        // drawdown is one of the criteria a verdict turns on.
        let bars = vec![bar(1, 100.0), bar(2, 50.0), bar(3, 100.0)];
        let ledger = [Trade {
            instrument: String::new(),
            opened: at(1),
            closed: None,
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: None,
            pnl: 0.0,
            commission: 0.0,
            exit_reason: ExitReason::StillOpen,
        }];

        let curve = equity_curve(1_000.0, &one("X.SIM", bars.clone()), arvo_data::BarInterval::DAILY, &ledger);
        let trough = curve
            .iter()
            .map(|point| point.equity)
            .fold(f64::INFINITY, f64::min);
        assert!(
            (trough - 500.0).abs() < 1e-9,
            "halving a fully-invested position is a 50% drawdown, got {trough}"
        );
    }

    #[test]
    fn a_curve_has_a_point_for_every_bar_plus_its_opening_balance() {
        // One point per *bar*, not per day. The annualisation factor already
        // assumed this; a daily series scaled by the five-minute factor
        // overstated volatility by about nine times.
        let bars: Vec<_> = (1..=7).map(|day| bar(day, 100.0)).collect();
        let curve = equity_curve(1_000.0, &one("X.SIM", bars.clone()), arvo_data::BarInterval::DAILY, &[]);
        assert_eq!(curve.len(), 8);
        assert!(curve.iter().all(|point| point.equity == 1_000.0));
    }

    #[test]
    fn no_bars_is_an_empty_curve_rather_than_a_lone_opening_balance() {
        // A single point reads as "the account never moved". Nothing ran.
        assert!(equity_curve(1_000.0, &[], arvo_data::BarInterval::DAILY, &[]).is_empty());
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

    #[test]
    fn a_position_is_marked_against_its_own_instrument_not_the_first_one() {
        // The whole reason the ledger names instruments. Marking every open
        // position against one series would price a held AAPL at MSFT's close,
        // which is not wrong by a little.
        let steady: Vec<_> = (1..=3).map(|day| bar(day, 100.0)).collect();
        let halved = vec![bar(1, 100.0), bar(2, 50.0), bar(3, 50.0)];
        let bars = vec![
            ("STEADY.SIM".to_owned(), steady),
            ("FALLER.SIM".to_owned(), halved),
        ];
        let ledger = [Trade {
            instrument: "FALLER.SIM".to_owned(),
            opened: at(1),
            closed: None,
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: None,
            pnl: 0.0,
            commission: 0.0,
            exit_reason: ExitReason::StillOpen,
        }];

        let curve = equity_curve(1_000.0, &bars, arvo_data::BarInterval::DAILY, &ledger);
        let last = curve.last().expect("non-empty").equity;
        assert!(
            (last - 500.0).abs() < 1e-9,
            "a halved position is a 500 loss on a 1,000 account, got {last}"
        );
    }

    #[test]
    fn an_instrument_that_did_not_print_holds_its_last_price() {
        // What actually happens to a position on a day its instrument does not
        // trade. Dropping it from the account for that bar would draw a
        // drawdown and a recovery that never happened.
        let dense: Vec<_> = (1..=3).map(|day| bar(day, 100.0)).collect();
        let sparse = vec![bar(1, 100.0), bar(3, 100.0)];
        let bars = vec![
            ("DENSE.SIM".to_owned(), dense),
            ("SPARSE.SIM".to_owned(), sparse),
        ];
        let ledger = [Trade {
            instrument: "SPARSE.SIM".to_owned(),
            opened: at(1),
            closed: None,
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: None,
            pnl: 0.0,
            commission: 0.0,
            exit_reason: ExitReason::StillOpen,
        }];

        let curve = equity_curve(1_000.0, &bars, arvo_data::BarInterval::DAILY, &ledger);
        assert!(
            curve.iter().all(|point| (point.equity - 1_000.0).abs() < 1e-9),
            "nothing moved, so the account did not: {curve:?}"
        );
    }

    #[test]
    fn two_instruments_are_measured_on_every_bar_either_of_them_reported() {
        let dense: Vec<_> = (1..=4).map(|day| bar(day, 100.0)).collect();
        let sparse = vec![bar(2, 100.0), bar(5, 100.0)];
        let bars = vec![
            ("DENSE.SIM".to_owned(), dense),
            ("SPARSE.SIM".to_owned(), sparse),
        ];

        let curve = equity_curve(1_000.0, &bars, arvo_data::BarInterval::DAILY, &[]);
        // Days 1-4 from one and 5 from the other, plus the opening balance.
        assert_eq!(curve.len(), 6, "{curve:?}");
    }

    #[test]
    fn a_ledger_from_before_trades_named_instruments_still_marks_to_market() {
        // Findings recorded before this field existed load with it empty. They
        // were all single-instrument, so the one series present is the right
        // one to mark against — and reading the empty name as "no instrument"
        // would silently drop the position from the curve.
        let bars = vec![bar(1, 100.0), bar(2, 50.0)];
        let ledger = [Trade {
            instrument: String::new(),
            opened: at(1),
            closed: None,
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: None,
            pnl: 0.0,
            commission: 0.0,
            exit_reason: ExitReason::StillOpen,
        }];

        let curve = equity_curve(
            1_000.0,
            &one("X.SIM", bars),
            arvo_data::BarInterval::DAILY,
            &ledger,
        );
        let last = curve.last().expect("non-empty").equity;
        assert!((last - 500.0).abs() < 1e-9, "{last}");
    }
}
