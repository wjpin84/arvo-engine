//! Selling SPY put spreads (#86), a month out or the same day (#87).
//!
//! The first rule that does not trade the series it reads. It watches the
//! underlying's daily closes and trades contracts it chooses from the chain:
//! when flat, it sells the put nearest a target delta on the expiration nearest
//! a target number of days, and buys the put a fixed width below it. It closes
//! the spread when it has kept enough of the credit or has too few days left,
//! and otherwise holds it into settlement.
//!
//! # What it reads, and when
//!
//! Every price it acts on is a trade *at the instant it decides*. A contract
//! that did not trade that day has no price that day, so it is neither chosen
//! nor closed on it — a spread whose leg did not print waits for a day when both
//! did. Deciding on yesterday's print for a contract that went quiet is how a
//! backtest sells premium that was never bid.
//!
//! The volatility behind each delta is implied from that same trade, against the
//! underlying's close, at a rate and dividend yield the experiment states. Puts
//! only are loaded, so the yield cannot be implied from parity (see
//! `arvo_research::greeks`); it is a parameter, and a wrong one moves a month's
//! delta by a little and a strike choice by a strike at most.
//!
//! # What makes it skip
//!
//! No expiration near the target, no prices, no leg a width below, no credit,
//! or a short strike at the edge of the chain that was loaded. The last one is
//! the chain being too narrow to trust: the strike wanted may lie beyond it, and
//! choosing the nearest one loaded would be choosing by what was fetched.
//!
//! # Sizing and legs
//!
//! In whole spreads, through the same gate as every rule, long leg first: the
//! short put is secured by the long one only once the long one is held
//! (ADR-0017). The engine fills an order after the bar that sent it, so the
//! short is sent when the long's fill arrives — asked any earlier, the gate sees
//! no long and wants the whole strike. How many spreads the account can
//! complete is worked out before either leg is sent, and if the short is
//! refused anyway the long is sold straight back, so no half a spread is held.

use std::collections::{BTreeMap, HashMap};

use arvo_data::option::{OptionContract, Right};

/// Units in one contract of `id`, as the instrument says (#186).
fn lot(id: &InstrumentId) -> f64 {
    arvo_data::Instrument::of(&id.to_string()).lot
}
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

use super::{Risk, EXIT_SIGNAL};

/// How many days either side of the target an expiration may be.
///
/// ponytail: fixed. Make it a parameter if weekly-only chains leave gaps wider
/// than two weeks.
const DTE_TOLERANCE: i64 = 7;

/// The rule's parameters, validated by the plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rule {
    /// Target calendar days to expiration when opening.
    pub dte: i64,
    /// Target delta of the short put, as a positive fraction.
    pub short_delta: f64,
    /// Strike distance from the short put down to the long one, in dollars.
    pub width: f64,
    /// Close once this fraction of the opening credit has been kept. At 1.0,
    /// never: all of it is kept only once the spread is worthless, and buying
    /// back a worthless spread pays two fills' spread for nothing settlement
    /// would not give free — found when a same-day rule closed 113 of 135
    /// spreads that way.
    pub take_profit: f64,
    /// Close at or below this many days to expiration. `None` holds into
    /// settlement unless a target or stop closes it first.
    pub exit_dte: Option<i64>,
    /// Close once the spread costs this multiple of its credit to buy back —
    /// 2.0 closes it at a loss the size of the credit. `None` has no stop.
    pub stop_multiple: Option<f64>,
    /// Intraday: open only in the half hour from this many minutes after the
    /// session opens, once a session. `None` decides on daily closes.
    pub entry_minutes: Option<i64>,
    pub rate: f64,
    pub dividend_yield: f64,
}

/// How long after `entry_minutes` an intraday entry may still be taken.
///
/// A fixed-time rule whose contracts did not trade at the stated bar would
/// otherwise drift to whatever time they next did, and the result would
/// describe entries at noon under a 10:00 name.
///
/// ponytail: fixed. Make it a parameter if thin chains need a wider window.
const ENTRY_WINDOW_MINUTES: i64 = 30;

/// A spread to open.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Choice {
    pub short: OptionContract,
    pub long: OptionContract,
    /// Per share: the short's price less the long's.
    pub credit: f64,
}

