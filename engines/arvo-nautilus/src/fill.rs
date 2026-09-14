//! Slippage, as a fill model.
//!
//! `CostModel::slippage_bps` says how much worse than the quoted price a fill
//! is assumed to land. Nautilus's own fill models cannot express that: every
//! one of them is either probabilistic or measured in whole ticks, and neither
//! scales with price the way a spread does. So this is the one place Arvo
//! supplies its own.
//!
//! The mechanism is a synthetic order book. Nautilus asks a fill model for a
//! book to match against, and a market order then simply takes the far side of
//! it. Widening that book by the recorded basis points is exactly the
//! assumption the record claims: a buy pays more, a sell receives less, and
//! the cost is charged at the fill price rather than smuggled in as a fee, so
//! stops and position values move the same way they would in life.

use nautilus_core::UnixNanos;
use nautilus_execution::models::fill::FillModel;
use nautilus_model::{
    data::order::BookOrder,
    enums::{BookType, OrderSide},
    instruments::{Instrument, InstrumentAny},
    orderbook::OrderBook,
    orders::OrderAny,
    types::{Price, Quantity},
};

/// The largest slippage that is a cost model rather than a typo.
///
/// 1000 bps is 10% a side. Anything beyond it is far likelier to be basis
/// points confused with percent than a real assumption about a real market.
pub const MAX_SLIPPAGE_BPS: f64 = 1_000.0;

/// Standing size at the slipped price, in units. Large enough that no order
/// Arvo can size walks past it, so slippage stays a function of the recorded
/// basis points and not of the order's size.
///
/// ponytail: a flat wall, so slippage does not grow with order size. Real
/// impact does. Upgrade to a depth curve when position sizes get large enough
/// relative to bar volume for that to matter — the bars already carry volume.
const DEPTH: u64 = 10_000_000_000;

/// A fill model that widens the book by a fixed number of basis points.
#[derive(Debug, Clone, Copy)]
pub struct BpsSlippage {
    /// The recorded basis points as a plain fraction.
    fraction: f64,
}

impl BpsSlippage {
    /// # Errors
    ///
    /// Returns the reason if `bps` is not a usable slippage: negative
    /// slippage would pay the trader to trade, and a value past
    /// [`MAX_SLIPPAGE_BPS`] is almost certainly a unit mix-up.
    pub fn new(bps: f64) -> Result<Self, String> {
        if !bps.is_finite() || bps < 0.0 {
            return Err(format!("slippage_bps must be zero or positive, got {bps}"));
        }
        if bps > MAX_SLIPPAGE_BPS {
            return Err(format!(
                "slippage_bps {bps} is past {MAX_SLIPPAGE_BPS}; that is {}% a side, \
                 which is more likely a percent/bps mix-up than a cost assumption",
                bps / 100.0
            ));
        }
        Ok(Self {
            fraction: bps / 10_000.0,
        })
    }

    /// Moves a price against whoever is trading, by at least one tick.
    ///
    /// Rounding is away from the trader rather than to nearest, and the
    /// minimum is a whole tick. On a coarse grid a small slippage would
    /// otherwise round to nothing and still report itself as applied — the
    /// exact failure this feature exists to remove. The cost of that choice is
    /// that slippage finer than one tick is charged as one tick, so on a cheap
    /// instrument the effective rate is higher than the record asks. Erring
    /// pessimistic is the side to err on.
    fn worsen(self, base: Price, tick: Price, precision: u8, up: bool) -> Option<Price> {
        let tick = tick.as_f64();
        let ticks = (base.as_f64() * self.fraction / tick).ceil().max(1.0);
        let moved = ticks * tick;
        let adjusted = if up {
            base.as_f64() + moved
        } else {
            // A bid below zero is not a price. It takes an absurd combination
            // to reach — 1000 bps cannot do it — but a clamp is cheaper than
            // reasoning about whether it can.
            (base.as_f64() - moved).max(tick)
        };
        Price::new_checked(adjusted, precision).ok()
    }
}

impl FillModel for BpsSlippage {
    fn is_limit_filled(&mut self) -> anyhow::Result<bool> {
        // The same answer as Nautilus's default model: a limit order the
        // market reached is filled. Slippage is a price assumption, not a
        // queue-position one.
        Ok(true)
    }

    fn is_slipped(&mut self) -> anyhow::Result<bool> {
        // No. Nautilus's one-tick slip would land on top of the widened book
        // below, charging the cost twice.
        Ok(false)
    }

    fn get_orderbook_for_fill_simulation(
        &mut self,
        instrument: &InstrumentAny,
        _order: &OrderAny,
        best_bid: Price,
        best_ask: Price,
    ) -> anyhow::Result<Option<OrderBook>> {
        let (tick, precision) = (instrument.price_increment(), instrument.price_precision());
        widened(
            instrument,
            self.worsen(best_bid, tick, precision, false),
            self.worsen(best_ask, tick, precision, true),
        )
    }
}

/// An option fill: half the spread off the premium, in dollars (#14).
///
/// The same synthetic book as [`BpsSlippage`], widened by
/// [`arvo_research::OptionSpread::half_spread`] rather than by basis points —
/// see that type for why a rate cannot describe an option's spread, and for
/// what the numbers were measured on.
#[derive(Debug, Clone, Copy)]
pub struct PremiumSpread {
    spread: arvo_research::OptionSpread,
}

impl PremiumSpread {
    #[must_use]
    pub const fn new(spread: arvo_research::OptionSpread) -> Self {
        Self { spread }
    }

