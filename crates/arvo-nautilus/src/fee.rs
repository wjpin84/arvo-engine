//! What the venue charges, beyond a proportional commission.
//!
//! Nautilus's default is `MakerTakerFeeModel`: notional times the
//! instrument's fee rate. That is the whole of a crypto exchange's schedule
//! and about half of a US equity broker's. The other half does not scale with
//! notional — a flat ticket charge, a per-share regulatory fee, a charge on
//! sale proceeds only — and none of it can be expressed as a rate.
//!
//! So this model is the default plus the three shapes it cannot say. With all
//! three at zero it computes exactly what `MakerTakerFeeModel` computes, by
//! construction: the proportional part is the same expression, reading the
//! same instrument fee. That is deliberate, and it is why installing this
//! model is safe on a schedule that has nothing extra to charge.

use nautilus_execution::models::fee::FeeModel;
use nautilus_model::{
    enums::{LiquiditySide, OrderSide},
    instruments::{Instrument, InstrumentAny},
    orders::{Order, OrderAny},
    types::{Money, Price, Quantity},
};

/// A US-equity-shaped fee schedule.
#[derive(Debug, Clone, Copy)]
pub struct VenueFees {
    per_fill: f64,
    per_unit_sold: f64,
    sell_notional_rate: f64,
}

impl VenueFees {
    /// Builds the model from the experiment's recorded cost model.
    #[must_use]
    pub fn new(costs: &arvo_research::CostModel) -> Self {
        Self {
            per_fill: costs.per_fill,
            per_unit_sold: costs.per_unit_sold,
            sell_notional_rate: costs.sell_notional_bps / 10_000.0,
        }
    }
}

impl FeeModel for VenueFees {
    fn get_commission(
        &self,
        order: &OrderAny,
        fill_quantity: Quantity,
        fill_px: Price,
        instrument: &InstrumentAny,
    ) -> anyhow::Result<Money> {
        let notional = instrument.try_calculate_notional_value(fill_quantity, fill_px, Some(false))?;

        // The proportional part, identical to Nautilus's own model down to
        // reading the rate off the instrument rather than off this struct —
        // one source for the commission, whichever model is installed.
        let rate = match order.liquidity_side() {
            Some(LiquiditySide::Maker) => instrument.maker_fee(),
            Some(LiquiditySide::Taker) => instrument.taker_fee(),
            Some(LiquiditySide::NoLiquiditySide) | None => {
                anyhow::bail!("liquidity side not set on {}", order.client_order_id())
            }
        };
        let mut commission = notional.as_f64() * f64::try_from(rate)?;

        commission += self.per_fill;

        // Sell side only. The regulatory charges a US broker passes through
        // are levied on sales, and charging them on the buy would double a
        // round trip's regulatory cost.
        if order.order_side() == OrderSide::Sell {
            commission += self.per_unit_sold * fill_quantity.as_f64();
            commission += notional.as_f64() * self.sell_notional_rate;
        }

        Ok(Money::new(commission, notional.currency))
    }
}