/// Why nothing was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    NoExpiration,
    NoPrices,
    EdgeOfChain,
    NoLongLeg,
    NoCredit,
}

/// The spread the rule would open on `day`, given the underlying at `spot`,
/// every put `loaded` for the run, and the ones that traded at `as_of` with
/// their prices.
///
/// Pure, so the choice — the part of this rule worth testing — is tested
/// without an engine.
pub(crate) fn choose(
    rule: Rule,
    spot: f64,
    day: NaiveDate,
    as_of: NaiveDateTime,
    loaded: &[OptionContract],
    traded: &[(OptionContract, f64)],
) -> Result<Choice, Skip> {
    let dte = |contract: &OptionContract| contract.days_to_expiry(day);
    // Same day means the same day: a 0DTE rule that took tomorrow's contract
    // when today's did not trade would be a different rule.
    let tolerance = if rule.dte == 0 { 0 } else { DTE_TOLERANCE };
    let expiration = loaded
        .iter()
        .filter(|c| c.right == Right::Put && dte(c) >= 0)
        .filter(|c| rule.exit_dte.is_none_or(|exit| dte(c) > exit))
        .filter(|c| (dte(c) - rule.dte).abs() <= tolerance)
        .map(|c| (c.expiration, (dte(c) - rule.dte).abs()))
        .min_by_key(|(expiration, distance)| (*distance, *expiration))
        .map(|(expiration, _)| expiration)
        .ok_or(Skip::NoExpiration)?;

    let on_expiry = |c: &&OptionContract| c.right == Right::Put && c.expiration == expiration;
    let lowest_loaded = loaded
        .iter()
        .filter(on_expiry)
        .map(|c| c.strike)
        .fold(f64::INFINITY, f64::min);
    let highest_loaded = loaded
        .iter()
        .filter(on_expiry)
        .map(|c| c.strike)
        .fold(f64::NEG_INFINITY, f64::max);

    let market = Market {
        spot,
        rate: rule.rate,
        dividend_yield: rule.dividend_yield,
    };
    let priced: Vec<(&OptionContract, f64, f64)> = traded
        .iter()
        .filter(|(c, _)| c.right == Right::Put && c.expiration == expiration)
        .filter_map(|(c, price)| {
            let years = years_to_expiry(c, as_of);
            let vol = implied_volatility(c, *price, market, years)?;
            Some((c, *price, -greeks(c, market, years, vol).delta))
        })
        .collect();
    let (short, short_price, _) = priced
        .iter()
        .min_by(|a, b| {
            (a.2 - rule.short_delta)
                .abs()
                .total_cmp(&(b.2 - rule.short_delta).abs())
                .then(a.0.strike.total_cmp(&b.0.strike))
        })
        .copied()
        .ok_or(Skip::NoPrices)?;
    if short.strike <= lowest_loaded || short.strike >= highest_loaded {
        return Err(Skip::EdgeOfChain);
    }

    let floor = short.strike - rule.width;
    if floor < lowest_loaded {
        return Err(Skip::EdgeOfChain);
    }
    let (long, long_price) = traded
        .iter()
        .filter(|(c, _)| c.right == Right::Put && c.expiration == expiration)
        .filter(|(c, _)| c.strike <= floor + 1e-9)
        .max_by(|a, b| a.0.strike.total_cmp(&b.0.strike))
        .map(|(c, price)| (c, *price))
        .ok_or(Skip::NoLongLeg)?;

    let credit = short_price - long_price;
    if credit <= 0.0 {
        return Err(Skip::NoCredit);
    }
    Ok(Choice {
        short: short.clone(),
        long: long.clone(),
        credit,
    })
}

/// A spread waiting on its long leg.
#[derive(Debug, Clone, Copy)]
struct Pending {
    open: Open,
    short_price: f64,
    now: NaiveDateTime,
}

/// A spread held.
#[derive(Debug, Clone, Copy)]
struct Open {
    short: InstrumentId,
    long: InstrumentId,
    expiration: NaiveDate,
    credit: f64,
}