    /// Moves a price against the trader by half the spread, rounded up to a
    /// whole tick and never less than one — the same pessimism as
    /// [`BpsSlippage::worsen`], for the same reason.
    fn worsen(self, base: Price, tick: Price, precision: u8, up: bool) -> Option<Price> {
        let tick = tick.as_f64();
        let ticks = (self.spread.half_spread(base.as_f64()) / tick - 1e-9)
            .ceil()
            .max(1.0);
        let moved = ticks * tick;
        let adjusted = if up {
            base.as_f64() + moved
        } else {
            // A contract bid at nothing is sold for a tick, not for a negative
            // price. Rounding up rather than to zero keeps the sale a sale.
            (base.as_f64() - moved).max(tick)
        };
        Price::new_checked(adjusted, precision).ok()
    }
}

impl FillModel for PremiumSpread {
    fn is_limit_filled(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }

    fn is_slipped(&mut self) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn get_orderbook_for_fill_simulation(
        &mut self,
        instrument: &InstrumentAny,
        _order: &OrderAny,
        best_bid: Price,
        best_ask: Price,
    ) -> anyhow::Result<Option<OrderBook>> {
        let (tick, precision) = (instrument.price_increment(), instrument.price_precision());
        widened(
            instrument,
            self.worsen(best_bid, tick, precision, false),
            self.worsen(best_ask, tick, precision, true),
        )
    }
}

/// A book standing at the worsened prices, deep enough that no order walks it.
fn widened(
    instrument: &InstrumentAny,
    bid: Option<Price>,
    ask: Option<Price>,
) -> anyhow::Result<Option<OrderBook>> {
    let (Some(bid), Some(ask)) = (bid, ask) else {
        // Unrepresentable at the instrument's precision. Returning `None`
        // would hand the fill back to Nautilus's own logic, which fills at
        // the unslipped price — silently free. Say so instead.
        anyhow::bail!("slipped price is not representable for {}", instrument.id());
    };
    let size = Quantity::from_mantissa_exponent(DEPTH, 0, instrument.size_precision());

    let mut book = OrderBook::new(instrument.id(), BookType::L2_MBP);
    book.add(
        BookOrder::new(OrderSide::Buy, bid, size, 1),
        0,
        0,
        UnixNanos::default(),
    );
    book.add(
        BookOrder::new(OrderSide::Sell, ask, size, 2),
        0,
        0,
        UnixNanos::default(),
    );
    Ok(Some(book))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(value: f64) -> Price {
        Price::new_checked(value, 2).expect("valid")
    }

    #[test]
    fn nonsense_slippage_is_refused_rather_than_applied() {
        for bps in [-1.0, f64::NAN, f64::INFINITY, 1_001.0] {
            assert!(BpsSlippage::new(bps).is_err(), "{bps} should be refused");
        }
    }

    #[test]
    fn a_buy_pays_more_and_a_sell_receives_less() {
        let model = BpsSlippage::new(10.0).expect("valid");
        let tick = price(0.01);
        // 10 bps of $100 is $0.10, exactly ten ticks each way.
        assert_eq!(
            model.worsen(price(100.0), tick, 2, true).expect("valid"),
            price(100.10)
        );
        assert_eq!(
            model.worsen(price(100.0), tick, 2, false).expect("valid"),
            price(99.90)
        );
    }

    #[test]
    fn slippage_scales_with_price_where_a_tick_model_would_not() {
        let model = BpsSlippage::new(10.0).expect("valid");
        let tick = price(0.01);
        assert_eq!(
            model.worsen(price(10.0), tick, 2, true).expect("valid"),
            price(10.01)
        );
        assert_eq!(
            model.worsen(price(1000.0), tick, 2, true).expect("valid"),
            price(1001.00)
        );
    }

    #[test]
    fn an_option_pays_a_floor_on_a_cheap_contract_and_a_fraction_on_a_dear_one() {
        let model = PremiumSpread::new(arvo_research::OptionSpread::MEASURED);
        let tick = price(0.01);
        // $0.10: the $0.025 floor, rounded up to three ticks — 30% of the
        // premium, where 1 bps of price would have charged one tick.
        assert_eq!(
            model.worsen(price(0.10), tick, 2, true).expect("valid"),
            price(0.13)
        );
        // $5.00: 2% is $0.10, exactly ten ticks.
        assert_eq!(
            model.worsen(price(5.00), tick, 2, true).expect("valid"),
            price(5.10)
        );
        assert_eq!(
            model.worsen(price(5.00), tick, 2, false).expect("valid"),
            price(4.90)
        );
    }

    #[test]
    fn a_contract_bid_at_nothing_still_sells_for_a_tick() {
        let model = PremiumSpread::new(arvo_research::OptionSpread::MEASURED);
        assert_eq!(
            model
                .worsen(price(0.01), price(0.01), 2, false)
                .expect("valid"),
            price(0.01)
        );
    }

    #[test]
    fn slippage_finer_than_a_tick_still_costs_a_tick() {
        // The failure this rounding exists to prevent: 0.01 bps of $10 is a
        // hundredth of a tick, and rounding to nearest would report slippage
        // as applied while charging nothing at all.
        let model = BpsSlippage::new(0.01).expect("valid");
        let tick = price(0.01);
        assert_eq!(
            model.worsen(price(10.0), tick, 2, true).expect("valid"),
            price(10.01)
        );
    }
}
