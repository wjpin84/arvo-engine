//! The single boundary between Arvo and NautilusTrader.
//!
//! # The containment rule
//!
//! **No crate other than this one may name a Nautilus type.** Nautilus's Rust
//! crates are published at `0.x` (0.63.0 at time of writing), so every release
//! may break; its own README treats Python as the primary API and Rust as
//! internal infrastructure. That is survivable only while the blast radius is
//! one crate, and the rule is what keeps it there.
//!
//! The rule is enforced by the direction of the dependency rather than by
//! anyone remembering it: [`arvo_research`] defines [`SimulationProvider`] and
//! this crate implements it, so the research domain cannot name a Nautilus
//! type even by accident. Nothing in this crate's public API mentions one
//! either — the only exported item is [`NautilusSimulation`], whose whole
//! surface is Arvo types.
//!
//! # What crosses the boundary
//!
//! Experiments down, results up — never trading primitives. Orders, fills,
//! positions and accounts exist only inside this crate and below. That is what
//! lets containment coexist with *not* duplicating Nautilus's domain model:
//! Arvo never manipulates a trading primitive, so it never needs to model one.
//!
//! # Panics are converted here, not propagated
//!
//! Nautilus's ergonomic constructors (`Price::new`, `Quantity::new`,
//! `BarSpecification::new`, ...) panic on invalid input; only the `_checked`
//! variants return a result. Everything below uses the checked forms, because
//! a bad number from a config file or a UI field is ordinary input, not a bug
//! worth aborting the process over.
//!
//! # Licensing
//!
//! NautilusTrader is LGPL-3.0-only and is linked into this binary. See the
//! repository `NOTICE`. A crate boundary is not a licensing boundary — the
//! obligation attaches to the distributed binary — but keeping the dependency
//! to this one crate keeps the fact obvious rather than diffuse.

mod fee;
mod fill;
mod ledger;
mod strategy;

use std::str::FromStr;
use std::sync::Once;

use arvo_data::BarProvider;
use arvo_research::{
    Experiment, SimulationError, SimulationProvider, SimulationResult, StrategySpec,
};
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
};
use nautilus_common::logging::logging_set_bypass;
use nautilus_execution::models::{fee::FeeModelHandle, fill::FillModelHandle};
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarSpecification, BarType, Data, IndexPriceUpdate},
    enums::{
        AccountType, AggregationSource, AssetClass, BarAggregation, BookType, OmsType, OptionKind,
        PriceType,
    },
    identifiers::{InstrumentId, Symbol},
    instruments::{Equity, IndexInstrument, InstrumentAny, OptionContract},
    types::{Currency, Money, Price, Quantity},
};
use nautilus_trading::strategy::{StrategyConfig, StrategyCore};
use rust_decimal::Decimal;
use ustr::Ustr;

/// The Nautilus version this crate is pinned to, recorded on every result.
///
/// A result is only comparable to another produced by the same engine, so the
/// version is part of the evidence rather than a build detail.
const ENGINE: &str = "nautilus 0.63.0";

/// The strategies wired up so far. See [`strategy`] for why these two.
const SMA_CROSS: &str = "sma_cross";
const BUY_AND_HOLD: &str = arvo_research::evaluation::BUY_AND_HOLD;
/// Sell an option to open and hold it to expiry (#84). Not in [`STRATEGIES`]:
/// it only means anything on an option contract, and nothing that offers the
/// menu can run one yet.
pub const SELL_AND_HOLD: &str = "sell_and_hold";
/// Sell put spreads on an underlying, choosing contracts from its chain (#86).
pub const PUT_SPREAD: &str = "put_spread";
/// Sell a same-day put spread at a fixed time and settle it, or stop out, by
/// the close (#87).
pub const ZERO_DTE_PUT_SPREAD: &str = "zero_dte_put_spread";
/// Buy a same-day call or put on a break of the session's opening range.
pub const ZERO_DTE_BREAKOUT: &str = "zero_dte_breakout";
const OPENING_RANGE: &str = "opening_range";
const VOLATILITY_BREAKOUT: &str = "volatility_breakout";
const VWAP_REVERSION: &str = "vwap_reversion";
const MOMENTUM_BREAKOUT: &str = "momentum_breakout";
/// The one rule here that ranks instruments against each other rather than
/// judging each on its own.
const CROSS_SECTIONAL: &str = "cross_sectional_momentum";

/// Every strategy this engine can run, for a caller that wants to offer a
/// choice rather than hardcode one.
pub const STRATEGIES: &[&str] = &[
    SMA_CROSS,
    OPENING_RANGE,
    VOLATILITY_BREAKOUT,
    VWAP_REVERSION,
    MOMENTUM_BREAKOUT,
    CROSS_SECTIONAL,
    BUY_AND_HOLD,
    PUT_SPREAD,
    ZERO_DTE_PUT_SPREAD,
    ZERO_DTE_BREAKOUT,
];

/// Strategies that rank instruments against each other, and therefore need
/// more than one to say anything at all.
///
/// A single-instrument run of one of these is not a weak result, it is a
/// meaningless one: the ranking has a field of one and holds it whatever it
/// did. Refused rather than run.
pub const CROSS_SECTIONAL_STRATEGIES: &[&str] = &[CROSS_SECTIONAL];

/// The strategies defined against a trading *session*, which therefore mean
/// nothing on daily bars.
///
/// On a daily series a session is one bar: an opening range is the whole day
/// and a session VWAP is that day's typical price. Both rules would still run
/// and produce a curve, which is exactly why this list exists.
pub const SESSION_ANCHORED: &[&str] = &[OPENING_RANGE, VWAP_REVERSION];

/// US equity conventions. Daily bars from the free exports are quoted in cents
/// and traded in whole shares; nothing yet needs another instrument class, and
/// guessing at one would mean guessing wrong.
const PRICE_PRECISION: u8 = 2;
const SIZE_PRECISION: u8 = 0;

/// Runs Arvo experiments on Nautilus's backtest engine.
///
/// Owns its data source so an experiment carries only a *reference* to its
/// dataset: reproducibility needs the identity of the input, and which
/// provider resolves that identity is a wiring decision, not a research one.
#[derive(Debug)]
pub struct NautilusSimulation<P> {
    bars: P,
}

impl<P: BarProvider> NautilusSimulation<P> {
    pub const fn new(bars: P) -> Self {
        Self { bars }
    }
}

impl<P: BarProvider> SimulationProvider for NautilusSimulation<P> {
    fn engine(&self) -> &str {
        ENGINE
    }

