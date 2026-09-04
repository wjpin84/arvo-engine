//! Strategies Arvo can ask Nautilus to run.
//!
//! One so far, deliberately. The first slice tests whether the *loop* works —
//! hypothesis to experiment to evidence — and a strategy nobody disputes is
//! the right instrument for that. A moving-average crossover is not a good
//! trading idea; it is a good control.
//!
//! A strategy here is a Nautilus component, which is why it lives on this side
//! of the boundary. `arvo-research` names it by string in `StrategySpec` and
//! never sees the type.

use std::collections::VecDeque;
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

/// Buys when the fast average crosses above the slow one, sells when it
/// crosses back below.
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
}

impl SmaCross {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        fast_period: usize,
        slow_period: usize,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            trade_size,
            fast: Sma::new(fast_period),
            slow: Sma::new(slow_period),
            previous_fast_above: None,
        }
    }

    fn enter(&mut self, side: OrderSide) -> anyhow::Result<()> {
        let instrument_id = self.instrument_id;
        let trade_size = self.trade_size;
        let order = self.order().market(
            instrument_id,
            side,
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
        let close = bar.close.as_f64();
        let (Some(fast), Some(slow)) = (self.fast.update(close), self.slow.update(close)) else {
            return Ok(());
        };

        let fast_above = fast > slow;
        let previous = self.previous_fast_above.replace(fast_above);

        match previous {
            Some(false) if fast_above => self.enter(OrderSide::Buy),
            Some(true) if !fast_above => self.enter(OrderSide::Sell),
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
    fn the_window_slides_rather_than_growing() {
        let mut sma = Sma::new(3);
        for value in [1.0, 2.0, 3.0] {
            let _ = sma.update(value);
        }
        assert_eq!(sma.update(6.0), Some(11.0 / 3.0), "oldest value drops out");
        assert_eq!(sma.window.len(), 3);
    }
}