pub(crate) struct PutSpread {
    core: StrategyCore,
    underlying: BarType,
    bar_types: Vec<BarType>,
    contracts: BTreeMap<InstrumentId, OptionContract>,
    rule: Rule,
    risk: Risk,
    trade_size: Quantity,
    /// The last print per instrument, and when.
    latest: HashMap<InstrumentId, (UnixNanos, f64)>,
    filling: Option<UnixNanos>,
    open: Option<Open>,
    /// The session an intraday rule last opened in. Once a session.
    entered_on: Option<NaiveDate>,
    /// A spread whose long leg has been sent and whose short waits on its fill.
    pending: Option<Pending>,
    /// An order the fill handler could not send, raised on the next bar: the
    /// handler has no way to return one, and swallowing it would leave a long
    /// leg held with nobody knowing why.
    failed: Option<String>,
    /// How often, and why, a flat rule opened nothing. Kept for a debugger and
    /// the tests; a result has nowhere to carry it yet.
    pub(crate) skipped: BTreeMap<&'static str, usize>,
}

impl PutSpread {
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
            open: None,
            entered_on: None,
            pending: None,
            failed: None,
            skipped: BTreeMap::new(),
        }
    }

    fn held(&self, id: &InstrumentId) -> f64 {
        f64::try_from(self.portfolio().net_position(id)).unwrap_or(0.0)
    }

    /// The price an instrument traded at, if it traded at `at`.
    fn printed(&self, id: &InstrumentId, at: UnixNanos) -> Option<f64> {
        self.latest
            .get(id)
            .filter(|(when, _)| *when == at)
            .map(|(_, price)| *price)
    }

    /// Everything the rule does at one completed instant.
    fn decide(&mut self, at: UnixNanos) -> anyhow::Result<()> {
        // A bar is stamped at the end of its period. A daily bar's day is the
        // one before its stamp and its prices are that day's 16:00 close; an
        // intraday bar's prices are as of its stamp.
        let Some(stamped) = super::nanos_to_instant(at) else {
            return Ok(());
        };
        let day = (stamped - chrono::Duration::seconds(1)).date();
        let as_of = match self.rule.entry_minutes {
            Some(_) => stamped,
            None => arvo_data::session::regular_close(day),
        };

        if self.pending.is_some() {
            return Ok(());
        }
        if let Some(open) = self.open {
            let (short, long) = (self.held(&open.short), self.held(&open.long));
            if short == 0.0 && long == 0.0 {
                self.open = None;
            } else {
                let value = self
                    .printed(&open.short, at)
                    .zip(self.printed(&open.long, at))
                    .map(|(s, l)| s - l);
                let broken = short == 0.0 || long == 0.0;
                let expiring = self
                    .rule
                    .exit_dte
                    .is_some_and(|exit| (open.expiration - day).num_days() <= exit);
                let kept = self.rule.take_profit < 1.0
                    && value.is_some_and(|value| value <= open.credit * (1.0 - self.rule.take_profit));
                let stopped = value.zip(self.rule.stop_multiple).is_some_and(
                    |(value, multiple)| value >= open.credit * multiple,
                );
                if broken || ((expiring || kept || stopped) && value.is_some()) {
                    // The short first: buying it back releases the cash the
                    // long sale does not need.
                    self.flatten(open.short)?;
                    self.flatten(open.long)?;
                }
                return Ok(());
            }
        }

        // An intraday rule opens once a session, in its entry window.
        if let Some(entry) = self.rule.entry_minutes {
            if self.entered_on == Some(day) {
                return Ok(());
            }
            let opened = arvo_data::session::regular_close(day) - chrono::Duration::minutes(390);
            let minutes = (stamped - opened).num_minutes();
            if minutes < entry || minutes > entry + ENTRY_WINDOW_MINUTES {
                return Ok(());
            }
        }

        let Some(spot) = self.printed(&self.underlying.instrument_id(), at) else {
            return Ok(());
        };
        let loaded: Vec<OptionContract> = self.contracts.values().cloned().collect();
        let traded: Vec<(OptionContract, f64)> = self
            .contracts
            .iter()
            .filter_map(|(id, contract)| {
                self.printed(id, at).map(|price| (contract.clone(), price))
            })
            .collect();
        let choice = match choose(self.rule, spot, day, as_of, &loaded, &traded) {
            Ok(choice) => choice,
            Err(skip) => {
                *self.skipped.entry(skip_name(skip)).or_default() += 1;
                return Ok(());
            }
        };
        self.open_spread(&choice, at, stamped)
    }

    fn open_spread(
        &mut self,
        choice: &Choice,
        at: UnixNanos,
        now: NaiveDateTime,
    ) -> anyhow::Result<()> {
        let venue = self.underlying.instrument_id().venue;
        let short_id = InstrumentId::new(choice.short.symbol().as_str().into(), venue);
        let long_id = InstrumentId::new(choice.long.symbol().as_str().into(), venue);
        let (Some(short_price), Some(long_price)) =
            (self.printed(&short_id, at), self.printed(&long_id, at))
        else {
            return Ok(());
        };

        // Whole spreads, and only as many as the account can complete. Asked
        // before anything is sent: buying the long leg and then finding the
        // short unaffordable meant selling the long back at a spread's loss,
        // and a rule short of cash did that every day it looked.
        let spreads = self.affordable_spreads(&short_id, &long_id, long_price, now.date());
        if spreads == 0 {
            *self.skipped.entry("refused").or_default() += 1;
            return Ok(());
        }
        let arvo_research::Decision::Accept {
            quantity: long_units,
        } = self.ask(
            &long_id,
            long_price,
            false,
            f64::from(spreads) * lot(&long_id),
            now,
        )
        else {
            *self.skipped.entry("refused").or_default() += 1;
            return Ok(());
        };
        let Ok(size) = Quantity::new_checked(long_units, 0) else {
            return Ok(());
        };
        self.send(long_id, OrderSide::Buy, size, None)?;
        self.entered_on = Some(now.date());
        self.pending = Some(Pending {
            open: Open {
                short: short_id,
                long: long_id,
                expiration: choice.short.expiration,
                credit: choice.credit,
            },
            short_price,
            now,
        });
        Ok(())
    }

    /// The long leg filled: sell the short against it, or unwind.
    fn long_filled(&mut self, quantity: f64) -> anyhow::Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let Ok(size) = Quantity::new_checked(quantity, 0) else {
            return Ok(());
        };
        match self.ask(
            &pending.open.short,
            pending.short_price,
            true,
            quantity,
            pending.now,
        ) {
            arvo_research::Decision::Accept { quantity: accepted } if accepted >= quantity => {
                self.send(pending.open.short, OrderSide::Sell, size, None)?;
                self.open = Some(pending.open);
            }
            _ => {
                // No half spreads: the long leg alone is a different position.
                *self.skipped.entry("refused").or_default() += 1;
                self.send(pending.open.long, OrderSide::Sell, size, Some(EXIT_SIGNAL))?;
            }
        }
        Ok(())
    }

    /// The most whole spreads, up to the rule's size, that the account can pay
    /// the long legs for and still secure the short legs against.
    fn affordable_spreads(
        &self,
        short: &InstrumentId,
        long: &InstrumentId,
        long_price: f64,
        today: NaiveDate,
    ) -> u32 {
        let wanted = (self.trade_size.as_f64() / lot(long)).floor();
        if wanted < 1.0 {
            return 0;
        }
        let (positions, _, _) = super::account_from_cache(&self.cache(), today);
        let cash = super::spendable(&self.cache(), &long.venue).unwrap_or(self.risk.starting_cash);
        let held: Vec<(&str, f64)> = positions
            .iter()
            .map(|(name, position)| (name.as_str(), position.quantity))
            .collect();
        let Ok(reserved) = arvo_research::collateral::reserved(held.iter().copied()) else {
            return 0;
        };
        let costs = self.risk.costs;
        let per_share = costs.option_spread.map_or(long_price, |spread| {
            long_price + spread.half_spread(long_price)
        }) * (1.0 + costs.commission_bps / 10_000.0);
        let (short_name, long_name) = (short.to_string(), long.to_string());
        let ceiling =
            self.risk.model.max_position_fraction.unwrap_or(1.0) * self.risk.starting_cash;

        (1..=wanted as u32)
            .rev()
            .find(|&spreads| {
                let units = f64::from(spreads) * lot(long);
                let long_cost = units * per_share + 2.0 * costs.per_fill;
                let mut with_long = held.clone();
                with_long.push((long_name.as_str(), units));
                let available = (cash - reserved).min(ceiling) - long_cost;
                arvo_research::collateral::sellable(&with_long, &short_name, available, spreads)
                    .is_ok_and(|sellable| sellable == spreads)
            })
            .unwrap_or(0)
    }

    /// One leg put to the gate, over the account as the engine holds it now.
    fn ask(
        &self,
        id: &InstrumentId,
        price: f64,
        opens_short: bool,
        size: f64,
        now: NaiveDateTime,
    ) -> arvo_research::Decision {
        let venue = id.venue;
        let (positions, realised_today, day_trades_used) =
            super::account_from_cache(&self.cache(), now.date());
        super::decide_entry(
            opens_short,
            &self.risk,
            Quantity::new_checked(size, 0).unwrap_or(self.trade_size),
            &id.to_string(),
            price,
            None,
            now,
            &positions,
            realised_today,
            false,
            self.risk.starting_cash,
            day_trades_used,
            super::spendable(&self.cache(), &venue),
            None,
        )
    }

    /// Closes whatever the venue says is held in one leg.
    fn flatten(&mut self, id: InstrumentId) -> anyhow::Result<()> {
        let held = self.held(&id);
        let side = if held < 0.0 {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };
        match Quantity::new_checked(held.abs(), 0) {
            Ok(size) if held != 0.0 => self.send(id, side, size, Some(EXIT_SIGNAL)),
            _ => Ok(()),
        }
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

const fn skip_name(skip: Skip) -> &'static str {
    match skip {
        Skip::NoExpiration => "no expiration",
        Skip::NoPrices => "no prices",
        Skip::EdgeOfChain => "edge of chain",
        Skip::NoLongLeg => "no long leg",
        Skip::NoCredit => "no credit",
    }
}