    /// Forwards to the data library, one instrument at a time.
    ///
    /// Nothing here touches Nautilus: distributions never reach the engine, and
    /// no backtest receives a cash credit for one. This exists because
    /// `arvo-research` reaches its data through this trait and nowhere else —
    /// see [`SimulationProvider::dividends`].
    ///
    /// An instrument with no series is left out of the map entirely rather than
    /// mapped to an empty list. `arvo_research::dividend` reads a missing key as
    /// *unknown* and an empty list as *paid nothing*, and those are different
    /// facts about a result.
    fn bars_for(
        &self,
        instrument: &str,
        interval: arvo_data::BarInterval,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Option<Vec<arvo_data::Bar>> {
        self.bars.bars(instrument, interval, from, to).ok()
    }

    fn dividends(
        &self,
        experiment: &Experiment,
    ) -> std::collections::HashMap<String, Vec<arvo_data::Dividend>> {
        experiment
            .instruments()
            .into_iter()
            .filter_map(|instrument| {
                let paid = self
                    .bars
                    .dividends(&instrument, experiment.window.from, experiment.window.to)
                    // A library that cannot be read is not a reason to fail a
                    // backtest that already ran. The gap goes unmeasured, which
                    // `advice` reports as unmeasured.
                    .ok()
                    .flatten()?;
                Some((instrument, paid))
            })
            .collect()
    }

    fn run(&self, experiment: &Experiment) -> Result<SimulationResult, SimulationError> {
        let plan = Plan::from_spec(&experiment.strategy, experiment.interval)?;
        experiment
            .risk
            .check()
            .map_err(|reason| SimulationError::Rejected(format!("risk model: {reason}")))?;
        experiment
            .costs
            .check()
            .map_err(|reason| SimulationError::Rejected(format!("cost model: {reason}")))?;

        experiment
            .check_instruments()
            .map_err(SimulationError::Rejected)?;

        if matches!(plan, Plan::PutSpread { .. } | Plan::ZeroDteBreakout { .. }) {
            return self.run_put_spread(experiment, &plan);
        }

        // An option run is priced by the option spread, and only by it. With no
        // spread every fill lands on the traded price for free; with equity
        // basis points as well, the cost is stated twice and applied once.
        let contracts: Vec<_> = experiment
            .instruments()
            .iter()
            .filter_map(|name| arvo_data::option::OptionContract::parse(name))
            .collect();
        let settlement = if contracts.is_empty() {
            None
        } else {
            Some(self.settlement(experiment, &contracts)?)
        };
        if !contracts.is_empty() {
            if experiment.costs.option_spread.is_none() {
                return Err(SimulationError::Rejected(
                    "cost model: an option run needs option_spread — without it every fill \
                     is at the traded price and crosses no spread"
                        .to_owned(),
                ));
            }
            if experiment.costs.slippage_bps != 0.0 {
                return Err(SimulationError::Rejected(
                    "cost model: slippage_bps is equity basis points; an option's spread is \
                     option_spread, so state it there and set slippage_bps to zero"
                        .to_owned(),
                ));
            }
        }

        // Every instrument the run holds, each with its own series. All of
        // them are checked before any of them is simulated: a book that is
        // going to fail on its third member should say so before spending the
        // time to run the first two.
        let mut book: Vec<(InstrumentId, String, Vec<arvo_data::Bar>)> = Vec::new();
        for name in experiment.instruments() {
            let id = InstrumentId::from_str(&name).map_err(|err| {
                SimulationError::Rejected(format!("instrument {name:?}: {err}"))
            })?;

            let bars = self
                .bars
                .bars(
                    &name,
                    experiment.interval,
                    experiment.window.from,
                    experiment.window.to,
                )
                // Named, because in a book the interesting half of this
                // failure is *which* member could not be loaded. Without it
                // the whole run reports "engine failed during the run" and the
                // reader has to guess which instrument to go and look at.
                .map_err(|err| {
                    SimulationError::Rejected(format!("reading {name}: {err}"))
                })?;

            if bars.is_empty() {
                return Err(SimulationError::NoData {
                    instrument: name,
                    from: experiment.window.from,
                    to: experiment.window.to,
                });
            }

            // A strategy that cannot even warm up has not been tested, and a
            // run that produces no signal is not evidence that there was none.
            if bars.len() <= plan.min_bars() {
                return Err(SimulationError::Rejected(format!(
                    "{name}: {} bars is not enough for {}, which needs more than {}",
                    bars.len(),
                    experiment.strategy.name,
                    plan.min_bars()
                )));
            }

            book.push((id, name, bars));
        }

        // A venue per run, taken from the head instrument. Every member has to
        // settle against the same balance for contention to exist at all, and
        // Nautilus accounts are per venue — so a book spanning two venues would
        // silently be two accounts, which is the one thing this must not be.
        let venue = book[0].0.venue;
        if let Some((id, name, _)) = book.iter().find(|(id, _, _)| id.venue != venue) {
            return Err(SimulationError::Rejected(format!(
                "{name} is on {} and {} is on {venue}: a shared account cannot span venues",
                id.venue, experiment.instrument,
            )));
        }

        run_backtest(experiment, &plan, &book, settlement.as_ref())
    }
}

/// What an option run settles against at expiry (#84).
struct Settlement {
    /// The underlying's symbol, which Nautilus looks up on the option's venue.
    symbol: String,
    /// The underlying's own bars, when a rule reads them to decide (#86).
    /// Empty for a run that only settles against it.
    drive: Vec<arvo_data::Bar>,
    /// Each contract expiring inside the window, with the underlying's close on
    /// that contract's expiration date.
    closes: Vec<(arvo_data::option::OptionContract, f64)>,
}

impl<P: BarProvider> NautilusSimulation<P> {
    /// The underlying an option run names, and its close on every expiration
    /// the window reaches.
    ///
    /// Refused rather than run without, because a contract held to expiry
    /// with nothing to settle against stays open and is marked at its last
    /// trade for as long as the window runs — found exactly that way, holding
    /// one for four months after it ceased to exist.
    fn settlement(
        &self,
        experiment: &Experiment,
        contracts: &[arvo_data::option::OptionContract],
    ) -> Result<Settlement, SimulationError> {
        let name = experiment.underlying.as_deref().ok_or_else(|| {
            SimulationError::Rejected(
                "an option run needs `underlying`, the stock series it settles against \
                 (e.g. SPY.AIEX): without it a contract held to expiry is never settled"
                    .to_owned(),
            )
        })?;
        self.settlement_against(name, experiment, contracts)
    }

