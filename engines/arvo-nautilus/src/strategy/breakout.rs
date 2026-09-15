//! Buying a same-day SPY option on an opening-range break.
//!
//! The long counterpart to selling 0DTE spreads. The session's first bars set
//! a range on the underlying. A later close above it buys the same-day call
//! nearest a target delta; a close below buys the put. The option is sold at a
//! multiple of what it cost, or at a fraction lost, and otherwise settles at
//! the close — which for most same-day options bought near the money is
//! nothing.
//!
//! # What it reads, and when
//!
//! As the put spread rule: every decision is taken on one instant's prints, and
//! a contract that did not trade at that instant is neither bought nor sold on
//! it. The range is the underlying's high and low over the session's first
//! `range_bars` bars; a break is a close beyond it on a later bar, so the bar
//! that breaks is never part of the range it breaks.
//!
//! # The levels are the decision price's
//!
//! The target and stop are multiples of the option's price at the instant it
//! was chosen, not of what the fill cost after crossing half the spread. That
//! makes both a touch easier to reach than they would be against the fill; the
//! difference is half a spread, and the cost itself is charged on both fills.

use std::collections::{BTreeMap, HashMap};

use arvo_data::option::{OptionContract, Right};
use arvo_research::greeks::{greeks, implied_volatility, years_to_expiry, Market};
use chrono::{NaiveDate, NaiveDateTime};
use nautilus_common::actor::data_actor::DataActor;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarType},
    enums::OrderSide,
    identifiers::InstrumentId,
    types::Quantity,
};
use nautilus_trading::strategy::{Strategy, StrategyCore};

use super::{Risk, EXIT_SIGNAL, EXIT_STOP};

/// The rule's parameters, validated by the plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rule {
    /// Bars of the session that set the range.
    pub range_bars: usize,
    /// Target delta of the option bought, as a positive fraction.
    pub delta: f64,
    /// Sell once the option trades at this multiple of its price when chosen.
    pub target_multiple: f64,
    /// Sell once it has lost this fraction of that price.
    pub stop_fraction: f64,
    pub rate: f64,
    pub dividend_yield: f64,
}

/// Why nothing was bought on a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    NoPrices,
    EdgeOfChain,
}

/// The same-day option of `right` nearest the rule's delta, among those that
/// traded at `as_of`.
///
/// Pure, so the choice is tested without an engine.
pub(crate) fn choose(
    rule: Rule,
    right: Right,
    spot: f64,
    day: NaiveDate,
    as_of: NaiveDateTime,
    loaded: &[OptionContract],
    traded: &[(OptionContract, f64)],
) -> Result<(OptionContract, f64), Skip> {
    let today = |c: &OptionContract| c.right == right && c.expiration == day;
    let market = Market {
        spot,
        rate: rule.rate,
        dividend_yield: rule.dividend_yield,
    };
    let (contract, price, _) = traded
        .iter()
        .filter(|(c, _)| today(c))
        .filter_map(|(c, price)| {
            let years = years_to_expiry(c, as_of);
            let vol = implied_volatility(c, *price, market, years)?;
            Some((c, *price, greeks(c, market, years, vol).delta.abs()))
        })
        .min_by(|a, b| {
            (a.2 - rule.delta)
                .abs()
                .total_cmp(&(b.2 - rule.delta).abs())
                .then(a.0.strike.total_cmp(&b.0.strike))
        })
        .ok_or(Skip::NoPrices)?;
    let strikes = loaded.iter().filter(|c| today(c)).map(|c| c.strike);
    let (lowest, highest) = strikes.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), k| {
        (lo.min(k), hi.max(k))
    });
    // The contract wanted may lie beyond what was loaded; the nearest loaded
    // one is then a choice made by the fetch.
    if contract.strike <= lowest || contract.strike >= highest {
        return Err(Skip::EdgeOfChain);
    }
    Ok((contract.clone(), price))
}

/// The session being read.
#[derive(Debug, Clone, Copy)]
struct Session {
    day: NaiveDate,
    bars: usize,
    high: f64,
    low: f64,
    entered: bool,
}

/// An option held, and the price it was chosen at.
#[derive(Debug, Clone, Copy)]
struct Held {
    id: InstrumentId,
    chosen_at: f64,
}