nautilus_trading::nautilus_strategy!(PutSpread, {
    fn on_order_filled(&mut self, event: &nautilus_model::events::OrderFilled) {
        let waiting = self
            .pending
            .is_some_and(|pending| pending.open.long == event.instrument_id);
        if waiting && event.order_side == OrderSide::Buy {
            if let Err(err) = self.long_filled(event.last_qty.as_f64()) {
                self.failed = Some(err.to_string());
            }
        }
    }
    fn on_order_denied(&mut self, event: nautilus_model::events::OrderDenied) {
        if self
            .pending
            .is_some_and(|pending| pending.open.long == event.instrument_id)
        {
            self.pending = None;
        }
    }
    fn on_order_rejected(&mut self, event: nautilus_model::events::OrderRejected) {
        if self
            .pending
            .is_some_and(|pending| pending.open.long == event.instrument_id)
        {
            self.pending = None;
        }
    }
});

impl std::fmt::Debug for PutSpread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PutSpread")
            .field("contracts", &self.contracts.len())
            .field("rule", &self.rule)
            .finish_non_exhaustive()
    }
}

impl DataActor for PutSpread {
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
        if let Some(failed) = self.failed.take() {
            anyhow::bail!("put_spread could not send its short leg: {failed}");
        }
        // As the ranking rule does: decide when an instant is complete, on the
        // prints that completed it, in an order that does not depend on which
        // bar the engine delivered first.
        if let Some(filling) = self.filling.filter(|filling| *filling != bar.ts_event) {
            self.decide(filling)?;
        }
        self.filling = Some(bar.ts_event);
        self.latest.insert(
            bar.bar_type.instrument_id(),
            (bar.ts_event, bar.close.as_f64()),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arvo_research::greeks::greeks;

    fn date(month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2025, month, day).expect("valid")
    }