    /// As [`Self::settlement`], against a named underlying.
    fn settlement_against(
        &self,
        name: &str,
        experiment: &Experiment,
        contracts: &[arvo_data::option::OptionContract],
    ) -> Result<Settlement, SimulationError> {
        let rejected = |why: String| SimulationError::Rejected(why);
        let symbol = name.split('.').next().unwrap_or_default();
        if let Some(contract) = contracts.iter().find(|c| c.underlying != symbol) {
            return Err(rejected(format!(
                "{} is an option on {}, and the underlying named is {name}",
                contract.symbol(),
                contract.underlying
            )));
        }
        let bars = self
            .bars
            .bars(name, experiment.interval, experiment.window.from, experiment.window.to)
            .map_err(|err| rejected(format!("reading underlying {name}: {err}")))?;

        let mut closes = Vec::new();
        for contract in contracts
            .iter()
            .filter(|c| c.expiration <= experiment.window.to)
        {
            // The last bar on the expiration date closes at 16:00 at any
            // resolution: the daily bar, or the 15:55 five-minute bar.
            let close = bars
                .iter()
                .rev()
                .find(|bar| bar.at.date() == contract.expiration)
                .map(|bar| bar.close)
                .ok_or_else(|| {
                    rejected(format!(
                        "{name} has no bar on {}, the day {} settles",
                        contract.expiration,
                        contract.symbol()
                    ))
                })?;
            closes.push((contract.clone(), close));
        }
        Ok(Settlement {
            symbol: symbol.to_owned(),
            drive: Vec::new(),
            closes,
        })
    }
}

/// How far below the underlying's lowest close a put spread run loads strikes.
///
/// Wide on purpose. A 20-delta put a month out sits a few percent below spot; a
/// short strike the rule wants that lies below what was loaded is caught and
/// skipped (`put_spread::Skip::EdgeOfChain`) rather than silently replaced.
const PUT_SPREAD_REACH: f64 = 0.30;

impl<P: BarProvider> NautilusSimulation<P> {
    /// A put spread run: the underlying drives, the chain's puts trade (#86).
    ///
    /// # Which contracts are loaded
    ///
    /// Puts expiring from the window's start to the rule's target days past its
    /// end, struck between [`PUT_SPREAD_REACH`] below the underlying's lowest
    /// price and its highest, over the days each could have been opened on.
    /// That range reads prices after an entry date, which would be look-ahead
    /// if it chose anything; it only decides what is *available*, as wide as the
    /// rule could want, and the rule refuses a strike at its edge.
    fn run_put_spread(
        &self,
        experiment: &Experiment,
        plan: &Plan,
    ) -> Result<SimulationResult, SimulationError> {
        // What the rule can choose from: how far past a day its expirations may
        // lie, whether it buys calls too, and so which side of the money needs
        // strikes.
        let (days_out, calls) = match *plan {
            Plan::PutSpread { rule, .. } => (rule.dte + 7, false),
            Plan::ZeroDteBreakout { .. } => (0, true),
            _ => return Err(SimulationError::Rejected("not a chain plan".to_owned())),
        };
        let rejected = |why: String| SimulationError::Rejected(why);
        let name = experiment.instrument.as_str();
        if arvo_data::option::OptionContract::parse(name).is_some()
            || !experiment.alongside.is_empty()
        {
            return Err(rejected(
                "put_spread runs on one underlying (e.g. SPY.AIEX) and chooses its own contracts"
                    .to_owned(),
            ));
        }
        if experiment
            .underlying
            .as_deref()
            .is_some_and(|underlying| underlying != name)
        {
            return Err(rejected(format!(
                "put_spread settles against its own instrument {name}, not {:?}",
                experiment.underlying
            )));
        }
        // The rule sizes by collateral and exits by its own levels. A stop, a
        // risk fraction or a drawdown halt would be recorded and not applied.
        let risk = experiment.risk;
        if risk.stop_atr_multiple.is_some()
            || risk.risk_per_trade.is_some()
            || risk.max_drawdown.is_some()
        {
            return Err(rejected(
                "put_spread does not apply a stop, a risk fraction or a drawdown halt; leave \
                 them unset rather than record limits the run ignores"
                    .to_owned(),
            ));
        }
        if experiment.costs.option_spread.is_none() || experiment.costs.slippage_bps != 0.0 {
            return Err(rejected(
                "cost model: an option run needs option_spread and no slippage_bps".to_owned(),
            ));
        }

        let (from, to) = (experiment.window.from, experiment.window.to);
        let underlying = self
            .bars
            .bars(name, experiment.interval, from, to)
            .map_err(|err| rejected(format!("reading {name}: {err}")))?;
        if underlying.is_empty() {
            return Err(SimulationError::NoData {
                instrument: name.to_owned(),
                from,
                to,
            });
        }
        let symbol = name.split('.').next().unwrap_or_default();

        let reach = chrono::Duration::days(days_out);
        let mut book = Vec::new();
        let mut contracts = Vec::new();
        let listed = self
            .bars
            .option_contracts(symbol, experiment.interval)
            .map_err(|err| rejected(format!("listing {symbol} contracts: {err}")))?;
        for contract_name in listed {
            let Some(contract) = arvo_data::option::OptionContract::parse(&contract_name) else {
                continue;
            };
            let is_call = contract.right == arvo_data::option::Right::Call;
            if (is_call && !calls) || contract.expiration < from || contract.expiration > to + reach
            {
                continue;
            }
            let openable = underlying
                .iter()
                .filter(|bar| {
                    bar.at.date() >= contract.expiration - reach
                        && bar.at.date() <= contract.expiration
                })
                .map(|bar| (bar.low, bar.high))
                .reduce(|(low, high), (l, h)| (low.min(l), high.max(h)));
            let Some((low, high)) = openable else {
                continue;
            };
            let (floor, ceiling) = if calls {
                (low * (1.0 - PUT_SPREAD_REACH), high * (1.0 + PUT_SPREAD_REACH))
            } else {
                (low * (1.0 - PUT_SPREAD_REACH), high)
            };
            if contract.strike < floor || contract.strike > ceiling {
                continue;
            }
            let bars = self
                .bars
                .bars(&contract_name, experiment.interval, from, to)
                .map_err(|err| rejected(format!("reading {contract_name}: {err}")))?;
            if bars.is_empty() {
                continue;
            }
            let id = InstrumentId::from_str(&contract_name)
                .map_err(|err| rejected(format!("instrument {contract_name:?}: {err}")))?;
            contracts.push(contract);
            book.push((id, contract_name, bars));
        }
        let Some(venue) = book.first().map(|(id, _, _)| id.venue) else {
            return Err(rejected(format!(
                "no {symbol} contracts in the library for {from}..{to} at {}; fetch the chain first",
                experiment.interval
            )));
        };
        if let Some((_, other, _)) = book.iter().find(|(id, _, _)| id.venue != venue) {
            return Err(rejected(format!(
                "{other} is not on {venue}: a shared account cannot span venues"
            )));
        }
        // Sorted, so instruments are added and ids issued in the same order on
        // every run.
        book.sort_by(|a, b| a.1.cmp(&b.1));

        let mut settlement = self.settlement_against(name, experiment, &contracts)?;
        settlement.drive = underlying;
        run_backtest(experiment, plan, &book, Some(&settlement))
    }
}

/// Whether the engine could run this strategy at this resolution.
///
/// The same validation `run` does, without the backtest. It exists so a caller
/// offering a menu of strategies can assert that everything on it is runnable
/// — a parameter the rule needs and nobody supplied, or a session-anchored
/// rule pointed at daily bars, is otherwise a failure the user meets after
/// choosing and waiting.
///
/// # Errors
///
/// Returns the same reason `run` would have given.
pub fn check_plan(
    spec: &StrategySpec,
    interval: arvo_data::BarInterval,
) -> Result<(), SimulationError> {
    Plan::from_spec(spec, interval).map(|_| ())
}

/// A strategy request, parsed out of the untyped spec and validated before
/// anything expensive starts.
enum Plan {
    SmaCross {
        fast_period: usize,
        slow_period: usize,
        trade_size: f64,
    },
    OpeningRange {
        range_bars: usize,
        target_range_multiple: f64,
        trade_size: f64,
    },
    VolatilityBreakout {
        entry_atr_multiple: f64,
        atr_period: usize,
        trade_size: f64,
    },
    VwapReversion {
        entry_deviations: f64,
        trade_size: f64,
    },
    MomentumBreakout {
        entry_period: usize,
        exit_period: usize,
        trade_size: f64,
    },
    CrossSectionalMomentum {
        lookback: usize,
        hold_top: usize,
        trade_size: f64,
    },
    BuyAndHold {
        trade_size: f64,
    },
    SellAndHold {
        trade_size: f64,
    },
    PutSpread {
        rule: strategy::PutSpreadRule,
        trade_size: f64,
    },
    ZeroDteBreakout {
        rule: strategy::BreakoutRule,
        trade_size: f64,
    },
}

impl Plan {
    /// Parses and validates a strategy request.
    ///
    /// The interval is needed as well as the spec because two of these rules
    /// are only defined intraday, and a resolution mismatch is not something
    /// the result would show: the run completes, the curve looks ordinary, and
    /// the numbers describe a rule nobody meant to test.
    fn from_spec(
        spec: &StrategySpec,
        interval: arvo_data::BarInterval,
    ) -> Result<Self, SimulationError> {
        let param = |name: &str| -> Result<f64, SimulationError> {
            spec.params.get(name).copied().ok_or_else(|| {
                SimulationError::Rejected(format!("{} requires a {name:?} parameter", spec.name))
            })
        };
        let period = |name: &str| -> Result<usize, SimulationError> {
            let value = param(name)?;
            if !value.is_finite() || value < 1.0 || value.fract() != 0.0 || value > 10_000.0 {
                return Err(SimulationError::Rejected(format!(
                    "{name} must be a whole number of bars between 1 and 10000, got {value}"
                )));
            }
            Ok(value as usize)
        };
        let trade_size = || -> Result<f64, SimulationError> {
            let value = param("trade_size")?;
            // `is_finite` first: every comparison against NaN is false, so a
            // bare `<= 0.0` would wave NaN straight through into sizing.
            if !value.is_finite() || value <= 0.0 {
                return Err(SimulationError::Rejected(format!(
                    "trade_size must be positive, got {value}"
                )));
            }
            Ok(value)
        };

        let multiple = |name: &str| -> Result<f64, SimulationError> {
            let value = param(name)?;
            if !value.is_finite() || value <= 0.0 || value > 100.0 {
                return Err(SimulationError::Rejected(format!(
                    "{name} must be a positive multiple no greater than 100, got {value}"
                )));
            }
            Ok(value)
        };

        if SESSION_ANCHORED.contains(&spec.name.as_str()) && !interval.is_intraday() {
            return Err(SimulationError::Rejected(format!(
                "{} is defined against a trading session and cannot run on {interval} bars; at \
                 that resolution a session is a single bar, so the rule would still produce a \
                 curve while measuring something nobody asked for",
                spec.name
            )));
        }

        match spec.name.as_str() {
            SMA_CROSS => {
                let fast_period = period("fast")?;
                let slow_period = period("slow")?;
                if fast_period >= slow_period {
                    return Err(SimulationError::Rejected(format!(
                        "fast period {fast_period} must be shorter than slow period {slow_period}"
                    )));
                }
                Ok(Self::SmaCross {
                    fast_period,
                    slow_period,
                    trade_size: trade_size()?,
                })
            }
            OPENING_RANGE => Ok(Self::OpeningRange {
                range_bars: period("range_bars")?,
                target_range_multiple: multiple("target_range_multiple")?,
                trade_size: trade_size()?,
            }),
            VOLATILITY_BREAKOUT => Ok(Self::VolatilityBreakout {
                entry_atr_multiple: multiple("entry_atr_multiple")?,
                atr_period: period("atr_period")?,
                trade_size: trade_size()?,
            }),
            VWAP_REVERSION => Ok(Self::VwapReversion {
                entry_deviations: multiple("entry_deviations")?,
                trade_size: trade_size()?,
            }),
            MOMENTUM_BREAKOUT => {
                let entry_period = period("entry_period")?;
                let exit_period = period("exit_period")?;
                if exit_period > entry_period {
                    return Err(SimulationError::Rejected(format!(
                        "exit period {exit_period} must not exceed entry period {entry_period}; a \
                         rule that needs more evidence to leave than to enter gives most of a \
                         trend back before it admits the trend ended"
                    )));
                }
                Ok(Self::MomentumBreakout {
                    entry_period,
                    exit_period,
                    trade_size: trade_size()?,
                })
            }
            CROSS_SECTIONAL => {
                let lookback = period("lookback")?;
                let hold_top = period("hold_top")?;
                Ok(Self::CrossSectionalMomentum {
                    lookback,
                    hold_top,
                    trade_size: trade_size()?,
                })
            }
            BUY_AND_HOLD => Ok(Self::BuyAndHold {
                trade_size: trade_size()?,
            }),
            SELL_AND_HOLD => Ok(Self::SellAndHold {
                trade_size: trade_size()?,
            }),
            ZERO_DTE_BREAKOUT => {
                if !interval.is_intraday() {
                    return Err(SimulationError::Rejected(format!(
                        "{ZERO_DTE_BREAKOUT} reads a session's opening range and cannot run on \
                         {interval} bars"
                    )));
                }
                let delta = param("delta")?;
                if !delta.is_finite() || delta <= 0.0 || delta >= 1.0 {
                    return Err(SimulationError::Rejected(format!(
                        "delta must be a fraction between 0 and 1, got {delta}"
                    )));
                }
                let target_multiple = param("target_multiple")?;
                if !target_multiple.is_finite() || target_multiple <= 1.0 || target_multiple > 20.0 {
                    return Err(SimulationError::Rejected(format!(
                        "target_multiple is what the option must reach as a multiple of its price, \
                         above 1 and no more than 20, got {target_multiple}"
                    )));
                }
                let stop_fraction = param("stop_fraction")?;
                if !stop_fraction.is_finite() || stop_fraction <= 0.0 || stop_fraction >= 1.0 {
                    return Err(SimulationError::Rejected(format!(
                        "stop_fraction is the share of the price lost before selling, between 0 \
                         and 1, got {stop_fraction}"
                    )));
                }
                let rate = |name: &str| -> Result<f64, SimulationError> {
                    let value = param(name)?;
                    if !value.is_finite() || !(0.0..0.5).contains(&value) {
                        return Err(SimulationError::Rejected(format!(
                            "{name} is a yearly fraction (0.04 is 4%), got {value}"
                        )));
                    }
                    Ok(value)
                };
                Ok(Self::ZeroDteBreakout {
                    rule: strategy::BreakoutRule {
                        range_bars: period("range_bars")?,
                        delta,
                        target_multiple,
                        stop_fraction,
                        rate: rate("rate")?,
                        dividend_yield: rate("dividend_yield")?,
                    },
                    trade_size: trade_size()?,
                })
            }
            PUT_SPREAD | ZERO_DTE_PUT_SPREAD => {
                let same_day = spec.name == ZERO_DTE_PUT_SPREAD;
                // A month-out spread is chosen on a day's closes; a same-day one
                // at a time of day, which daily bars do not have.
                if same_day != interval.is_intraday() {
                    return Err(SimulationError::Rejected(format!(
                        "{} decides on {} bars and cannot run on {interval} bars",
                        spec.name,
                        if same_day { "intraday" } else { "daily" }
                    )));
                }
                let fraction = |name: &str, upper_inclusive: bool| -> Result<f64, SimulationError> {
                    let value = param(name)?;
                    let fits = value.is_finite()
                        && value > 0.0
                        && (value < 1.0 || (upper_inclusive && value <= 1.0));
                    if !fits {
                        return Err(SimulationError::Rejected(format!(
                            "{name} must be a fraction between 0 and 1, got {value}"
                        )));
                    }
                    Ok(value)
                };
                let rate = |name: &str| -> Result<f64, SimulationError> {
                    let value = param(name)?;
                    if !value.is_finite() || !(0.0..0.5).contains(&value) {
                        return Err(SimulationError::Rejected(format!(
                            "{name} is a yearly fraction (0.04 is 4%), got {value}"
                        )));
                    }
                    Ok(value)
                };
                let (dte, exit_dte, stop_multiple, entry_minutes) = if same_day {
                    let stop = param("stop_multiple")?;
                    if !stop.is_finite() || stop <= 1.0 || stop > 20.0 {
                        return Err(SimulationError::Rejected(format!(
                            "stop_multiple is what buying the spread back may cost as a multiple \
                             of its credit, above 1 and no more than 20, got {stop}"
                        )));
                    }
                    let entry = param("entry_minutes")?;
                    // Leaves the last hour: an entry in it is a different trade.
                    if !entry.is_finite() || entry.fract() != 0.0 || !(0.0..=330.0).contains(&entry) {
                        return Err(SimulationError::Rejected(format!(
                            "entry_minutes is whole minutes after the open, 0 to 330, got {entry}"
                        )));
                    }
                    (0, None, Some(stop), Some(entry as i64))
                } else {
                    let dte = period("dte")?;
                    let exit_dte = param("exit_dte")?;
                    if !exit_dte.is_finite()
                        || exit_dte < 0.0
                        || exit_dte.fract() != 0.0
                        || exit_dte >= dte as f64
                    {
                        return Err(SimulationError::Rejected(format!(
                            "exit_dte must be a whole number of days below dte ({dte}), got {exit_dte}"
                        )));
                    }
                    (dte as i64, Some(exit_dte as i64), None, None)
                };
                let width = param("width")?;
                if !width.is_finite() || width <= 0.0 {
                    return Err(SimulationError::Rejected(format!(
                        "width is a strike distance in dollars and must be positive, got {width}"
                    )));
                }
                Ok(Self::PutSpread {
                    rule: strategy::PutSpreadRule {
                        dte,
                        short_delta: fraction("short_delta", false)?,
                        width,
                        take_profit: fraction("take_profit", true)?,
                        exit_dte,
                        stop_multiple,
                        entry_minutes,
                        rate: rate("rate")?,
                        dividend_yield: rate("dividend_yield")?,
                    },
                    trade_size: trade_size()?,
                })
            }
            _ => Err(SimulationError::UnknownStrategy(spec.name.clone())),
        }
    }

