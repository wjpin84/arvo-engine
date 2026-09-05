//! Strategies Arvo can ask Nautilus to run.
//!
//! One so far, deliberately. The first slice tests whether the *loop* works —
//! hypothesis to experiment to evidence — and a strategy nobody disputes is
//! the right instrument for that. A moving-average crossover is not a good
//! trading idea; it is a good control.
//!
//! The second is [`BuyAndHold`], which exists so evaluation has something to
//! score against. It is not decoration: absolute return mostly measures
//! whether the market went up, so a result with no benchmark is not a result.
//!
//! A strategy here is a Nautilus component, which is why it lives on this side
//! of the boundary. `arvo-research` names it by string in `StrategySpec` and
//! never sees the type.

use std::collections::VecDeque;

/// Tags stamped on a closing order to say why it was sent.
///
/// Read back by [`crate::ledger`], which is the other half of this contract:
/// change a spelling here and the ledger silently reclassifies every exit.
pub(crate) const EXIT_STOP: &str = "arvo:exit=stop";
pub(crate) const EXIT_SIGNAL: &str = "arvo:exit=signal";

use std::fmt::Debug;

use nautilus_common::actor::DataActor;
use nautilus_model::{
    data::{Bar, BarType},
    enums::OrderSide,
    identifiers::InstrumentId,
    types::Quantity,
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyCore},
};

/// Simple moving average over the last `period` values.
///
/// Hand-rolled rather than pulling in `nautilus-indicators`: it is a running
/// sum over a ring buffer, and one fewer pinned `0.x` crate is worth more than
/// the ten lines saved.
#[derive(Debug)]
struct Sma {
    period: usize,
    window: VecDeque<f64>,
    sum: f64,
}

impl Sma {
    fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
        }
    }

    /// Feeds a value in, returning the average once `period` values are held.
    ///
    /// `None` until the window is full — an average over three of a ten-day
    /// window is not a ten-day average, and treating it as one is a quiet way
    /// to manufacture signal at the start of every backtest.
    fn update(&mut self, value: f64) -> Option<f64> {
        self.window.push_back(value);
        self.sum += value;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or(0.0);
        }
        if self.window.len() < self.period {
            return None;
        }
        Some(self.sum / self.period as f64)
    }
}

/// Average true range: how far this instrument actually moves in a bar.
///
/// True range is the widest of the bar's own span and the two gaps to the
/// previous close, so an overnight gap counts as movement rather than being
/// invisible. That matters for a stop: a gap is exactly the move a stop exists
/// to survive, and a range that ignored it would size stops off the calm part
/// of the distribution.
#[derive(Debug)]
struct Atr {
    period: usize,
    window: VecDeque<f64>,
    sum: f64,
    previous_close: Option<f64>,
}

impl Atr {
    fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
            previous_close: None,
        }
    }

    /// Feeds a bar in, returning the average once `period` ranges are held.
    fn update(&mut self, high: f64, low: f64, close: f64) -> Option<f64> {
        let range = match self.previous_close {
            None => high - low,
            Some(previous) => (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
        };
        self.previous_close = Some(close);

        self.window.push_back(range);
        self.sum += range;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or(0.0);
        }
        if self.window.len() < self.period {
            return None;
        }
        Some(self.sum / self.period as f64)
    }
}

/// What a strategy does to protect a position, resolved from the experiment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Risk {
    pub(crate) stop_atr_multiple: Option<f64>,
    pub(crate) atr_period: usize,
    /// Capital-at-risk per trade, already in currency rather than a fraction.
    pub(crate) risk_amount: Option<f64>,
    /// The most one position may be worth, in currency.
    pub(crate) max_position_value: Option<f64>,
}

/// Buys when the fast average crosses above the slow one, sells when it
/// crosses back below.
///
/// With a risk model it also carries a stop, and sizes the position so that
/// being stopped out costs roughly the stated amount.
pub(crate) struct SmaCross {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    trade_size: Quantity,
    fast: Sma,
    slow: Sma,
    /// `None` until both averages have filled, so the first crossing observed
    /// is a real crossing rather than an artefact of starting up.
    previous_fast_above: Option<bool>,
    risk: Risk,
    atr: Atr,
    /// Where this position gets out if it goes wrong. `None` when flat, or
    /// when no stop was configured.
    stop: Option<f64>,
    /// What is actually held, so an exit closes the position rather than a
    /// default quantity.
    ///
    /// Once sizing varies per trade these are no longer the same number, and
    /// exiting with the default silently leaves a remainder on the book — a
    /// position that outlives the signal that opened it and keeps losing after
    /// the stop was supposed to have ended it.
    held: Option<Quantity>,
}

