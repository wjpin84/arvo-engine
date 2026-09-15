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

pub mod bands;
pub mod csv;
pub mod history;

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
    /// Units held. `None` when the source reports only money — a collective
    /// investment trust inside a 401(k) commonly does, giving a balance and
    /// no unit count at all. A holding is "some amount of something worth
    /// some money", and different sources report different parts of that.
    pub quantity: Option<f64>,
    /// Total paid, not per share. Per-share cost is a division; total is what
    /// a statement reports, and deriving down is safer than up.
    ///
    /// `None` when the source does not report one, which is normal for a
    /// tax-deferred account: a 401(k) export often omits cost basis entirely
    /// because no capital gain is ever realised on it. Treating that absence
    /// as zero would report the entire balance as an unrealised gain, which
    /// is a lie rather than an approximation.
    pub cost_basis: Option<f64>,
    /// Price from the statement, when it carried one. `None` falls back to
    /// the last close in the data library, and failing that the holding is
    /// reported as unpriced rather than silently valued at zero.
    pub price: Option<f64>,
    /// Market value as the source reported it, when it did.
    ///
    /// Preferred over multiplying quantity by price: it is what the statement
    /// actually says, and it is the only thing available when there is no
    /// unit count to multiply.
    pub value: Option<f64>,
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
    /// `None` when the source reported money without units.
    pub quantity: Option<f64>,
    /// `None` when there is a value but no unit count to divide it by.
    pub price: Option<f64>,
    pub market_value: f64,
    /// `None` when the source did not report one.
    pub cost_basis: Option<f64>,
    /// `None` when there is no cost basis to compare against.
    pub unrealized: Option<f64>,
    /// `None` when cost basis is absent or zero — a percentage gain on
    /// nothing is not a number, and 0% or ∞ would both be lies.
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
    /// `None` unless *every* priced holding reported a cost basis.
    ///
    /// Summing the ones that did and ignoring the rest would produce a total
    /// that looks complete and understates cost by however much was missing —
    /// and nothing about the number would show it. Better to have no total
    /// than a quietly wrong one; [`Self::without_cost_basis`] says how many
    /// were responsible.
    pub total_cost: Option<f64>,
    pub unrealized: Option<f64>,
    pub unrealized_pct: Option<f64>,
    /// How many priced holdings reported no cost basis.
    pub without_cost_basis: usize,
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
            let is_cash = holding.instrument.eq_ignore_ascii_case(CASH);

            // In order of authority: what the statement said the position was
            // worth, then what it said a unit was worth, then our own last
            // close. A reported value beats a computed one — it is the number
            // the custodian stands behind.
            let (market_value, source) = if let Some(value) = holding.value {
                (
                    Some(value),
                    if is_cash {
                        PriceSource::Face
                    } else {
                        PriceSource::Statement
                    },
                )
            } else if is_cash {
                (holding.quantity, PriceSource::Face)
            } else if let (Some(quantity), Some(price)) = (holding.quantity, holding.price) {
                (Some(quantity * price), PriceSource::Statement)
            } else if let (Some(quantity), Some(close)) = (
                holding.quantity,
                last_close.get(&holding.instrument).copied(),
            ) {
                (Some(quantity * close), PriceSource::LastClose)
            } else {
                (None, PriceSource::LastClose)
            };

            let Some(market_value) = market_value else {
                unpriced.push(holding.instrument.clone());
                continue;
            };

            if source == PriceSource::Face {
                cash += market_value;
            }

            // Only derivable when there are units to divide by.
            let price = holding.price.or_else(|| {
                holding
                    .quantity
                    .filter(|quantity| *quantity != 0.0)
                    .map(|quantity| market_value / quantity)
            });

            priced.push((
                ValuedHolding {
                    instrument: holding.instrument.clone(),
                    quantity: holding.quantity,
                    price,
                    market_value,
                    cost_basis: holding.cost_basis,
                    unrealized: holding.cost_basis.map(|cost| market_value - cost),
                    unrealized_pct: holding
                        .cost_basis
                        .filter(|cost| *cost != 0.0)
                        .map(|cost| (market_value - cost) / cost),
                    weight: 0.0,
                    priced_by: source,
                },
                (),
            ));
        }

        let total_value: f64 = priced.iter().map(|(h, ())| h.market_value).sum();
        let without_cost_basis = priced
            .iter()
            .filter(|(h, ())| h.cost_basis.is_none())
            .count();
        // All or nothing, deliberately — see the field docs.
        let total_cost: Option<f64> = (without_cost_basis == 0)
            .then(|| priced.iter().filter_map(|(h, ())| h.cost_basis).sum());

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
            unrealized: total_cost.map(|cost| total_value - cost),
            unrealized_pct: total_cost
                .filter(|cost| *cost != 0.0)
                .map(|cost| (total_value - cost) / cost),
            without_cost_basis,
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
            quantity: Some(quantity),
            cost_basis: Some(cost),
            price,
            value: None,
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
            (valued.unrealized.expect("cost was reported") - 600.0).abs() < 1e-9,
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

        assert!((valued.holdings[0].unrealized.expect("cost known") - 50.0).abs() < 1e-9);
        assert_eq!(
            valued.holdings[0].unrealized_pct, None,
            "a percentage gain on nothing is not a number"
        );
        assert_eq!(valued.unrealized_pct, None);
    }

    #[test]
    fn one_holding_with_no_cost_basis_withholds_the_whole_total() {
        // The 401(k) case: a tax-deferred account often reports no cost basis
        // at all. Summing the holdings that do report one would give a total
        // that looks complete and understates cost, with nothing to show it.
        let valued = portfolio(vec![
            holding("A.X", 10.0, 500.0, Some(100.0)),
            Holding {
                instrument: "B.X".to_owned(),
                quantity: Some(10.0),
                cost_basis: None,
                price: Some(50.0),
                value: None,
            },
        ])
        .value(&BTreeMap::new());

        assert!(
            (valued.total_value - 1500.0).abs() < 1e-9,
            "value is still known"
        );
        assert_eq!(valued.total_cost, None, "cost is not");
        assert_eq!(valued.unrealized, None);
        assert_eq!(valued.without_cost_basis, 1, "and it says how many");
    }

    #[test]
    fn a_balance_with_no_unit_count_still_values() {
        // The real 401(k) case: a collective investment trust reports a
        // dollar balance and a cost basis, and no share count anywhere.
        // Requiring units would have refused the entire account.
        let valued = portfolio(vec![Holding {
            instrument: "JPMCB SRPB 2050 CFX1".to_owned(),
            quantity: None,
            cost_basis: Some(154_350.78),
            price: None,
            value: Some(224_630.21),
        }])
        .value(&BTreeMap::new());

        assert!((valued.total_value - 224_630.21).abs() < 1e-9);
        assert!((valued.unrealized.expect("cost reported") - 70_279.43).abs() < 1e-6);
        assert_eq!(valued.holdings[0].quantity, None);
        assert_eq!(
            valued.holdings[0].price, None,
            "no units to divide by, so there is no unit price to report"
        );
        assert!(valued.unpriced.is_empty());
    }

    #[test]
    fn a_reported_value_beats_a_computed_one() {
        // The custodian's own number wins: it accounts for accruals and
        // fractional units we cannot see.
        let valued = portfolio(vec![Holding {
            instrument: "A.X".to_owned(),
            quantity: Some(10.0),
            cost_basis: Some(500.0),
            price: Some(100.0),
            value: Some(1_050.0),
        }])
        .value(&BTreeMap::new());

        assert!((valued.total_value - 1_050.0).abs() < 1e-9, "not 10 x 100");
    }

    #[test]
    fn an_empty_portfolio_values_to_nothing_without_dividing_by_zero() {
        let valued = portfolio(Vec::new()).value(&BTreeMap::new());
        assert!((valued.total_value - 0.0).abs() < f64::EPSILON);
        assert_eq!(valued.unrealized_pct, None);
        assert!(valued.holdings.is_empty());
        assert_eq!(
            valued.total_cost,
            Some(0.0),
            "nothing missing a cost basis, so the total is known and zero"
        );
    }
}