    /// Bars needed before the strategy can act at all.
    const fn min_bars(&self) -> usize {
        match self {
            Self::SmaCross { slow_period, .. } => *slow_period,
            // The range itself. A window that only covers the range has not
            // given the rule a single bar to break out on.
            Self::OpeningRange { range_bars, .. } => *range_bars,
            Self::VolatilityBreakout { atr_period, .. } => *atr_period,
            // Enough of a session for a volume-weighted deviation to mean
            // something, which is the gate this rule's entry waits on.
            Self::VwapReversion { .. } => 5,
            Self::MomentumBreakout { entry_period, .. } => *entry_period,
            // The lookback the ranking is computed over. Until every
            // instrument has one, the field is partial and the ranking is a
            // statement about whichever happened to warm up first.
            Self::CrossSectionalMomentum { lookback, .. } => *lookback,
            // One to buy on, and at least one more for the position to have
            // done anything.
            Self::BuyAndHold { .. } | Self::SellAndHold { .. } | Self::PutSpread { .. } => 1,
            // The range has to be formed before a break means anything.
            Self::ZeroDteBreakout { rule, .. } => rule.range_bars,
        }
    }

    const fn trade_size(&self) -> f64 {
        match self {
            Self::SmaCross { trade_size, .. }
            | Self::OpeningRange { trade_size, .. }
            | Self::VolatilityBreakout { trade_size, .. }
            | Self::VwapReversion { trade_size, .. }
            | Self::MomentumBreakout { trade_size, .. }
            | Self::CrossSectionalMomentum { trade_size, .. }
            | Self::BuyAndHold { trade_size }
            | Self::SellAndHold { trade_size }
            | Self::PutSpread { trade_size, .. }
            | Self::ZeroDteBreakout { trade_size, .. } => *trade_size,
        }
    }
}

/// Stops Nautilus writing its own log to stdout.
///
/// Nautilus installs a process-wide logger and emits a line per order and per
/// bar. Arvo already owns observability (`arvo_runtime::init_tracing`), and a
/// release build has no console at all — `windows_subsystem = "windows"` — so
/// that output goes nowhere while still costing the run.
///
/// `BacktestEngineConfig::bypass_logging` looks like the knob for this and is
/// not: nothing in the Rust kernel reads that field, it is honoured on the
/// Python side. `logging_set_bypass` is the switch that works.
///
/// It is a global, so this is deliberately process-wide and set once. Engine
/// failures are unaffected — they come back as `Err` from `run`, not as a log
/// line somebody has to notice.
fn silence_nautilus_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // `NAUTILUS_LOG` is Nautilus's own escape hatch. If it is set, someone
        // is deliberately debugging an engine run, so leave their logging
        // alone — a silence with no way out is how integration faults stay
        // hidden.
        if std::env::var_os("NAUTILUS_LOG").is_none() {
            logging_set_bypass();
        }
    });
}