impl SmaCross {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        fast_period: usize,
        slow_period: usize,
        risk: Risk,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            trade_size,
            fast: Sma::new(fast_period),
            slow: Sma::new(slow_period),
            previous_fast_above: None,
            atr: Atr::new(risk.atr_period),
            risk,
            stop: None,
            held: None,
        }
    }

    /// How many shares to buy so that a stop-out costs about the stated risk.
    ///
    /// Rounded down to whole shares, and `None` when that rounds to zero —
    /// buying a share anyway would silently risk more than the model allows,
    /// which is the failure this sizing exists to prevent.
    fn sized(&self, stop_distance: f64, price: f64) -> Option<Quantity> {
        let risk_amount = self.risk.risk_amount?;
        if stop_distance <= 0.0 || price <= 0.0 {
            return None;
        }
        let mut shares = (risk_amount / stop_distance).floor();

        // A tighter stop asks for a bigger position, so this is where an
        // intraday stop of a dollar tries to buy several accounts' worth.
        // Capping is what turns that into a smaller trade rather than a
        // rejected order and a silently empty backtest.
        if let Some(cap) = self.risk.max_position_value {
            shares = shares.min((cap / price).floor());
        }

        if shares < 1.0 {
            return None;
        }
        Quantity::new_checked(shares, 0).ok()
    }

    /// Closes whatever is held. Does nothing when flat.
    ///
    /// The `reason` is stamped on the closing order as a tag, and that is the
    /// only place it survives. A stop here is a market order the strategy
    /// sends when it sees the level breached, not a resting stop order the
    /// venue holds, so nothing about the order itself says why it was sent —
    /// which means a ledger reading order *types* would report every exit as
    /// a signal and never as a stop. It would have been wrong silently.
    ///
    /// Asks the engine what the position actually is rather than trusting the
    /// quantity this strategy asked for. The two diverge whenever an order is
    /// rejected or partially filled — insufficient funds, for one — and a
    /// strategy that sold its *intended* size would then either leave a
    /// remainder that outlives its stop or flip short without ever deciding
    /// to. Its own record is the fallback, for a venue that reports nothing.
    fn exit(&mut self, reason: &'static str) -> anyhow::Result<()> {
        self.stop = None;
        let intended = self.held.take();

        let instrument_id = self.instrument_id;
        let actual = self.portfolio().net_position(&instrument_id);
        let size = match f64::try_from(actual).ok().filter(|held| *held > 0.0) {
            Some(held) => Quantity::new_checked(held, 0).ok(),
            None => intended,
        };

        let Some(size) = size else {
            return Ok(());
        };
        self.enter_sized(OrderSide::Sell, size, Some(reason))
    }

    fn enter_sized(
        &mut self,
        side: OrderSide,
        trade_size: Quantity,
        reason: Option<&'static str>,
    ) -> anyhow::Result<()> {
        let instrument_id = self.instrument_id;
        let order = self.order().market(
            instrument_id,
            side,
            trade_size,
            None, // time_in_force
            None, // reduce_only
            None, // quote_quantity
            None, // exec_algorithm_id
            None, // exec_algorithm_params
            reason.map(|reason| vec![ustr::Ustr::from(reason)]),
            None, // client_order_id
        );
        self.submit_order(order, None, None, None)
    }
}

nautilus_strategy!(SmaCross);

impl Debug for SmaCross {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(SmaCross))
            .field("instrument_id", &self.instrument_id)
            .field("trade_size", &self.trade_size)
            .field("fast_period", &self.fast.period)
            .field("slow_period", &self.slow.period)
            .finish()
    }
}

impl DataActor for SmaCross {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self.atr.update(high, low, close);

        // The stop is checked before the signal, and on the bar's low rather
        // than its close. A strategy that only ever exits on a crossover is
        // not the strategy that was specified, and checking the close would
        // let a position that traded through the stop intraday survive to the
        // end of the bar.
        //
        // The exit is a market order, so it fills on the engine's terms rather
        // than at the stop price. That is deliberately the pessimistic
        // reading: a bar that gapped through the stop never offered the stop
        // price, and assuming it did is how a backtest flatters a strategy
        // exactly where it hurts most.
        if let Some(stop) = self.stop {
            if low <= stop {
                // Reset the crossover memory too: after a stop the next entry
                // should need a fresh signal, not the stale one that is still
                // technically in force.
                self.previous_fast_above = None;
                return self.exit(EXIT_STOP);
            }
        }

