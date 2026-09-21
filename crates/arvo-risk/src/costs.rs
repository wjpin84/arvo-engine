//! What a fill is assumed to cost.

use serde::{Deserialize, Serialize};

/// What a fill is assumed to cost.
///
/// Pinned into the experiment because it changes results more than most
/// strategy parameters do, and because an unstated cost assumption is the
/// most common way a backtest flatters itself.
///
/// Two proportional rates were the whole model at first, which quietly
/// assumed every cost scales with notional. Real US equity schedules do not:
/// a flat ticket charge falls hardest on small positions, per-share fees
/// scale with size rather than value, and the regulatory charges fall on
/// *sells* only. Each field below is a different shape for that reason, and
/// every one of them defaults to zero, so a schedule that does not have a
/// charge simply does not state it.
///
/// Taxes are deliberately absent — see [`crate::trade`] for why they are not
/// a per-fill cost.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    /// Proportional commission, charged on every fill, both sides.
    pub commission_bps: f64,
    /// How much worse than the quoted price a fill is assumed to land.
    pub slippage_bps: f64,
    /// Flat charge per fill, in account currency.
    ///
    /// The one cost that does not scale at all, so it is the one that decides
    /// whether a strategy trading small size often is viable. Zero at a
    /// commission-free US equity broker.
    #[serde(default)]
    pub per_fill: f64,
    /// Charged per unit sold — the shape of FINRA's Trading Activity Fee.
    ///
    /// Sell side only, and per *share* rather than per dollar, so it bites
    /// hardest on cheap instruments where a share is worth little.
    #[serde(default)]
    pub per_unit_sold: f64,
    /// Basis points of sale proceeds — the shape of the SEC Section 31 fee.
    ///
    /// Sell side only. The rate is reset by the SEC periodically and is not a
    /// constant worth hardcoding anywhere; it belongs in whatever states the
    /// broker schedule, checked against a current one.
    #[serde(default)]
    pub sell_notional_bps: f64,
    /// What an option fill pays in spread, instead of [`Self::slippage_bps`].
    ///
    /// `None` for anything that is not an option, and skipped when absent so
    /// every record written before options existed serialises byte for byte
    /// as it did. An option run without one is refused, not run free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_spread: Option<OptionSpread>,
}

/// Half the bid-ask spread an option fill crosses (#14).
///
/// # Why not basis points
///
/// Slippage in basis points of price is an equity model: a spread there is a
/// few cents on a hundred-dollar share. An option's spread is a floor of a
/// cent or two on a premium that may be a dime, so it is a *large* fraction of
/// a cheap contract and a small one of an expensive contract — and no single
/// rate says both. So: a dollar floor, or a fraction of the premium, whichever
/// is more.
///
/// # What the defaults were measured on
///
/// [`Self::MEASURED`] is the 90th percentile of SPY half-spreads recorded on
/// 2026-09-14, 18:27-19:57 UTC — seven snapshots, 24k quotes, on Alpaca's
/// *indicative* feed, on a calm afternoon. By premium:
///
/// | premium | p50 half-spread | p90 | model |
/// |---|---|---|---|
/// | under $0.10 | $0.005 | $0.015-0.025 | $0.025 |
/// | $0.10-1 | $0.005 | $0.025 | $0.025 |
/// | $1-3 | $0.010 | $0.035 | $0.025-0.06 |
/// | $3-10 | $0.02-0.035 | $0.07-0.12 | $0.06-0.20 |
/// | $10+, 1-30 days | $0.27-1.70 | $1.7-2.2 | **$0.20-0.60+** |
///
/// Pessimistic up to $10, which is where a 0DTE or a sold out-of-the-money
/// contract trades. **It underprices deep in-the-money contracts** over $10
/// with more than a day left, whose indicative spreads were several percent;
/// a strategy that trades those must raise the fraction. And one calm
/// afternoon says nothing about the open, the close or a stressed market,
/// when spreads widen most — recalibrate as the recorder (#83) accumulates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OptionSpread {
    /// The smallest half-spread charged, in dollars per share.
    pub min_half_spread: f64,
    /// Half-spread as a fraction of the premium.
    pub half_spread_fraction: f64,
}

impl OptionSpread {
    /// The 2026-09-14 calibration. See the type's docs for what it misses.
    pub const MEASURED: Self = Self {
        min_half_spread: 0.025,
        half_spread_fraction: 0.02,
    };

    /// Half the spread, in dollars per share, on a contract trading at
    /// `premium`.
    #[must_use]
    pub fn half_spread(&self, premium: f64) -> f64 {
        self.min_half_spread
            .max(self.half_spread_fraction * premium.abs())
    }
}

/// Three named cost models, as multiples of the one the experiment states
/// (#192).
///
/// The experiment's own model is the *realistic* one: what a fill is
/// expected to cost at the venue it was studied for. A finding is read
/// under that model and then asked whether it survives the conservative one,
/// because a result that only holds when fills are cheap is a result about
/// the cost assumption, not about the rule. The optimistic tier exists so
/// the question "how much of this is costs" has a floor to be asked against;
/// nothing is ever judged under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostTier {
    /// Half the commission, no slippage, no fees: a floor, never a judgement.
    Optimistic,
    /// The model as stated.
    Realistic,
    /// Half again the commission, twice the slippage and never under five
    /// basis points, twice the fees, twice the option spread's floor.
    Conservative,
}

