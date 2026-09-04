//! What you actually hold.
//!
//! The other half of the platform. Everything else here answers "does this
//! idea work?"; this answers "what do I own, what is it worth, and how is it
//! distributed?" — the long-term investing side rather than the research side.
//!
//! # Why this is Arvo's and not Nautilus's
//!
//! Nautilus has a `Portfolio`, and this is not a duplicate of it. Nautilus
//! tracks positions *inside a running engine* — what a strategy currently
//! holds during a backtest or a live session. This tracks what a person holds
//! at a broker, with cost basis, across accounts the engine has never heard
//! of. The engine cannot know about a brokerage account it does not trade,
//! so there is nothing here to delegate.
//!
//! # Read-only, and deliberately
//!
//! Nothing here places, cancels or modifies anything. A portfolio is
//! observed, not operated. Execution remains a separate, later, explicit
//! decision — see the architecture map.

pub mod csv;

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Cash is spelled this way in a holdings file, and is worth its face value.
///
/// A special case, but a small and documented one: the alternative is a
/// separate cash field that every reader, writer and sum has to remember
/// about, which is a bigger special case wearing a disguise.
pub const CASH: &str = "CASH";

/// One position: how much, and what it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holding {
    /// The same identifier the data library uses, so a holding and its price
    /// history are the same instrument rather than two things that look alike.
    pub instrument: String,
    pub quantity: f64,
    /// Total paid, not per share. Per-share cost is a division; total is what
    /// a statement actually reports, and deriving down is safer than up.
    pub cost_basis: f64,
    /// Price from the statement, when it carried one. `None` falls back to
    /// the last close in the data library, and failing that the holding is
    /// reported as unpriced rather than silently valued at zero.
    pub price: Option<f64>,
}

/// A set of holdings as of a date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Portfolio {
    pub name: String,
    pub as_of: NaiveDate,
    pub holdings: Vec<Holding>,
}

/// A holding with its value worked out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValuedHolding {
    pub instrument: String,
    pub quantity: f64,
    pub price: f64,
    pub market_value: f64,
    pub cost_basis: f64,
    pub unrealized: f64,
    /// `None` when cost basis is zero — a percentage gain on nothing is not a
    /// number, and rendering it as 0% or ∞ would both be lies.
    pub unrealized_pct: Option<f64>,
    /// Share of the portfolio's total value.
    pub weight: f64,
    /// Where the price came from, so a stale or missing price is visible
    /// rather than assumed.
    pub priced_by: PriceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// The holdings file carried a price.
    Statement,
    /// The last close in the data library.
    LastClose,
    /// Cash, at face value.
    Face,
}

/// A whole portfolio, valued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValuedPortfolio {
    pub name: String,
    pub as_of: NaiveDate,
    pub total_value: f64,
    pub total_cost: f64,
    pub unrealized: f64,
    pub unrealized_pct: Option<f64>,
    pub cash: f64,
    pub holdings: Vec<ValuedHolding>,
    /// Holdings no price could be found for. Listed rather than dropped: a
    /// total that quietly excludes a position is worse than one that says
    /// which position it could not include.
    pub unpriced: Vec<String>,
}

impl Portfolio {
    /// Values every holding, using statement prices where present and the
    /// supplied last-close prices otherwise.
    ///
    /// Holdings with neither are excluded from the totals and named in
    /// [`ValuedPortfolio::unpriced`]. Weights are computed after that
    /// exclusion, so they still sum to one and describe the priced portion —
    /// which is the only portion the numbers describe.
    #[must_use]
    pub fn value(&self, last_close: &BTreeMap<String, f64>) -> ValuedPortfolio {
        let mut priced: Vec<(ValuedHolding, ())> = Vec::new();
        let mut unpriced = Vec::new();
        let mut cash = 0.0;

        for holding in &self.holdings {
            let (price, source) = if holding.instrument.eq_ignore_ascii_case(CASH) {
                (Some(1.0), PriceSource::Face)
            } else if let Some(price) = holding.price {
                (Some(price), PriceSource::Statement)
            } else {
                (
                    last_close.get(&holding.instrument).copied(),
                    PriceSource::LastClose,
                )
            };

            let Some(price) = price else {
                unpriced.push(holding.instrument.clone());
                continue;
            };

            let market_value = holding.quantity * price;
            if source == PriceSource::Face {
                cash += market_value;
            }

            priced.push((
                ValuedHolding {
                    instrument: holding.instrument.clone(),
                    quantity: holding.quantity,
                    price,
                    market_value,
                    cost_basis: holding.cost_basis,
                    unrealized: market_value - holding.cost_basis,
                    unrealized_pct: (holding.cost_basis != 0.0)
                        .then(|| (market_value - holding.cost_basis) / holding.cost_basis),
                    weight: 0.0,
                    priced_by: source,
                },
                (),
            ));
        }

        let total_value: f64 = priced.iter().map(|(h, ())| h.market_value).sum();
        let total_cost: f64 = priced.iter().map(|(h, ())| h.cost_basis).sum();

        let mut holdings: Vec<ValuedHolding> = priced
            .into_iter()
            .map(|(mut holding, ())| {
                holding.weight = if total_value == 0.0 {
                    0.0
                } else {
                    holding.market_value / total_value
                };
                holding
            })
            .collect();

        // Largest first: an allocation view is read top-down, and the biggest
        // position is the one that matters most.
        holdings.sort_by(|a, b| b.market_value.total_cmp(&a.market_value));

        ValuedPortfolio {
            name: self.name.clone(),
            as_of: self.as_of,
            total_value,
            total_cost,
            unrealized: total_value - total_cost,
            unrealized_pct: (total_cost != 0.0).then(|| (total_value - total_cost) / total_cost),
            cash,
            holdings,
            unpriced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 4).expect("valid")
    }