        let (Some(fast), Some(slow)) = (self.fast.update(close), self.slow.update(close)) else {
            return Ok(());
        };

        let fast_above = fast > slow;
        let previous = self.previous_fast_above.replace(fast_above);

        match previous {
            Some(false) if fast_above => {
                // A stop needs an ATR, and an ATR needs its warm-up. Entering
                // unprotected because the indicator is not ready yet would be
                // running a different strategy for the first few trades.
                let Some(distance) = self
                    .risk
                    .stop_atr_multiple
                    .and_then(|multiple| atr.map(|atr| atr * multiple))
                else {
                    return if self.risk.stop_atr_multiple.is_some() {
                        Ok(())
                    } else {
                        self.held = Some(self.trade_size);
                        self.enter_sized(OrderSide::Buy, self.trade_size, None)
                    };
                };

                let size = match self.risk.risk_amount {
                    None => Some(self.trade_size),
                    // No size that keeps the loss within budget means no
                    // trade. Taking it anyway would break the one rule the
                    // risk model exists to enforce.
                    Some(_) => self.sized(distance, close),
                };
                let Some(size) = size else {
                    return Ok(());
                };

                self.stop = Some(close - distance);
                self.held = Some(size);
                self.enter_sized(OrderSide::Buy, size, None)
            }
            Some(true) if !fast_above => self.exit(EXIT_SIGNAL),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_average_is_withheld_until_its_window_is_full() {
        let mut sma = Sma::new(3);
        assert_eq!(sma.update(1.0), None);
        assert_eq!(sma.update(2.0), None);
        assert_eq!(sma.update(3.0), Some(2.0));
    }

    #[test]
    fn true_range_counts_an_overnight_gap() {
        let mut atr = Atr::new(2);
        // First bar has no previous close, so its range is just high-low.
        assert_eq!(atr.update(10.0, 9.0, 10.0), None);
        // Second bar opens far below and never touches the old close: its own
        // span is 1.0, but the real move from 10.0 down to 6.0 is 4.0. A range
        // that ignored the gap would size every stop off the calm part of the
        // distribution.
        let value = atr.update(7.0, 6.0, 6.5).expect("two ranges held");
        assert!((value - (1.0 + 4.0) / 2.0).abs() < 1e-12, "{value}");
    }

    #[test]
    fn an_average_true_range_waits_for_its_window() {
        let mut atr = Atr::new(3);
        assert_eq!(atr.update(10.0, 9.0, 9.5), None);
        assert_eq!(atr.update(10.0, 9.0, 9.5), None);
        assert!(
            atr.update(10.0, 9.0, 9.5).is_some(),
            "three ranges is three"
        );
    }

    #[test]
    fn the_window_slides_rather_than_growing() {
        let mut sma = Sma::new(3);
        for value in [1.0, 2.0, 3.0] {
            let _ = sma.update(value);
        }
        assert_eq!(sma.update(6.0), Some(11.0 / 3.0), "oldest value drops out");
        assert_eq!(sma.window.len(), 3);
    }
}

/// Buys once on the first bar it sees and holds to the end of the run.
///
/// The benchmark every experiment is scored against. It pays the same
/// commission and runs through the same engine, so the difference between its
/// curve and a strategy's is the strategy — not the market, and not the fees.
pub(crate) struct BuyAndHold {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    trade_size: Quantity,
    entered: bool,
}

impl BuyAndHold {
    pub(crate) fn new(core: StrategyCore, bar_type: BarType, trade_size: Quantity) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            trade_size,
            entered: false,
        }
    }
}

nautilus_strategy!(BuyAndHold);

impl Debug for BuyAndHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(stringify!(BuyAndHold))
            .field("instrument_id", &self.instrument_id)
            .field("trade_size", &self.trade_size)
            .finish()
    }
}

impl DataActor for BuyAndHold {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, _bar: &Bar) -> anyhow::Result<()> {
        if self.entered {
            return Ok(());
        }
        self.entered = true;

        let instrument_id = self.instrument_id;
        let trade_size = self.trade_size;
        let order = self.order().market(
            instrument_id,
            OrderSide::Buy,
            trade_size,
            None, // time_in_force
            None, // reduce_only
            None, // quote_quantity
            None, // exec_algorithm_id
            None, // exec_algorithm_params
            None, // tags
            None, // client_order_id
        );
        self.submit_order(order, None, None, None)
    }
}