fn run_backtest(
    experiment: &Experiment,
    plan: &Plan,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
    settlement: Option<&Settlement>,
) -> Result<SimulationResult, SimulationError> {
    let rejected = |context: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("{context}: {err}"))
    };

    silence_nautilus_logging();

    // Checked by the caller, which will not build an empty book.
    let venue = book
        .first()
        .ok_or_else(|| SimulationError::Rejected("no instruments to run".to_owned()))?
        .0
        .venue;

    let mut engine = BacktestEngine::new(BacktestEngineConfig::default())
        .map_err(|err| rejected("creating the engine", &err))?;

    let currency = Currency::USD();
    let starting_balance = Money::new_checked(experiment.starting_cash, currency)
        .map_err(|err| rejected("starting cash", &err))?;

    engine
        .add_venue(
            SimulatedVenueConfig::builder()
                .venue(venue)
                .oms_type(OmsType::Netting)
                .account_type(AccountType::Cash)
                .book_type(BookType::L1_MBP)
                .starting_balances(vec![starting_balance])
                .bar_execution(true)
                // Only when there is something to charge that a rate cannot
                // say. With nothing extra, Nautilus's own model computes the
                // same number, and leaving it in place keeps every result
                // produced before venue fees existed reproducible.
                .fee_model(if experiment.costs.has_venue_fees() {
                    FeeModelHandle::new(fee::VenueFees::new(&experiment.costs))
                } else {
                    FeeModelHandle::default()
                })
                .build()
                .map_err(|err| rejected("venue config", &err))?,
        )
        .map_err(|err| rejected("adding the venue", &err))?;

    // Only when there is slippage to apply. A zero-slippage run keeps
    // Nautilus's own matching untouched, so every result produced before this
    // existed is still reproducible byte for byte.
    if experiment.costs.slippage_bps != 0.0 {
        let model = fill::BpsSlippage::new(experiment.costs.slippage_bps)
            .map_err(SimulationError::Rejected)?;
        engine.change_fill_model(venue, FillModelHandle::new(model));
    }
    // `run` has refused an option run without a spread, or with basis points
    // beside it, so this is the only fill model an option run can have.
    if let Some(spread) = experiment.costs.option_spread {
        engine.change_fill_model(
            venue,
            FillModelHandle::new(fill::PremiumSpread::new(spread)),
        );
    }

    let (step, aggregation) = aggregation_of(experiment.interval)?;
    let spec = BarSpecification::new_checked(step, aggregation, PriceType::Last)
        .map_err(|err| rejected("bar specification", &err))?;

    let mut bar_types = Vec::with_capacity(book.len());
    for (instrument_id, _, bars) in book {
        let instrument = match arvo_data::option::OptionContract::parse(&instrument_id.to_string())
        {
            Some(contract) => option(
                *instrument_id,
                &contract,
                currency,
                experiment.costs.commission_bps,
            ),
            None => equity(*instrument_id, currency, experiment.costs.commission_bps),
        }
        .map_err(|err| rejected("building the instrument", &err))?;
        engine
            .add_instrument(&instrument)
            .map_err(|err| rejected("adding the instrument", &err))?;

        // `External` says these bars arrived already aggregated rather than
        // being built by the engine from ticks, which is what a daily export is.
        let bar_type = BarType::new(*instrument_id, spec, AggregationSource::External);

        let data = bars
            .iter()
            .map(|bar| to_nautilus_bar(bar_type, bar, experiment.interval).map(Data::Bar))
            .collect::<Result<Vec<_>, _>>()?;

        // Once per instrument rather than one merged batch: Nautilus sorts what
        // it is given, and handing it each series separately keeps the merge its
        // problem rather than a second place interleaving could go wrong.
        engine
            .add_data(data, None, true, true)
            .map_err(|err| rejected("adding bar data", &err))?;

        bar_types.push(bar_type);
    }

    // The underlying, on the option's venue, because that is where Nautilus's
    // own settlement looks for it — and registered as an *index*, so that an
    // in-the-money contract is settled in cash at its intrinsic value and an
    // out-of-the-money one at nothing.
    //
    // # Cash, not shares
    //
    // SPY options deliver shares, and Nautilus will deliver them if asked. In a
    // cash account that is wrong more often than right: sizing pays for the
    // premium, not the strike, so exercising one in-the-money call a $10k
    // account holds bought $64,000 of SPY it could never have owned, and every
    // figure afterwards carried six times the account in stock. A broker sells
    // such a contract at the close instead. Settling at intrinsic value against
    // the close is that sale, and it is also what delivery nets to wherever the
    // shares would be sold again — a spread's two legs, or a 0DTE position.
    // What it does not model is a strategy that means to keep the shares.
    //
    // The one price settlement reads is the underlying's index price, so each
    // expiration gets exactly one: that day's close, a nanosecond before the
    // contract's expiry fires. Daily bars are stamped at the end of their day,
    // after 16:00 — without this, a daily run would settle on the day before.
    let mut driver: Option<BarType> = None;
    if let Some(settlement) = settlement {
        let underlying_id = InstrumentId::from(format!("{}.{venue}", settlement.symbol).as_str());
        let index = IndexInstrument::builder()
            .instrument_id(underlying_id)
            .raw_symbol(Symbol::from(settlement.symbol.as_str()))
            .currency(currency)
            .price_precision(PRICE_PRECISION)
            .size_precision(SIZE_PRECISION)
            .price_increment(
                Price::new_checked(0.01, PRICE_PRECISION)
                    .map_err(|err| rejected("underlying tick", &err))?,
            )
            .size_increment(Quantity::from(1))
            .ts_event(UnixNanos::default())
            .ts_init(UnixNanos::default())
            .build()
            .map_err(|err| rejected("building the underlying", &err))?;
        engine
            .add_instrument(&InstrumentAny::IndexInstrument(index))
            .map_err(|err| rejected("adding the underlying", &err))?;
        let prints = settlement
            .closes
            .iter()
            .map(|(contract, close)| {
                let at = contract
                    .expires_at()
                    .and_utc()
                    .timestamp_nanos_opt()
                    .and_then(|ns| u64::try_from(ns - 1).ok())
                    .ok_or_else(|| {
                        SimulationError::Rejected(format!(
                            "{} expiry is not representable",
                            contract.symbol()
                        ))
                    })?;
                // ponytail: rounded to the cent, so a 657.535 close settles at
                // 657.54 — up to half a cent a share, $0.50 a contract, either
                // way. Round against each contract's holder if that matters.
                let price = Price::new_checked(*close, PRICE_PRECISION)
                    .map_err(|err| rejected("underlying close", &err))?;
                Ok(Data::IndexPrice(IndexPriceUpdate::new(
                    underlying_id,
                    price,
                    UnixNanos::from(at),
                    UnixNanos::from(at),
                )))
            })
            .collect::<Result<Vec<_>, SimulationError>>()?;
        if !prints.is_empty() {
            engine
                .add_data(prints, None, true, true)
                .map_err(|err| rejected("adding settlement prints", &err))?;
        }
        if !settlement.drive.is_empty() {
            let bar_type = BarType::new(underlying_id, spec, AggregationSource::External);
            let data = settlement
                .drive
                .iter()
                .map(|bar| to_nautilus_bar(bar_type, bar, experiment.interval).map(Data::Bar))
                .collect::<Result<Vec<_>, _>>()?;
            engine
                .add_data(data, None, true, true)
                .map_err(|err| rejected("adding underlying bars", &err))?;
            driver = Some(bar_type);
        }
    }

    let trade_size = Quantity::new_checked(plan.trade_size(), SIZE_PRECISION)
        .map_err(|err| rejected("trade size", &err))?;

    // Risk is expressed as a fraction of capital in the record and as an
    // amount of money here: the strategy needs a distance-to-loss in the same
    // units as the price it is stopping against.
    //
    // Measured against *starting* capital rather than current equity, so
    // sizing is fixed-fractional rather than compounding. That is a real and
    // common choice, but it is a choice — a compounding version risks more
    // after a win and less after a loss, and would produce a different curve.
    //
    // Every member of a book gets the same limits, expressed against the whole
    // account rather than a share of it. That is deliberate: a per-member cap
    // of `1/N` would pre-allocate capital and there would be nothing left to
    // contend for. The contention is the measurement.
    // The model travels whole rather than as resolved currency amounts. The
    // fractions are divided out at decision time by `arvo_research::decide`,
    // which is the same function a live gate calls — so the engine and a live
    // session cannot drift apart on how a limit is applied.
    let risk = strategy::Risk {
        model: experiment.risk,
        costs: experiment.costs,
        starting_cash: experiment.starting_cash,
    };

    // One estimate for the whole run, shared by every strategy instance. A book's
    // members know nothing of each other, so a per-instrument tracker would only
    // ever see one series and could not correlate anything with anything.
    //
    // Fed from bars as the engine delivers them, never computed over the window
    // up front — that would refuse a trade in March on the strength of how two
    // instruments moved in November.
    let correlations = std::sync::Arc::new(arvo_research::RollingCorrelations::default());

    // A ranking rule is one decision-maker over the whole set, not one per
    // instrument: it has to see every score before it can say which is best.
    // So it is added once, subscribed to all of them, and the per-instrument
    // loop below is skipped entirely.
    if let (Plan::ZeroDteBreakout { rule, .. }, Some(driver)) = (plan, driver) {
        let core = StrategyCore::new(StrategyConfig {
            strategy_id: None,
            order_id_tag: Some("001".to_owned()),
            oms_type: Some(OmsType::Netting),
            ..StrategyConfig::default()
        });
        engine
            .add_strategy(strategy::ZeroDteBreakout::new(
                core,
                driver,
                bar_types.clone(),
                *rule,
                risk,
                trade_size,
            ))
            .map_err(|err| rejected("adding the strategy", &err))?;
        let clock = settlement.map(|settlement| {
            (format!("{}.{venue}", settlement.symbol), settlement.drive.as_slice())
        });
        return finish_marked(engine, experiment, book, clock);
    }

    if let (Plan::PutSpread { rule, .. }, Some(driver)) = (plan, driver) {
        let core = StrategyCore::new(StrategyConfig {
            strategy_id: None,
            order_id_tag: Some("001".to_owned()),
            oms_type: Some(OmsType::Netting),
            ..StrategyConfig::default()
        });
        engine
            .add_strategy(strategy::PutSpread::new(
                core,
                driver,
                bar_types.clone(),
                *rule,
                risk,
                trade_size,
            ))
            .map_err(|err| rejected("adding the strategy", &err))?;
        let clock = settlement.map(|settlement| {
            (format!("{}.{venue}", settlement.symbol), settlement.drive.as_slice())
        });
        return finish_marked(engine, experiment, book, clock);
    }

    if let Plan::CrossSectionalMomentum {
        lookback, hold_top, ..
    } = *plan
    {
        let core = StrategyCore::new(StrategyConfig {
            strategy_id: None,
            order_id_tag: Some("001".to_owned()),
            oms_type: Some(OmsType::Netting),
            ..StrategyConfig::default()
        });
        engine
            .add_strategy(strategy::CrossSectionalMomentum::new(
                core,
                bar_types.clone(),
                trade_size,
                lookback,
                hold_top,
                risk,
                correlations.clone(),
            ))
            .map_err(|err| rejected("adding the strategy", &err))?;
        return finish(engine, experiment, book);
    }

    // One strategy instance per instrument, all settling against the one
    // account added above. This is what makes capital contention real: when two
    // members want in at the same time, the second is filled out of whatever
    // the first left, and on a cash account it may not be filled at all. A
    // simulation that runs each instrument separately with the whole balance
    // behind it cannot express that, however its results are combined
    // afterwards.
    //
    // The rules themselves are untouched. Each still sees exactly one
    // instrument; sharing is the account's job, not theirs.
    for (index, bar_type) in bar_types.iter().copied().enumerate() {
        let core = StrategyCore::new(StrategyConfig {
            strategy_id: None,
            // Distinct per member, because Nautilus builds client order ids
            // from it. Two strategies sharing a tag collide on their first
            // simultaneous order — which is exactly the case a book exists to
            // simulate, so it must not be an id collision instead.
            order_id_tag: Some(format!("{:03}", index + 1)),
            oms_type: Some(OmsType::Netting),
            // Deliberately NOT `manage_stop`. It looks like the right thing —
            // flatten open positions when the run ends so nothing is left
            // unrealised — but Nautilus already marks open positions to market
            // in its returns series, so it changes no number, and its
            // market-exit loop never completes in a backtest with no data left
            // to fill against. The trader then never reaches STOPPED and
            // disposal fails on every single run.
            ..StrategyConfig::default()
        });

        match *plan {
            Plan::SmaCross {
                fast_period,
                slow_period,
                ..
            } => engine.add_strategy(strategy::SmaCross::new(
                core,
                bar_type,
                trade_size,
                fast_period,
                slow_period,
                risk,
                correlations.clone(),
            )),
            Plan::OpeningRange {
                range_bars,
                target_range_multiple,
                ..
            } => engine.add_strategy(strategy::OpeningRange::new(
                core,
                bar_type,
                trade_size,
                range_bars,
                target_range_multiple,
                risk,
                correlations.clone(),
            )),
            Plan::VolatilityBreakout {
                entry_atr_multiple,
                atr_period,
                ..
            } => engine.add_strategy(strategy::VolatilityBreakout::new(
                core,
                bar_type,
                trade_size,
                entry_atr_multiple,
                atr_period,
                risk,
                correlations.clone(),
            )),
            Plan::VwapReversion {
                entry_deviations, ..
            } => engine.add_strategy(strategy::VwapReversion::new(
                core,
                bar_type,
                trade_size,
                entry_deviations,
                risk,
                correlations.clone(),
            )),
            Plan::MomentumBreakout {
                entry_period,
                exit_period,
                ..
            } => engine.add_strategy(strategy::MomentumBreakout::new(
                core,
                bar_type,
                trade_size,
                entry_period,
                exit_period,
                risk,
                correlations.clone(),
            )),
            Plan::BuyAndHold { .. } => {
                engine.add_strategy(strategy::BuyAndHold::new(
                    core,
                    bar_type,
                    trade_size,
                    experiment.starting_cash,
                    experiment.costs,
                    correlations.clone(),
                ))
            }
            Plan::SellAndHold { .. } => engine.add_strategy(strategy::SellAndHold::new(
                core,
                bar_type,
                trade_size,
                risk,
                correlations.clone(),
            )),
            Plan::CrossSectionalMomentum { .. }
            | Plan::PutSpread { .. }
            | Plan::ZeroDteBreakout { .. } => {
                // Added once for the whole set above, and returned before
                // reaching here. The compiler cannot see that, so this says
                // it rather than pretending the case is possible.
                return Err(SimulationError::Rejected(
                    "a ranking or chain rule is added once across every instrument, not once per instrument"
                        .to_owned(),
                ));
            }
        }
        .map_err(|err| rejected("adding the strategy", &err))?;
    }

    finish(engine, experiment, book)
}