pub(crate) struct ZeroDteBreakout {
    core: StrategyCore,
    underlying: BarType,
    bar_types: Vec<BarType>,
    contracts: BTreeMap<InstrumentId, OptionContract>,
    rule: Rule,
    risk: Risk,
    trade_size: Quantity,
    latest: HashMap<InstrumentId, (UnixNanos, f64)>,
    filling: Option<UnixNanos>,
    session: Option<Session>,
    held: Option<Held>,
    /// How often, and why, a break bought nothing.
    pub(crate) skipped: BTreeMap<&'static str, usize>,
}

impl ZeroDteBreakout {
    pub(crate) fn new(
        core: StrategyCore,
        underlying: BarType,
        bar_types: Vec<BarType>,
        rule: Rule,
        risk: Risk,
        trade_size: Quantity,
    ) -> Self {
        let contracts = bar_types
            .iter()
            .filter_map(|bar_type| {
                let id = bar_type.instrument_id();
                OptionContract::parse(&id.to_string()).map(|contract| (id, contract))
            })
            .collect();
        Self {
            core,
            underlying,
            bar_types,
            contracts,
            rule,
            risk,
            trade_size,
            latest: HashMap::new(),
            filling: None,
            session: None,
            held: None,
            skipped: BTreeMap::new(),
        }
    }

    fn printed(&self, id: &InstrumentId, at: UnixNanos) -> Option<f64> {
        self.latest
            .get(id)
            .filter(|(when, _)| *when == at)
            .map(|(_, price)| *price)
    }

    /// Everything the rule does at one completed instant.
    fn decide(&mut self, at: UnixNanos) -> anyhow::Result<()> {
        let Some(stamped) = super::nanos_to_instant(at) else {
            return Ok(());
        };
        let day = (stamped - chrono::Duration::seconds(1)).date();

        if let Some(held) = self.held {
            let position = f64::try_from(self.portfolio().net_position(&held.id)).unwrap_or(0.0);
            if position == 0.0 {
                // Sold, or settled at the close.
                self.held = None;
            } else if let Some(price) = self.printed(&held.id, at) {
                let reason = if price >= held.chosen_at * self.rule.target_multiple {
                    Some(EXIT_SIGNAL)
                } else if price <= held.chosen_at * (1.0 - self.rule.stop_fraction) {
                    Some(EXIT_STOP)
                } else {
                    None
                };
                if let (Some(reason), Ok(size)) = (reason, Quantity::new_checked(position, 0)) {
                    self.send(held.id, OrderSide::Sell, size, Some(reason))?;
                }
            }
            return Ok(());
        }

        let Some(session) = self.session.filter(|session| session.day == day) else {
            return Ok(());
        };
        if session.entered || session.bars <= self.rule.range_bars {
            return Ok(());
        }
        let Some(spot) = self.printed(&self.underlying.instrument_id(), at) else {
            return Ok(());
        };
        let right = if spot > session.high {
            Right::Call
        } else if spot < session.low {
            Right::Put
        } else {
            return Ok(());
        };

        let loaded: Vec<OptionContract> = self.contracts.values().cloned().collect();
        let traded: Vec<(OptionContract, f64)> = self
            .contracts
            .iter()
            .filter_map(|(id, contract)| self.printed(id, at).map(|price| (contract.clone(), price)))
            .collect();
        let (contract, price) = match choose(self.rule, right, spot, day, stamped, &loaded, &traded) {
            Ok(found) => found,
            Err(skip) => {
                let name = match skip {
                    Skip::NoPrices => "no prices",
                    Skip::EdgeOfChain => "edge of chain",
                };
                *self.skipped.entry(name).or_default() += 1;
                return Ok(());
            }
        };
        // One break a session, bought or not: a rule that bought the second
        // break after refusing the first is a different rule.
        if let Some(session) = self.session.as_mut() {
            session.entered = true;
        }

        let id = InstrumentId::new(contract.symbol().as_str().into(), self.underlying.instrument_id().venue);
        let (positions, realised_today, day_trades_used) =
            super::account_from_cache(&self.cache(), stamped.date());
        let decision = super::decide_entry(
            false,
            self.risk,
            self.trade_size,
            &id.to_string(),
            price,
            None,
            stamped,
            &positions,
            realised_today,
            false,
            self.risk.starting_cash,
            day_trades_used,
            super::spendable(&self.cache(), &id.venue),
            None,
        );
        let arvo_research::Decision::Accept { quantity } = decision else {
            *self.skipped.entry("refused").or_default() += 1;
            return Ok(());
        };
        let Ok(size) = Quantity::new_checked(quantity, 0) else {
            return Ok(());
        };
        self.send(id, OrderSide::Buy, size, None)?;
        self.held = Some(Held {
            id,
            chosen_at: price,
        });
        Ok(())
    }