    fn rule() -> Rule {
        Rule {
            dte: 35,
            short_delta: 0.20,
            width: 5.0,
            take_profit: 0.5,
            exit_dte: Some(21),
            stop_multiple: None,
            entry_minutes: None,
            rate: 0.04,
            dividend_yield: 0.013,
        }
    }

    /// A chain priced by the model itself at 15% volatility, so the delta each
    /// strike implies is the delta it was priced at.
    fn chain(
        expiration: NaiveDate,
        strikes: std::ops::RangeInclusive<u32>,
        day: NaiveDate,
    ) -> Vec<(OptionContract, f64)> {
        let market = Market {
            spot: 600.0,
            rate: 0.04,
            dividend_yield: 0.013,
        };
        // Priced the morning of `day`, so a same-day chain still has time left.
        let as_of = day.and_hms_opt(14, 30, 0).expect("valid");
        strikes
            .step_by(5)
            .map(|strike| {
                let contract = OptionContract {
                    underlying: "SPY".to_owned(),
                    expiration,
                    right: Right::Put,
                    strike: f64::from(strike),
                };
                let price =
                    greeks(&contract, market, years_to_expiry(&contract, as_of), 0.15).price;
                (contract, (price * 100.0).round() / 100.0)
            })
            .collect()
    }