    fn holding(instrument: &str, quantity: f64, cost: f64, price: Option<f64>) -> Holding {
        Holding {
            instrument: instrument.to_owned(),
            quantity,
            cost_basis: cost,
            price,
        }
    }

    fn portfolio(holdings: Vec<Holding>) -> Portfolio {
        Portfolio {
            name: "test".to_owned(),
            as_of: date(),
            holdings,
        }
    }

    #[test]
    fn a_statement_price_wins_over_the_data_library() {
        let closes = BTreeMap::from([("AAPL.NASDAQ".to_owned(), 100.0)]);
        let valued =
            portfolio(vec![holding("AAPL.NASDAQ", 10.0, 900.0, Some(150.0))]).value(&closes);

        assert_eq!(valued.holdings[0].priced_by, PriceSource::Statement);
        assert!((valued.total_value - 1500.0).abs() < 1e-9);
        assert!(
            (valued.unrealized - 600.0).abs() < 1e-9,
            "the statement is closer to the truth than a daily close"
        );
    }

    #[test]
    fn a_holding_with_no_price_anywhere_is_named_not_dropped() {
        let valued = portfolio(vec![
            holding("AAPL.NASDAQ", 10.0, 900.0, Some(150.0)),
            holding("OBSCURE.X", 5.0, 500.0, None),
        ])
        .value(&BTreeMap::new());

        assert_eq!(valued.holdings.len(), 1);
        assert_eq!(valued.unpriced, vec!["OBSCURE.X".to_owned()]);
        assert!(
            (valued.total_value - 1500.0).abs() < 1e-9,
            "the total covers what it could price, and says what it could not"
        );
    }

    #[test]
    fn weights_describe_the_priced_portion_and_sum_to_one() {
        let valued = portfolio(vec![
            holding("A.X", 10.0, 500.0, Some(100.0)),
            holding("B.X", 10.0, 500.0, Some(300.0)),
            holding("GONE.X", 1.0, 10.0, None),
        ])
        .value(&BTreeMap::new());

        let total: f64 = valued.holdings.iter().map(|h| h.weight).sum();
        assert!((total - 1.0).abs() < 1e-9, "{total}");
        assert_eq!(valued.holdings[0].instrument, "B.X", "largest first");
        assert!((valued.holdings[0].weight - 0.75).abs() < 1e-9);
    }

    #[test]
    fn cash_is_worth_its_face_value_and_is_reported_separately() {
        let valued = portfolio(vec![
            holding("A.X", 10.0, 500.0, Some(100.0)),
            holding(CASH, 250.0, 250.0, None),
        ])
        .value(&BTreeMap::new());

        assert!((valued.cash - 250.0).abs() < 1e-9);
        assert!((valued.total_value - 1250.0).abs() < 1e-9);
        assert!(
            valued.unpriced.is_empty(),
            "cash never needs a price looked up"
        );
    }

    #[test]
    fn a_gain_on_zero_cost_is_no_percentage_at_all() {
        let valued =
            portfolio(vec![holding("GIFT.X", 10.0, 0.0, Some(5.0))]).value(&BTreeMap::new());

        assert!((valued.holdings[0].unrealized - 50.0).abs() < 1e-9);
        assert_eq!(
            valued.holdings[0].unrealized_pct, None,
            "a percentage gain on nothing is not a number"
        );
        assert_eq!(valued.unrealized_pct, None);
    }

    #[test]
    fn an_empty_portfolio_values_to_nothing_without_dividing_by_zero() {
        let valued = portfolio(Vec::new()).value(&BTreeMap::new());
        assert!((valued.total_value - 0.0).abs() < f64::EPSILON);
        assert_eq!(valued.unrealized_pct, None);
        assert!(valued.holdings.is_empty());
    }
}