    /// Folds one underlying bar into the session's range.
    fn observe_underlying(&mut self, bar: &Bar) {
        let Some(stamped) = super::nanos_to_instant(bar.ts_event) else {
            return;
        };
        let day = (stamped - chrono::Duration::seconds(1)).date();
        let (high, low) = (bar.high.as_f64(), bar.low.as_f64());
        let session = match self.session {
            Some(session) if session.day == day => session,
            _ => Session {
                day,
                bars: 0,
                high: f64::NEG_INFINITY,
                low: f64::INFINITY,
                entered: false,
            },
        };
        let bars = session.bars + 1;
        let in_range = bars <= self.rule.range_bars;
        self.session = Some(Session {
            bars,
            high: if in_range { session.high.max(high) } else { session.high },
            low: if in_range { session.low.min(low) } else { session.low },
            ..session
        });
    }

    fn send(
        &mut self,
        instrument: InstrumentId,
        side: OrderSide,
        size: Quantity,
        reason: Option<&'static str>,
    ) -> anyhow::Result<()> {
        let order = self.order().market(
            instrument,
            side,
            size,
            None,
            None,
            None,
            None,
            None,
            reason.map(|reason| vec![ustr::Ustr::from(reason)]),
            None,
        );
        self.submit_order(order, None, None, None)
    }
}

nautilus_trading::nautilus_strategy!(ZeroDteBreakout);

impl std::fmt::Debug for ZeroDteBreakout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZeroDteBreakout")
            .field("contracts", &self.contracts.len())
            .field("rule", &self.rule)
            .finish_non_exhaustive()
    }
}

impl DataActor for ZeroDteBreakout {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.underlying, None, None);
        for bar_type in self.bar_types.clone() {
            self.subscribe_bars(bar_type, None, None);
        }
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.underlying, None, None);
        for bar_type in self.bar_types.clone() {
            self.unsubscribe_bars(bar_type, None, None);
        }
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        if let Some(filling) = self.filling.filter(|filling| *filling != bar.ts_event) {
            self.decide(filling)?;
        }
        self.filling = Some(bar.ts_event);
        let id = bar.bar_type.instrument_id();
        if id == self.underlying.instrument_id() {
            self.observe_underlying(bar);
        }
        self.latest.insert(id, (bar.ts_event, bar.close.as_f64()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            range_bars: 6,
            delta: 0.5,
            target_multiple: 2.0,
            stop_fraction: 0.5,
            rate: 0.04,
            dividend_yield: 0.013,
        }
    }

    fn chain(right: Right, day: NaiveDate) -> Vec<(OptionContract, f64)> {
        let market = Market { spot: 600.0, rate: 0.04, dividend_yield: 0.013 };
        let as_of = day.and_hms_opt(15, 0, 0).expect("valid");
        (590..=610)
            .map(|strike| {
                let contract = OptionContract {
                    underlying: "SPY".to_owned(),
                    expiration: day,
                    right,
                    strike: f64::from(strike),
                };
                let price = greeks(&contract, market, years_to_expiry(&contract, as_of), 0.2).price;
                (contract, price.max(0.01))
            })
            .collect()
    }

    #[test]
    fn buys_the_side_that_broke_at_the_delta_asked() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 7).expect("valid");
        let as_of = day.and_hms_opt(15, 0, 0).expect("valid");
        for right in [Right::Call, Right::Put] {
            let traded = chain(right, day);
            let loaded: Vec<_> = traded.iter().map(|(c, _)| c.clone()).collect();
            let (contract, _) = choose(rule(), right, 600.0, day, as_of, &loaded, &traded).expect("an option");
            assert_eq!(contract.right, right);
            assert!((contract.strike - 600.0).abs() <= 1.0, "half a delta is at the money: {}", contract.strike);
        }
    }

    #[test]
    fn a_contract_wanted_beyond_the_loaded_strikes_is_not_replaced() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 7).expect("valid");
        let as_of = day.and_hms_opt(15, 0, 0).expect("valid");
        let traded: Vec<_> = chain(Right::Call, day).into_iter().filter(|(c, _)| c.strike >= 605.0).collect();
        let loaded: Vec<_> = traded.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(choose(rule(), Right::Call, 600.0, day, as_of, &loaded, &traded), Err(Skip::EdgeOfChain));
        let tomorrow = day.succ_opt().expect("valid");
        assert_eq!(choose(rule(), Right::Call, 600.0, tomorrow, as_of, &loaded, &traded), Err(Skip::NoPrices));
    }
}