/// Runs the engine and reads the result back.
///
/// Shared by the two ways strategies get added — one per instrument, or one
/// across all of them — so a ranking rule and an ordinary one cannot come to
/// differ in how their results are collected.
fn finish(
    engine: BacktestEngine,
    experiment: &Experiment,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
) -> Result<SimulationResult, SimulationError> {
    finish_marked(engine, experiment, book, None)
}

/// As [`finish`], with the curve's clock taken from `driver` and only the
/// instruments the ledger holds marked (#87).
///
/// A chain run holds tens of thousands of contracts it never trades. Marking
/// the curve against every one of them at every bar is instants times
/// instruments — minutes per run at five-minute bars, for a curve that only the
/// traded few can move. The underlying's bars give every instant a session
/// has; the traded contracts give every mark.
fn finish_marked(
    mut engine: BacktestEngine,
    experiment: &Experiment,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
    driver: Option<(String, &[arvo_data::Bar])>,
) -> Result<SimulationResult, SimulationError> {
    // The window is already expressed by the data: bars were filtered to it on
    // the way in, so bounding the run again would only add a way to disagree
    // with itself.
    engine
        .run(None, None, Some(experiment.id.to_string()), false)
        .map_err(|err| SimulationError::Engine(Box::new(BacktestFailed(err.to_string()))))?;

    // Before `dispose`: the positions live in the kernel's cache, and
    // disposal is what tears it down.
    let ledger = ledger::from_cache(&engine.kernel_mut().cache.borrow());
    let refused = ledger::refused(&engine.kernel_mut().cache.borrow());
    engine.dispose();

    // From the ledger and the prices, not from `engine.get_result()`. Nautilus
    // reports a `returns_series`, and it is not an equity curve: it is the
    // day-over-day change in the account's *cash balance*, which on a cash
    // account excludes the market value of anything held. Buying reads as a
    // catastrophic loss and selling as an enormous gain, both the size of the
    // position's notional. See `arvo_research::trade::equity_curve`.
    let series: Vec<(String, Vec<arvo_data::Bar>)> = match driver {
        None => book
            .iter()
            .map(|(_, name, bars)| (name.clone(), bars.clone()))
            .collect(),
        Some((name, bars)) => {
            let traded: std::collections::BTreeSet<&str> =
                ledger.iter().map(|trade| trade.instrument.as_str()).collect();
            std::iter::once((name, bars.to_vec()))
                .chain(
                    book.iter()
                        .filter(|(_, name, _)| traded.contains(name.as_str()))
                        .map(|(_, name, bars)| (name.clone(), bars.clone())),
                )
                .collect()
        }
    };
    let equity_curve = arvo_research::trade::equity_curve(
        experiment.starting_cash,
        &series,
        experiment.interval,
        &ledger,
    );

    Ok(SimulationResult {
        experiment: experiment.id.clone(),
        engine: ENGINE.to_owned(),
        // Positions, not legs: a spread is one trade (see `trade::positions`).
        trades: u32::try_from(arvo_research::trade::positions(&ledger).len()).unwrap_or(u32::MAX),
        equity_curve,
        ledger,
        refused,
    })
}