impl CostModel {
    /// This model at a [`CostTier`]. `Realistic` is the model itself.
    #[must_use]
    pub fn at(self, tier: CostTier) -> Self {
        match tier {
            CostTier::Realistic => self,
            CostTier::Optimistic => Self {
                commission_bps: self.commission_bps * 0.5,
                slippage_bps: 0.0,
                per_fill: 0.0,
                per_unit_sold: 0.0,
                sell_notional_bps: 0.0,
                option_spread: self.option_spread.map(|spread| OptionSpread {
                    min_half_spread: spread.min_half_spread * 0.5,
                    half_spread_fraction: spread.half_spread_fraction * 0.5,
                }),
            },
            CostTier::Conservative => Self {
                commission_bps: self.commission_bps * 1.5,
                slippage_bps: (self.slippage_bps * 2.0).max(5.0),
                per_fill: self.per_fill * 2.0,
                per_unit_sold: self.per_unit_sold * 2.0,
                sell_notional_bps: self.sell_notional_bps * 2.0,
                option_spread: self.option_spread.map(|spread| OptionSpread {
                    min_half_spread: spread.min_half_spread * 2.0,
                    half_spread_fraction: spread.half_spread_fraction * 2.0,
                }),
            },
        }
    }

    /// Only the two proportional costs — what the model was before venue and
    /// regulatory fees existed.
    ///
    /// Kept because most callers genuinely have nothing else to say, and
    /// spelling three zeroes at every construction site invites one of them
    /// being wrong.
    #[must_use]
    pub const fn proportional(commission_bps: f64, slippage_bps: f64) -> Self {
        Self {
            commission_bps,
            slippage_bps,
            per_fill: 0.0,
            per_unit_sold: 0.0,
            sell_notional_bps: 0.0,
            option_spread: None,
        }
    }

    /// Whether anything beyond the proportional rates is charged.
    ///
    /// Used to decide whether the engine needs Arvo's own fee model at all:
    /// with nothing extra to charge, the venue default already computes the
    /// same number.
    #[must_use]
    pub fn has_venue_fees(&self) -> bool {
        self.per_fill != 0.0 || self.per_unit_sold != 0.0 || self.sell_notional_bps != 0.0
    }

    /// Rejects a schedule that cannot mean anything.
    ///
    /// # Errors
    ///
    /// Returns a reason if any rate is negative or not finite. A negative fee
    /// is a rebate, and a backtest that pays the trader to trade is the single
    /// most flattering bug available.
    pub fn check(&self) -> Result<(), String> {
        for (name, value) in [
            ("commission_bps", self.commission_bps),
            ("slippage_bps", self.slippage_bps),
            ("per_fill", self.per_fill),
            ("per_unit_sold", self.per_unit_sold),
            ("sell_notional_bps", self.sell_notional_bps),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{name} must be zero or positive, got {value}"));
            }
        }
        if let Some(spread) = self.option_spread {
            for (name, value) in [
                ("option_spread.min_half_spread", spread.min_half_spread),
                (
                    "option_spread.half_spread_fraction",
                    spread.half_spread_fraction,
                ),
            ] {
                if !value.is_finite() || value < 0.0 {
                    return Err(format!("{name} must be zero or positive, got {value}"));
                }
            }
            // Half the spread larger than the premium: the bid would be below
            // zero. That is a percent typed as a fraction, not a market.
            if spread.half_spread_fraction >= 1.0 {
                return Err(format!(
                    "option_spread.half_spread_fraction {} is a fraction of the premium; \
                     {} would put the bid below zero",
                    spread.half_spread_fraction, spread.half_spread_fraction
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_option_spread_is_a_floor_or_a_fraction_of_the_premium() {
        let spread = OptionSpread::MEASURED;
        assert!(
            (spread.half_spread(0.10) - 0.025).abs() < 1e-12,
            "a dime pays the floor"
        );
        assert!((spread.half_spread(5.0) - 0.10).abs() < 1e-12, "$5 pays 2%");
    }

    #[test]
    fn a_nonsense_option_spread_is_refused() {
        let with = |min_half_spread, half_spread_fraction| CostModel {
            option_spread: Some(OptionSpread {
                min_half_spread,
                half_spread_fraction,
            }),
            ..CostModel::proportional(0.0, 0.0)
        };
        assert!(with(0.025, 0.02).check().is_ok());
        for bad in [with(-0.01, 0.02), with(0.025, f64::NAN), with(0.025, 2.0)] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_cost_model_without_an_option_spread_serialises_as_it_always_did() {
        // Stored records and shared experiments carry their cost model; adding a
        // field must not change what an old one looks like.
        let text = serde_json::to_string(&CostModel::proportional(1.0, 2.0)).expect("serialises");
        assert!(!text.contains("option_spread"), "{text}");
    }
}