    #[test]
    fn sells_the_put_nearest_the_delta_and_buys_one_a_width_below() {
        let day = date(8, 8);
        let traded = chain(date(9, 12), 500..=620, day);
        let loaded: Vec<_> = traded.iter().map(|(c, _)| c.clone()).collect();
        let choice = choose(
            rule(),
            600.0,
            day,
            arvo_data::session::regular_close(day),
            &loaded,
            &traded,
        )
        .expect("a spread");

        let market = Market {
            spot: 600.0,
            rate: 0.04,
            dividend_yield: 0.013,
        };
        let years = years_to_expiry(&choice.short, arvo_data::session::regular_close(day));
        let delta = -greeks(&choice.short, market, years, 0.15).delta;
        assert!(
            (delta - 0.20).abs() < 0.05,
            "short delta {delta} at {}",
            choice.short.strike
        );
        assert!((choice.short.strike - choice.long.strike - 5.0).abs() < 1e-9);
        assert!(choice.credit > 0.0);
    }

    #[test]
    fn picks_the_expiration_nearest_the_target() {
        let day = date(8, 8);
        let mut traded = chain(date(9, 5), 500..=620, day); // 28 days
        traded.extend(chain(date(9, 12), 500..=620, day)); // 35 days
        traded.extend(chain(date(9, 19), 500..=620, day)); // 42 days
        let loaded: Vec<_> = traded.iter().map(|(c, _)| c.clone()).collect();
        let choice = choose(
            rule(),
            600.0,
            day,
            arvo_data::session::regular_close(day),
            &loaded,
            &traded,
        )
        .expect("a spread");
        assert_eq!(choice.short.expiration, date(9, 12));
    }

    #[test]
    fn a_same_day_rule_takes_only_the_same_day() {
        let day = date(8, 8);
        let as_of = day.and_hms_opt(14, 30, 0).expect("valid");
        let zero = Rule {
            dte: 0,
            exit_dte: None,
            stop_multiple: Some(2.0),
            entry_minutes: Some(30),
            short_delta: 0.10,
            ..rule()
        };
        let tomorrow = chain(date(8, 11), 580..=610, day);
        let loaded: Vec<_> = tomorrow.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            choose(zero, 600.0, day, as_of, &loaded, &tomorrow),
            Err(Skip::NoExpiration),
            "Monday's contract is not Friday's 0DTE"
        );
        let today = chain(day, 580..=610, day);
        let loaded: Vec<_> = today.iter().map(|(c, _)| c.clone()).collect();
        let choice = choose(zero, 600.0, day, as_of, &loaded, &today).expect("a spread");
        assert_eq!(choice.short.expiration, day);
    }

    #[test]
    fn skips_rather_than_choosing_by_what_was_fetched() {
        let day = date(8, 8);
        let as_of = day.and_hms_opt(14, 30, 0).expect("valid");
        // Only strikes near the money were loaded: the 20-delta strike is below
        // all of them, so the nearest loaded one is not the rule's choice.
        let narrow = chain(date(9, 12), 590..=620, day);
        let loaded: Vec<_> = narrow.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            choose(rule(), 600.0, day, as_of, &loaded, &narrow),
            Err(Skip::EdgeOfChain)
        );

        // Nothing traded today.
        let whole = chain(date(9, 12), 500..=620, day);
        let loaded: Vec<_> = whole.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            choose(rule(), 600.0, day, as_of, &loaded, &[]),
            Err(Skip::NoPrices)
        );

        // Nothing near 35 days.
        let far = chain(date(12, 19), 500..=620, day);
        let loaded: Vec<_> = far.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            choose(rule(), 600.0, day, as_of, &loaded, &far),
            Err(Skip::NoExpiration)
        );
    }
}