/// Builds the traded instrument, with the experiment's commission applied as
/// the venue fee.
///
/// The cost model has to reach the engine or pinning it in the experiment is
/// theatre — this is the half that does reach it.
fn equity(
    instrument_id: InstrumentId,
    currency: Currency,
    commission_bps: f64,
) -> anyhow::Result<InstrumentAny> {
    let fee = Decimal::try_from(commission_bps / 10_000.0)?;
    let tick = Price::new_checked(0.01, PRICE_PRECISION)?;

    // Optional fields are left unset rather than passed as `None`: the builder
    // applies the same defaults checked construction would.
    let equity = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .currency(currency)
        .price_precision(PRICE_PRECISION)
        .price_increment(tick)
        .maker_fee(fee)
        .taker_fee(fee)
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;

    Ok(InstrumentAny::Equity(equity))
}

/// Builds an option contract, traded in shares of what it delivers.
///
/// # A multiplier of one, in lots of a hundred
///
/// A contract is 100 shares' worth, quoted per share. Nautilus can carry that
/// as a multiplier of 100 on a quantity of contracts — and then every figure
/// Arvo derives from a fill (the equity curve, a stop distance, sizing, the
/// per-unit fees) would need to know to multiply, and each one that did not
/// would be wrong by a factor of a hundred without failing.
///
/// So the unit is one share of the deliverable: a multiplier of one, a price
/// per share as quoted, and a quantity that the risk gate only ever sizes in
/// hundreds (`arvo_research::risk`). A price times a quantity is dollars
/// everywhere, as it is for a stock.
///
/// The expiration is real, so Nautilus refuses an order after it, and that
/// refusal reaches the ledger like any other.
fn option(
    instrument_id: InstrumentId,
    contract: &arvo_data::option::OptionContract,
    currency: Currency,
    commission_bps: f64,
) -> anyhow::Result<InstrumentAny> {
    let fee = Decimal::try_from(commission_bps / 10_000.0)?;
    let tick = Price::new_checked(0.01, PRICE_PRECISION)?;
    let expires = contract
        .expires_at()
        .and_utc()
        .timestamp_nanos_opt()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "expiration {} is not a representable instant",
                contract.expiration
            )
        })?;

    let option = OptionContract::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .asset_class(AssetClass::Equity)
        .underlying(Ustr::from(contract.underlying.as_str()))
        .option_kind(match contract.right {
            arvo_data::option::Right::Call => OptionKind::Call,
            arvo_data::option::Right::Put => OptionKind::Put,
        })
        .strike_price(Price::new_checked(contract.strike, PRICE_PRECISION)?)
        .currency(currency)
        .activation_ns(UnixNanos::default())
        .expiration_ns(UnixNanos::from(u64::try_from(expires)?))
        .price_precision(PRICE_PRECISION)
        .price_increment(tick)
        .multiplier(Quantity::from(1))
        .lot_size(Quantity::from(arvo_data::option::MULTIPLIER as u64))
        .maker_fee(fee)
        .taker_fee(fee)
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;

    Ok(InstrumentAny::OptionContract(option))
}

fn to_nautilus_bar(
    bar_type: BarType,
    bar: &arvo_data::Bar,
    interval: arvo_data::BarInterval,
) -> Result<Bar, SimulationError> {
    let rejected = |what: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("bar {}: {what}: {err}", bar.at))
    };

    let price = |name: &str, value: f64| {
        Price::new_checked(value, PRICE_PRECISION).map_err(|err| rejected(name, &err))
    };

    // A bar is only knowable once its period has closed. Timestamping it at
    // the *end* of that period is what stops a strategy acting on a close it
    // could not have seen yet — the look-ahead bias this platform exists to
    // catch. That was close-of-day while everything was daily; it is
    // close-of-bar now, and the daily case is unchanged by it.
    let ts = close_of_bar(bar.at, interval).ok_or_else(|| {
        SimulationError::Rejected(format!(
            "bar timestamp {} is outside the representable range",
            bar.at
        ))
    })?;

    Bar::new_checked(
        bar_type,
        price("open", bar.open)?,
        price("high", bar.high)?,
        price("low", bar.low)?,
        price("close", bar.close)?,
        Quantity::new_checked(bar.volume, SIZE_PRECISION)
            .map_err(|err| rejected("volume", &err))?,
        ts,
        ts,
    )
    .map_err(|err| rejected("failed Nautilus's OHLC checks", &err))
}

/// The instant a bar's period ends, as UNIX nanoseconds.
fn close_of_bar(at: chrono::NaiveDateTime, interval: arvo_data::BarInterval) -> Option<UnixNanos> {
    let end = at.checked_add_signed(interval.duration())?.and_utc();
    let nanos = end.timestamp_nanos_opt()?;
    u64::try_from(nanos).ok().map(UnixNanos::from)
}

/// Maps an Arvo interval onto Nautilus's own aggregation vocabulary.
///
/// # Errors
///
/// Returns [`SimulationError::Unsupported`] for a resolution Nautilus has no
/// aggregation for, rather than silently substituting a neighbouring one — a
/// backtest quietly run at the wrong resolution is worse than one refused.
fn aggregation_of(
    interval: arvo_data::BarInterval,
) -> Result<(usize, BarAggregation), SimulationError> {
    use arvo_data::IntervalUnit;
    let step = usize::try_from(interval.step).map_err(|_| {
        SimulationError::Unsupported(format!("interval step {} is too large", interval.step))
    })?;
    let aggregation = match interval.unit {
        IntervalUnit::Second => BarAggregation::Second,
        IntervalUnit::Minute => BarAggregation::Minute,
        IntervalUnit::Hour => BarAggregation::Hour,
        IntervalUnit::Day => BarAggregation::Day,
        IntervalUnit::Week => BarAggregation::Week,
    };
    Ok((step, aggregation))
}

/// Wraps a Nautilus engine failure so it can cross the boundary as a plain
/// `std::error::Error` without exporting a Nautilus type.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct BacktestFailed(String);

#[cfg(test)]
mod tests;
