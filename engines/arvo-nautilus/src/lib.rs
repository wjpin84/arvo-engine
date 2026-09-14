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
    data::{Bar, BarSpecification, BarType, Data},
    enums::{
        AccountType, AggregationSource, AssetClass, BarAggregation, BookType, OmsType, OptionKind,
        PriceType,
    },
    identifiers::{InstrumentId, Symbol},
    instruments::{Equity, InstrumentAny, OptionContract},
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

        // An option run is priced by the option spread, and only by it. With no
        // spread every fill lands on the traded price for free; with equity
        // basis points as well, the cost is stated twice and applied once.
        let contracts: Vec<_> = experiment
            .instruments()
            .iter()
            .filter_map(|name| arvo_data::option::OptionContract::parse(name))
            .collect();
        // A position held through expiry is neither exercised nor assigned nor
        // expired — it stays open, marked at its last trade, for as long as
        // the window runs. Settlement is #84; until it exists a window that
        // outlives a contract is refused rather than valued as if it had not.
        if let Some(contract) = contracts
            .iter()
            .find(|contract| experiment.window.to > contract.expiration)
        {
            return Err(SimulationError::Rejected(format!(
                "{} expires {} and the window runs to {}: settlement at expiry is not \
                 modelled yet, so end the window on or before the expiration",
                contract.symbol(),
                contract.expiration,
                experiment.window.to
            )));
        }
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

        run_backtest(experiment, &plan, &book)
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
            Self::BuyAndHold { .. } => 1,
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
            | Self::BuyAndHold { trade_size } => *trade_size,
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
                    correlations.clone(),
                ))
            }
            Plan::CrossSectionalMomentum { .. } => {
                // Added once for the whole set above, and returned before
                // reaching here. The compiler cannot see that, so this says
                // it rather than pretending the case is possible.
                return Err(SimulationError::Rejected(
                    "a ranking rule is added once across every instrument, not once per instrument"
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
    mut engine: BacktestEngine,
    experiment: &Experiment,
    book: &[(InstrumentId, String, Vec<arvo_data::Bar>)],
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
    let series: Vec<(String, Vec<arvo_data::Bar>)> = book
        .iter()
        .map(|(_, name, bars)| (name.clone(), bars.clone()))
        .collect();
    let equity_curve = arvo_research::trade::equity_curve(
        experiment.starting_cash,
        &series,
        experiment.interval,
        &ledger,
    );

    Ok(SimulationResult {
        experiment: experiment.id.clone(),
        engine: ENGINE.to_owned(),
        trades: u32::try_from(ledger.len()).unwrap_or(u32::MAX),
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
mod tests {
    use super::*;
    use arvo_data::InMemoryBars;
    use chrono::NaiveTime;
    use arvo_research::{
        CostModel, DatasetRef, DateRange, ExperimentId, HypothesisId, StrategySpec,
    };
    use chrono::NaiveDate;
    use std::collections::BTreeMap;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("test date is valid")
    }

    /// A deterministic price path that crosses in both directions, so the
    /// strategy has something to react to.
    fn sawtooth(days: usize) -> Vec<arvo_data::Bar> {
        let mut start = date(2024, 1, 1);
        let mut bars = Vec::with_capacity(days);
        for index in 0..days {
            // Slow drift up, with a cycle superimposed to force crossings.
            let phase = (index % 40) as f64;
            let cycle = if phase < 20.0 { phase } else { 40.0 - phase };
            let close = 100.0 + index as f64 * 0.05 + cycle * 0.5;
            bars.push(arvo_data::Bar {
                at: start.and_time(NaiveTime::MIN),
                open: close,
                high: close + 0.5,
                low: close - 0.5,
                close,
                volume: 10_000.0,
            });
            start = start.succ_opt().expect("date stays in range");
        }
        bars
    }

    /// Five-minute bars across whole sessions, shaped so a session-anchored
    /// rule has something to find: a quiet opening range, then a break, then
    /// a fade back through the session's average.
    ///
    /// Timestamps are UTC and sit inside US regular hours — January, so
    /// 14:30–21:00 — and a session never crosses UTC midnight, which is the
    /// assumption `strategy::indicator::Session` is built on.
    fn sessions(count: usize, bars_each: usize) -> Vec<arvo_data::Bar> {
        let mut day = date(2024, 1, 2);
        let mut bars = Vec::with_capacity(count * bars_each);
        for session in 0..count {
            let open = day.and_hms_opt(14, 30, 0).expect("valid");
            // Alternate the direction of the break so neither a breakout rule
            // nor a reversion rule is handed a one-sided fixture.
            let sign = if session % 2 == 0 { 1.0 } else { -1.0 };
            for index in 0..bars_each {
                let phase = index as f64 / bars_each as f64;
                // Flat for the first fifth, then a directional leg, then a
                // partial retrace.
                let close = 100.0
                    + sign
                        * if phase < 0.2 {
                            0.0
                        } else if phase < 0.6 {
                            (phase - 0.2) * 25.0
                        } else {
                            10.0 - (phase - 0.6) * 15.0
                        };
                bars.push(arvo_data::Bar {
                    at: open + chrono::Duration::minutes(5 * index as i64),
                    open: close,
                    high: close + 0.2,
                    low: close - 0.2,
                    close,
                    volume: 10_000.0 + index as f64 * 100.0,
                });
            }
            day = day.succ_opt().expect("date stays in range");
        }
        bars
    }

    fn intraday_experiment(
        name: &str,
        params: BTreeMap<String, f64>,
        bars: &[arvo_data::Bar],
    ) -> Experiment {
        let mut experiment = experiment(params, bars);
        experiment.strategy.name = name.to_owned();
        experiment.interval = arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);
        experiment
    }

    fn intraday_provider(bars: Vec<arvo_data::Bar>) -> NautilusSimulation<InMemoryBars> {
        NautilusSimulation::new(InMemoryBars::new().with_interval(
            "AAPL.NASDAQ",
            arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute),
            bars,
        ))
    }

    fn experiment(params: BTreeMap<String, f64>, bars: &[arvo_data::Bar]) -> Experiment {
        Experiment {
            id: ExperimentId::from("e-1"),
            hypothesis: HypothesisId::from("h-1"),
            instrument: "AAPL.NASDAQ".to_owned(),
            alongside: Vec::new(),
            window: DateRange::new(
                bars.first().expect("fixture is not empty").at.date(),
                bars.last().expect("fixture is not empty").at.date(),
            )
            .expect("fixture window is ordered"),
            interval: arvo_data::BarInterval::DAILY,
            dataset: DatasetRef {
                id: "fixture".to_owned(),
                version: "1".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
            strategy: StrategySpec {
                name: SMA_CROSS.to_owned(),
                params,
            },
            costs: CostModel::proportional(1.0, 0.0),
            risk: arvo_research::RiskModel::default(),
            starting_cash: 100_000.0,
            seed: 42,
        }
    }

    fn experiment_named(
        name: &str,
        params: BTreeMap<String, f64>,
        bars: &[arvo_data::Bar],
    ) -> Experiment {
        let mut experiment = experiment(params, bars);
        experiment.strategy.name = name.to_owned();
        experiment
    }

    fn params(fast: f64, slow: f64) -> BTreeMap<String, f64> {
        BTreeMap::from([
            ("fast".to_owned(), fast),
            ("slow".to_owned(), slow),
            ("trade_size".to_owned(), 100.0),
        ])
    }

    fn provider(bars: Vec<arvo_data::Bar>) -> NautilusSimulation<InMemoryBars> {
        NautilusSimulation::new(InMemoryBars::new().with_instrument("AAPL.NASDAQ", bars))
    }

    /// A price path that compounds at `drift` per bar, so a set of them has a
    /// ranking known in advance.
    fn drifting(days: usize, drift: f64) -> Vec<arvo_data::Bar> {
        let mut start = date(2024, 1, 1);
        let mut bars = Vec::with_capacity(days);
        for index in 0..days {
            let close = 100.0 * (1.0 + drift).powi(i32::try_from(index).expect("small"));
            bars.push(arvo_data::Bar {
                at: start.and_time(NaiveTime::MIN),
                open: close,
                high: close,
                low: close,
                close,
                volume: 10_000.0,
            });
            start = start.succ_opt().expect("date stays in range");
        }
        bars
    }

    fn cross_sectional_params(lookback: f64, hold_top: f64) -> BTreeMap<String, f64> {
        BTreeMap::from([
            ("lookback".to_owned(), lookback),
            ("hold_top".to_owned(), hold_top),
            ("trade_size".to_owned(), 10.0),
        ])
    }

    #[test]
    fn a_ranking_rule_holds_the_risers_and_not_the_fallers() {
        // The property no other rule here can express. Three instruments that
        // rise and one that falls, over a window where the ranking never
        // changes: the faller must never be bought, and the risers must be.
        let mut library = InMemoryBars::new();
        library = library.with_instrument("FAST.SIM", drifting(200, 0.004));
        library = library.with_instrument("MID.SIM", drifting(200, 0.002));
        library = library.with_instrument("SLOW.SIM", drifting(200, 0.001));
        library = library.with_instrument("DOWN.SIM", drifting(200, -0.002));

        let bars = drifting(200, 0.004);
        let mut experiment = experiment_named(
            CROSS_SECTIONAL,
            cross_sectional_params(20.0, 2.0),
            &bars,
        );
        experiment.instrument = "FAST.SIM".to_owned();
        experiment.alongside = vec![
            "MID.SIM".to_owned(),
            "SLOW.SIM".to_owned(),
            "DOWN.SIM".to_owned(),
        ];

        let result = NautilusSimulation::new(library)
            .run(&experiment)
            .expect("the ranking rule should run");

        let traded: std::collections::BTreeSet<&str> = result
            .ledger
            .iter()
            .map(|trade| trade.instrument.as_str())
            .collect();
        assert!(
            !traded.contains("DOWN.SIM"),
            "a falling instrument is never in the top two: {traded:?}"
        );
        assert!(
            traded.contains("FAST.SIM"),
            "the fastest riser must be held: {traded:?}"
        );
    }

    #[test]
    fn a_ranking_rule_trades_under_the_risk_model_the_record_pins() {
        // Every other test here builds its experiment from `RiskModel::default`,
        // which configures no stop. The runtime's template configures one, and
        // `Position::plan` refuses to size at all when a stop is asked for and
        // no ATR is supplied. The ranking rule passed no ATR, so under the only
        // risk model it was ever actually run with it bought nothing — six
        // configurations that each looked like a failed idea rather than one
        // unfed indicator.
        //
        // So this fixes the risk model rather than the assertion: the property
        // is that the rule trades under the model the record pins, not under
        // the one the tests found convenient.
        let mut library = InMemoryBars::new();
        for (name, drift) in [
            ("A.SIM", 0.004),
            ("B.SIM", 0.003),
            ("C.SIM", 0.002),
            ("D.SIM", -0.002),
        ] {
            library = library.with_instrument(name, drifting(200, drift));
        }

        let bars = drifting(200, 0.004);
        let mut experiment =
            experiment_named(CROSS_SECTIONAL, cross_sectional_params(20.0, 2.0), &bars);
        experiment.instrument = "A.SIM".to_owned();
        experiment.alongside = vec!["B.SIM".to_owned(), "C.SIM".to_owned(), "D.SIM".to_owned()];
        experiment.risk = arvo_research::RiskModel {
            stop_atr_multiple: Some(2.0),
            atr_period: 14,
            risk_per_trade: Some(0.01),
            ..arvo_research::RiskModel::default()
        };

        let result = NautilusSimulation::new(library)
            .run(&experiment)
            .expect("runs");

        assert!(
            !result.ledger.is_empty(),
            "a configured stop must size the position, not silence the rule"
        );
        let traded: std::collections::BTreeSet<&str> = result
            .ledger
            .iter()
            .map(|trade| trade.instrument.as_str())
            .collect();
        assert!(
            !traded.contains("D.SIM"),
            "the ranking still governs what is bought: {traded:?}"
        );
    }

    #[test]
    fn a_ranking_rule_holds_no_more_than_it_was_told_to() {
        // Four instruments, hold the top two. Concurrency is read from the
        // ledger's own open and close times rather than from anything the
        // strategy reported, so it is what the account did.
        let mut library = InMemoryBars::new();
        for (name, drift) in [
            ("A.SIM", 0.004),
            ("B.SIM", 0.003),
            ("C.SIM", 0.002),
            ("D.SIM", 0.001),
        ] {
            library = library.with_instrument(name, drifting(200, drift));
        }

        let bars = drifting(200, 0.004);
        let mut experiment =
            experiment_named(CROSS_SECTIONAL, cross_sectional_params(20.0, 2.0), &bars);
        experiment.instrument = "A.SIM".to_owned();
        experiment.alongside = vec!["B.SIM".to_owned(), "C.SIM".to_owned(), "D.SIM".to_owned()];

        let result = NautilusSimulation::new(library)
            .run(&experiment)
            .expect("runs");

        let mut edges: Vec<(chrono::NaiveDateTime, i32)> = Vec::new();
        for trade in &result.ledger {
            edges.push((trade.opened, 1));
            if let Some(closed) = trade.closed {
                edges.push((closed, -1));
            }
        }
        edges.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let (mut live, mut peak) = (0, 0);
        for (_, delta) in edges {
            live += delta;
            peak = peak.max(live);
        }
        assert!(peak <= 2, "told to hold two, held {peak} at once");
    }

    #[test]
    fn a_ranking_rule_over_one_instrument_is_a_field_of_one() {
        // Not a weak result, a meaningless one: the ranking holds whatever it
        // has whatever it did. Worth knowing that it runs rather than breaks,
        // and worth the caller refusing it — see `CROSS_SECTIONAL_STRATEGIES`.
        let bars = drifting(200, 0.004);
        let experiment =
            experiment_named(CROSS_SECTIONAL, cross_sectional_params(20.0, 2.0), &bars);
        let result = NautilusSimulation::new(
            InMemoryBars::new().with_instrument("AAPL.NASDAQ", bars.clone()),
        )
        .run(&experiment)
        .expect("it runs");
        assert!(
            result.trades <= 1,
            "one riser, held throughout: {} trades",
            result.trades
        );
    }

    #[test]
    fn an_experiment_runs_end_to_end_through_nautilus() {
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);
        let simulation = provider(bars);

        let result = simulation
            .run(&experiment)
            .expect("the backtest should run");

        assert_eq!(result.experiment, experiment.id);
        assert_eq!(result.engine, ENGINE);
        assert!(
            !result.equity_curve.is_empty(),
            "a completed run always has at least its opening balance"
        );
        assert!(
            (result.equity_curve[0].equity - experiment.starting_cash).abs() < f64::EPSILON,
            "the curve opens at the starting balance"
        );
        assert!(result.trades > 0, "a crossing path should trade");
    }

    #[test]
    fn the_same_experiment_twice_gives_the_same_answer() {
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);

        let first = provider(bars.clone())
            .run(&experiment)
            .expect("first run should succeed");
        let second = provider(bars)
            .run(&experiment)
            .expect("second run should succeed");

        assert_eq!(
            first.equity_curve, second.equity_curve,
            "reproducibility is the whole point; two identical experiments must agree"
        );
        assert_eq!(first.trades, second.trades);
    }

    #[test]
    fn every_advertised_strategy_can_actually_be_planned() {
        // `STRATEGIES` is what a caller offers in a menu. A name in it that no
        // arm of `from_spec` matches is a rejection the user only discovers
        // after picking it.
        let intraday = arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);
        for name in STRATEGIES {
            let spec = StrategySpec {
                name: (*name).to_owned(),
                params: BTreeMap::from([
                    ("fast".to_owned(), 5.0),
                    ("slow".to_owned(), 20.0),
                    ("trade_size".to_owned(), 100.0),
                    ("range_bars".to_owned(), 6.0),
                    ("target_range_multiple".to_owned(), 2.0),
                    ("entry_atr_multiple".to_owned(), 1.5),
                    ("atr_period".to_owned(), 14.0),
                    ("entry_deviations".to_owned(), 2.0),
                    ("entry_period".to_owned(), 20.0),
                    ("exit_period".to_owned(), 10.0),
                    ("lookback".to_owned(), 60.0),
                    ("hold_top".to_owned(), 3.0),
                ]),
            };
            assert!(
                Plan::from_spec(&spec, intraday).is_ok(),
                "{name} is advertised but cannot be planned"
            );
        }
    }

    #[test]
    fn a_session_anchored_rule_refuses_daily_bars() {
        // Both would run happily and produce a curve: on daily bars the
        // opening range is the whole day and the session VWAP is that day's
        // typical price. The numbers would describe a rule nobody asked for.
        let bars = sawtooth(200);
        for name in SESSION_ANCHORED {
            let mut experiment = experiment(params(5.0, 20.0), &bars);
            experiment.strategy.name = (*name).to_owned();
            experiment
                .strategy
                .params
                .extend([
                    ("range_bars".to_owned(), 6.0),
                    ("target_range_multiple".to_owned(), 2.0),
                    ("entry_deviations".to_owned(), 2.0),
                ]);

            let err = provider(bars.clone())
                .run(&experiment)
                .expect_err("a session is one bar at this resolution");
            assert!(
                matches!(err, SimulationError::Rejected(ref why) if why.contains("session")),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn an_opening_range_takes_at_most_one_trade_a_session() {
        // Re-entering on the same range turns one signal into several
        // correlated bets on the same premise, which inflates the very count
        // the evaluation criteria use to decide there is enough evidence.
        let bars = sessions(10, 40);
        let experiment = intraday_experiment(
            OPENING_RANGE,
            BTreeMap::from([
                ("range_bars".to_owned(), 6.0),
                ("target_range_multiple".to_owned(), 2.0),
                ("trade_size".to_owned(), 10.0),
            ]),
            &bars,
        );

        let result = intraday_provider(bars).run(&experiment).expect("runs");
        assert!(result.trades > 0, "the fixture breaks its range every day");
        assert!(
            result.trades <= 10,
            "{} trades across 10 sessions is more than one a day",
            result.trades
        );
    }

    #[test]
    fn vwap_reversion_trades_and_exits_at_the_average() {
        let bars = sessions(10, 40);
        let experiment = intraday_experiment(
            VWAP_REVERSION,
            BTreeMap::from([
                ("entry_deviations".to_owned(), 1.0),
                ("trade_size".to_owned(), 10.0),
            ]),
            &bars,
        );

        let result = intraday_provider(bars).run(&experiment).expect("runs");
        assert!(
            result.trades > 0,
            "the fixture stretches away from its VWAP every session"
        );
        // Reversion holds for part of a session, never across one.
        for trade in result.ledger.iter().filter(|t| t.closed.is_some()) {
            let held = trade.holding_period().expect("closed");
            assert!(
                held <= chrono::Duration::days(1),
                "a session rule held {held} — it should flatten at the boundary"
            );
        }
    }

    #[test]
    fn a_volatility_breakout_trades_on_a_moving_fixture() {
        let bars = sawtooth(300);
        let experiment = experiment_named(
            VOLATILITY_BREAKOUT,
            BTreeMap::from([
                ("entry_atr_multiple".to_owned(), 0.5),
                ("atr_period".to_owned(), 14.0),
                ("trade_size".to_owned(), 100.0),
            ]),
            &bars,
        );

        let result = provider(bars).run(&experiment).expect("runs");
        assert!(result.trades > 0, "a sawtooth thrusts in both directions");
    }

    #[test]
    fn a_momentum_breakout_trades_on_a_trending_fixture() {
        let bars = sawtooth(300);
        let experiment = experiment_named(
            MOMENTUM_BREAKOUT,
            BTreeMap::from([
                ("entry_period".to_owned(), 20.0),
                ("exit_period".to_owned(), 10.0),
                ("trade_size".to_owned(), 100.0),
            ]),
            &bars,
        );

        let result = provider(bars).run(&experiment).expect("runs");
        assert!(result.trades > 0, "the fixture drifts up through its channel");
    }

    #[test]
    fn a_breakout_that_leaves_slower_than_it_enters_is_refused() {
        let bars = sawtooth(100);
        let experiment = experiment_named(
            MOMENTUM_BREAKOUT,
            BTreeMap::from([
                ("entry_period".to_owned(), 10.0),
                ("exit_period".to_owned(), 50.0),
                ("trade_size".to_owned(), 100.0),
            ]),
            &bars,
        );

        let err = provider(bars)
            .run(&experiment)
            .expect_err("a slower exit gives the trend back before admitting it ended");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn an_unknown_strategy_is_named_back() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.strategy.name = "buy_the_dip".to_owned();

        let err = provider(bars)
            .run(&experiment)
            .expect_err("nothing implements that");
        assert!(
            matches!(err, SimulationError::UnknownStrategy(ref name) if name == "buy_the_dip"),
            "{err}"
        );
    }

    #[test]
    fn slippage_makes_the_same_strategy_worse() {
        // The reason the feature exists: a backtest run without it is
        // optimistic, and the optimism has to show up as a number.
        let bars = sawtooth(200);
        let clean = experiment(params(5.0, 10.0), &bars);
        let mut slipped = clean.clone();
        slipped.costs.slippage_bps = 25.0;

        let without = provider(bars.clone()).run(&clean).expect("runs");
        let with = provider(bars).run(&slipped).expect("runs");

        assert_eq!(
            without.trades, with.trades,
            "slippage should cost money, not change which signals fired"
        );
        let final_equity = |result: &SimulationResult| {
            result.equity_curve.last().expect("non-empty").equity
        };
        assert!(
            final_equity(&with) < final_equity(&without),
            "slipped {} should end below unslipped {}",
            final_equity(&with),
            final_equity(&without)
        );
    }

    /// The sawtooth, fifty times cheaper and on whole cents: an option's price.
    fn premium_path(days: usize) -> Vec<arvo_data::Bar> {
        let cents = |value: f64| (value / 50.0 * 100.0).round() / 100.0;
        sawtooth(days)
            .into_iter()
            .map(|bar| arvo_data::Bar {
                open: cents(bar.open),
                high: cents(bar.high),
                low: cents(bar.low),
                close: cents(bar.close),
                ..bar
            })
            .collect()
    }

    fn option_run(
        contract: &str,
        costs: CostModel,
        bars: &[arvo_data::Bar],
    ) -> Result<SimulationResult, SimulationError> {
        let mut experiment = experiment(params(5.0, 10.0), bars);
        experiment.instrument = contract.to_owned();
        experiment.costs = costs;
        NautilusSimulation::new(InMemoryBars::new().with_instrument(contract, bars.to_vec()))
            .run(&experiment)
    }

    const LATE_CONTRACT: &str = "SPY241220C00100000.AOPT";

    fn spread(spread: arvo_research::OptionSpread) -> CostModel {
        CostModel {
            option_spread: Some(spread),
            ..CostModel::proportional(0.0, 0.0)
        }
    }

    #[test]
    fn an_option_run_without_a_spread_is_refused_not_run_free() {
        let bars = premium_path(200);
        let err = option_run(LATE_CONTRACT, CostModel::proportional(1.0, 0.0), &bars)
            .expect_err("no spread");
        assert!(
            matches!(err, SimulationError::Rejected(ref why) if why.contains("option_spread")),
            "{err}"
        );

        let mut both = spread(arvo_research::OptionSpread::MEASURED);
        both.slippage_bps = 5.0;
        let err = option_run(LATE_CONTRACT, both, &bars).expect_err("a cost stated twice");
        assert!(
            matches!(err, SimulationError::Rejected(ref why) if why.contains("slippage_bps")),
            "{err}"
        );
    }

    #[test]
    fn an_option_trades_whole_contracts_and_pays_its_spread_on_every_fill() {
        let bars = premium_path(200);
        let tight = option_run(
            LATE_CONTRACT,
            spread(arvo_research::OptionSpread {
                min_half_spread: 0.0,
                half_spread_fraction: 0.0,
            }),
            &bars,
        )
        .expect("runs");
        let measured = option_run(
            LATE_CONTRACT,
            spread(arvo_research::OptionSpread::MEASURED),
            &bars,
        )
        .expect("runs");

        assert!(
            measured.trades > 0,
            "the fixture has to trade to test anything"
        );
        assert_eq!(
            tight.trades, measured.trades,
            "the spread costs money; it does not move signals"
        );
        for trade in &measured.ledger {
            assert!(
                trade.quantity >= 100.0 && trade.quantity % 100.0 == 0.0,
                "{} is not whole contracts",
                trade.quantity
            );
        }
        // The tightest book still moves a price one tick, so the traded price
        // is that tick back from the tight fill. From there the measured model
        // moves it by half the spread — 2% of a ~$2 premium, over the $0.025
        // floor — in whole ticks, on the way in and on the way out.
        let ticks = |premium: f64| {
            (arvo_research::OptionSpread::MEASURED.half_spread(premium) / 0.01 - 1e-9).ceil() * 0.01
        };
        for (tight, measured) in tight.ledger.iter().zip(&measured.ledger) {
            let traded = tight.entry - 0.01;
            assert!(
                (measured.entry - (traded + ticks(traded))).abs() < 1e-9,
                "entry {} from {traded}",
                measured.entry
            );
            if let (Some(a), Some(b)) = (tight.exit, measured.exit) {
                let traded = a + 0.01;
                assert!(
                    (b - (traded - ticks(traded))).abs() < 1e-9,
                    "exit {b} from {traded}"
                );
            }
        }
        let final_equity =
            |result: &SimulationResult| result.equity_curve.last().expect("non-empty").equity;
        assert!(final_equity(&measured) < final_equity(&tight));
    }

    #[test]
    fn a_window_past_a_contracts_expiration_is_refused_until_settlement_exists() {
        // Found by holding one through: the position stayed open for four
        // months after the contract ceased to exist, marked at its last trade.
        let bars = premium_path(200);
        let err = option_run(
            "SPY240315C00100000.AOPT",
            spread(arvo_research::OptionSpread::MEASURED),
            &bars,
        )
        .expect_err("the window runs to July");
        assert!(
            matches!(err, SimulationError::Rejected(ref why) if why.contains("2024-03-15")),
            "{err}"
        );
    }

    #[test]
    fn the_ledger_describes_the_same_run_the_curve_does() {
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);
        let result = provider(bars).run(&experiment).expect("runs");

        assert_eq!(
            result.trades as usize,
            result.ledger.len(),
            "the count is derived from the ledger, so they cannot disagree"
        );
        assert!(!result.ledger.is_empty(), "the fixture is built to trade");

        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);
        assert_eq!(stats.closed + stats.still_open, result.trades);

        // Every closed trade must be a real round trip with both prices and
        // an ordered pair of timestamps. A ledger that reports a fill at zero
        // or an exit before its entry is worse than no ledger.
        for trade in result.ledger.iter().filter(|t| t.closed.is_some()) {
            assert!(trade.entry > 0.0 && trade.quantity > 0.0, "{trade:?}");
            assert!(trade.exit.expect("closed") > 0.0, "{trade:?}");
            assert!(trade.closed.expect("closed") >= trade.opened, "{trade:?}");
        }

        // The ledger is the only place commission is visible at all, and it
        // has to be non-zero at 1 bps or the cost model is not reaching fills.
        assert!(
            stats.total_commission > 0.0,
            "1 bps of commission should have been charged somewhere"
        );

        // Realised P&L must account for the curve. Not exactly — a position
        // still open at the end is marked to market by the curve and not yet
        // realised by the ledger — so the check is that they agree in sign
        // and magnitude once the open one is set aside.
        if stats.still_open == 0 {
            let realised: f64 = result.ledger.iter().map(|trade| trade.pnl).sum();
            let curve = result.equity_curve.last().expect("non-empty").equity
                - experiment.starting_cash;
            assert!(
                (realised - curve).abs() < 1.0,
                "ledger realised {realised} vs curve {curve}"
            );
        }
    }

    /// Rises until a crossover rule is holding, then collapses.
    ///
    /// Engineered rather than realistic, and for a specific reason: it has to
    /// put the account under water *while a position is open*. A rule that
    /// merely loses slowly cannot test this — the per-trade stop closes each
    /// loss long before the account falls far, and a full-size position on a
    /// cash account cannot re-enter until settlement, so the losses never
    /// accumulate. Both of those are real behaviours, and both were found by
    /// writing the obvious fixture first and getting one trade out of it.
    fn collapse(days: usize) -> Vec<arvo_data::Bar> {
        let mut start = date(2024, 1, 1);
        let mut bars = Vec::with_capacity(days);
        for index in 0..days {
            // Three acts, and the first is not decoration. On a monotonic rise
            // the fast average is already above the slow one from the first
            // bar it exists, so a crossover rule never sees a crossing and
            // never enters — which the previous version of this fixture proved
            // by producing a drawdown of exactly zero. So: fall first, so the
            // averages are the right way round to cross.
            let quarter = days / 4;
            let close = if index < quarter {
                150.0 - index as f64 * 0.5
            } else if index < days / 2 {
                150.0 - quarter as f64 * 0.5 + (index - quarter) as f64 * 1.0
            } else {
                // Steeply. The account has to fall past the limit *before*
                // the crossover notices and exits — which is the whole point:
                // this bounds the loss a signal-based exit is too slow to.
                let peak = 150.0 - quarter as f64 * 0.5 + quarter as f64;
                let fallen = (index - days / 2) as f64;
                (peak - fallen * 8.0).max(5.0)
            };
            bars.push(arvo_data::Bar {
                at: start.and_time(NaiveTime::MIN),
                open: close,
                high: close + 0.4,
                low: close - 0.4,
                close,
                volume: 10_000.0,
            });
            start = start.succ_opt().expect("date stays in range");
        }
        bars
    }

    #[test]
    fn a_drawdown_limit_stops_the_run_and_the_ledger_says_so() {
        // A per-trade stop bounds one loss; this bounds their sum, which is
        // the number that actually ends accounts. Twenty consecutive stop-outs
        // at one percent each is a well-behaved rule and a twenty percent hole.
        let bars = collapse(200);
        let mut experiment = experiment(params(5.0, 40.0), &bars);
        // Half the account in one position, so the collapse actually shows up
        // in the *account* rather than only in the instrument. A hundred
        // shares of a hundred-dollar stock is a tenth of the capital, and a
        // limit measured on the account cannot see a fall that small.
        experiment
            .strategy
            .params
            .insert("trade_size".to_owned(), 400.0);
        // No per-trade stop, which is the point: this is the loss a stop does
        // not bound. The position rides the collapse and the *account* is what
        // ends the run.
        experiment.risk = arvo_research::RiskModel {
            stop_atr_multiple: None,
            atr_period: 14,
            risk_per_trade: None,
            max_position_fraction: Some(1.0),
            max_drawdown: Some(0.05),
            max_concurrent_positions: None,
            max_daily_loss: None,
            correlation_cap: None,
            day_trading: arvo_research::DayTradingRule::Unconstrained,
        };

        let result = provider(bars.clone()).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);
        assert!(stats.halted, "a rule that only loses must reach the limit");

        // Exactly one halt, and it is the last thing that happened: the halt
        // is permanent, so nothing may trade after it.
        let halts = result
            .ledger
            .iter()
            .filter(|trade| trade.exit_reason == arvo_research::ExitReason::Halted)
            .count();
        assert_eq!(halts, 1, "the halt fires once and stays fired");
        assert_eq!(
            result.ledger.last().map(|trade| trade.exit_reason),
            Some(arvo_research::ExitReason::Halted),
            "nothing may trade after the account has stopped"
        );

        // And the halt did its job: the drawdown is bounded near the limit
        // rather than running on to whatever the market was going to do.
        let drawdown = arvo_research::Metrics::from_curve(
            &result.equity_curve,
            result.trades,
            arvo_data::BarInterval::DAILY.periods_per_year(),
        )
        .expect("a curve to measure")
        .max_drawdown;
        assert!(
            drawdown < 0.12,
            "a 5% limit should not let a 12% hole open: {drawdown}"
        );
    }

    #[test]
    fn without_a_limit_the_same_run_keeps_trading() {
        // The control for the test above. If the losing fixture stopped early
        // on its own, the halt test would pass while proving nothing.
        let bars = collapse(200);
        let mut experiment = experiment(params(5.0, 40.0), &bars);
        experiment
            .strategy
            .params
            .insert("trade_size".to_owned(), 400.0);
        experiment.risk = arvo_research::RiskModel {
            stop_atr_multiple: None,
            atr_period: 14,
            risk_per_trade: None,
            max_position_fraction: Some(1.0),
            max_drawdown: None,
            max_concurrent_positions: None,
            max_daily_loss: None,
            correlation_cap: None,
            day_trading: arvo_research::DayTradingRule::Unconstrained,
        };

        let result = provider(bars).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);
        assert!(!stats.halted, "nothing to halt against");

        // The claim that makes the halt meaningful: without it, this run draws
        // down further than the limit would have allowed. Drawdown rather than
        // final equity, because the account peaks on the way up and the fall
        // is measured from that peak — a run can end in profit and still have
        // been through a hole nobody would have sat in.
        let drawdown = arvo_research::Metrics::from_curve(
            &result.equity_curve,
            result.trades,
            arvo_data::BarInterval::DAILY.periods_per_year(),
        )
        .expect("a curve to measure")
        .max_drawdown;
        assert!(
            drawdown > 0.05,
            "the control must exceed the limit or the halt proves nothing: {drawdown}"
        );
    }

    #[test]
    fn a_nonsense_drawdown_limit_is_refused_before_the_engine_starts() {
        // Zero halts before the first trade; one or more can never be reached.
        // Both describe a run nobody meant to ask for.
        let bars = sawtooth(50);
        for limit in [0.0, 1.0, 1.5, f64::NAN] {
            let mut experiment = experiment(params(5.0, 10.0), &bars);
            experiment.risk.max_drawdown = Some(limit);
            let err = provider(bars.clone())
                .run(&experiment)
                .expect_err("a limit outside (0, 1) means nothing");
            assert!(matches!(err, SimulationError::Rejected(_)), "{limit}: {err}");
        }
    }

    #[test]
    fn a_stop_exit_is_distinguishable_from_a_signal_exit() {
        // The strategy enforces its own stops with plain market orders, so
        // nothing about the order *type* says why it was sent. Without the
        // tag this reports zero stops on every run, which reads as a fact
        // about the strategy rather than a hole in the instrumentation.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.risk = arvo_research::RiskModel {
            stop_atr_multiple: Some(1.0),
            atr_period: 14,
            ..arvo_research::RiskModel::default()
        };

        let result = provider(bars).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);

        assert_eq!(
            stats.signal_exits + stats.stop_exits,
            stats.closed,
            "every closed trade left for exactly one reason"
        );
        assert!(
            stats.stop_exits > 0,
            "a one-ATR stop on an oscillating fixture must be hit at least once; got {} signal exits and no stops",
            stats.signal_exits
        );
    }

    #[test]
    fn venue_fees_cost_money_that_a_rate_alone_would_not_charge() {
        // The point of the extra shapes: a flat per-fill charge is invisible
        // to a bps model, and on a small position it is the dominant cost.
        let bars = sawtooth(200);
        let free = experiment(params(10.0, 30.0), &bars);
        let mut charged = free.clone();
        charged.costs.per_fill = 5.0;

        let without = provider(bars.clone()).run(&free).expect("runs");
        let with = provider(bars).run(&charged).expect("runs");

        let commission = |result: &SimulationResult| {
            arvo_research::TradeStats::from_ledger(&result.ledger).total_commission
        };
        assert_eq!(
            without.trades, with.trades,
            "a fee should cost money, not change which signals fired"
        );
        // Two fills a round trip at $5 each.
        let expected = 10.0 * f64::from(with.trades);
        let extra = commission(&with) - commission(&without);
        assert!(
            (extra - expected).abs() < 1.0,
            "expected about {expected} of extra fees, got {extra}"
        );
    }

    #[test]
    fn an_order_the_account_cannot_pay_for_is_counted_not_lost() {
        // Buy-and-hold sends its fixed size without asking the gate, so an
        // account too small for it is refused by the venue. Before refusals
        // were counted that run reported no trades and nothing else.
        let bars = sawtooth(60);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.strategy.name = BUY_AND_HOLD.to_owned();
        experiment.starting_cash = 1_000.0;

        let result = provider(bars).run(&experiment).expect("runs");
        assert!(result.ledger.is_empty(), "nothing could be bought");
        assert!(result.refused.entries >= 1, "{:?}", result.refused);
    }

    #[test]
    fn a_sell_side_fee_is_not_charged_on_the_buy() {
        // SEC- and FINRA-shaped charges fall on sales. Charging them both
        // ways would double a round trip's regulatory cost, and nothing in
        // the output would show it.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        // A round number against a ~$100 fixture: 100 bps of sale proceeds.
        experiment.costs.sell_notional_bps = 100.0;

        let result = provider(bars).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);

        // Sell proceeds only, so roughly 1% of one side of each round trip
        // rather than of both. Compared against the two-sided figure, which
        // is what a side-blind implementation would produce.
        let sold: f64 = result
            .ledger
            .iter()
            .filter_map(|trade| Some(trade.exit? * trade.quantity))
            .sum();
        let one_sided = sold * 0.01;
        assert!(
            stats.total_commission < one_sided * 1.5,
            "commission {} looks two-sided against {one_sided} of sale proceeds",
            stats.total_commission
        );
        assert!(
            stats.total_commission > one_sided * 0.5,
            "commission {} is too small to include the sell-side charge",
            stats.total_commission
        );
    }

    #[test]
    fn a_negative_fee_is_refused_rather_than_paying_us_to_trade() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.costs.per_fill = -1.0;

        let err = provider(bars).run(&experiment).expect_err("a fee is not a rebate");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn nonsense_slippage_is_refused_rather_than_run() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.costs.slippage_bps = -2.0;

        let err = provider(bars)
            .run(&experiment)
            .expect_err("negative slippage would pay us to trade");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn a_window_with_no_data_is_distinguished_from_a_bad_one() {
        let bars = sawtooth(50);
        let mut experiment = experiment(params(5.0, 10.0), &bars);
        experiment.window =
            DateRange::new(date(2030, 1, 1), date(2030, 2, 1)).expect("window is ordered");

        let err = provider(bars).run(&experiment).expect_err("no bars there");
        assert!(matches!(err, SimulationError::NoData { .. }), "{err}");
    }

    #[test]
    fn too_little_history_to_fill_the_slow_average_is_rejected() {
        let bars = sawtooth(10);
        let experiment = experiment(params(5.0, 30.0), &bars);

        let err = provider(bars)
            .run(&experiment)
            .expect_err("30-period average cannot fill from 10 bars");
        assert!(matches!(err, SimulationError::Rejected(_)), "{err}");
    }

    #[test]
    fn nonsensical_periods_are_rejected_before_the_engine_starts() {
        let bars = sawtooth(200);

        for (fast, slow, why) in [
            (30.0, 10.0, "fast must be shorter than slow"),
            (0.0, 10.0, "a zero period is not a period"),
            (10.5, 30.0, "a period is a whole number of bars"),
        ] {
            let experiment = experiment(params(fast, slow), &bars);
            let err = provider(bars.clone()).run(&experiment).expect_err(why);
            assert!(matches!(err, SimulationError::Rejected(_)), "{why}: {err}");
        }
    }

    #[test]
    fn a_daily_bar_is_timestamped_at_the_close_of_its_day() {
        let ts = close_of_bar(
            date(2024, 1, 2).and_time(NaiveTime::MIN),
            arvo_data::BarInterval::DAILY,
        )
        .expect("date is representable");
        let expected = date(2024, 1, 3)
            .and_time(NaiveTime::MIN)
            .and_utc()
            .timestamp_nanos_opt()
            .expect("date is representable");
        assert_eq!(
            ts.as_u64(),
            u64::try_from(expected).expect("timestamp is positive"),
            "a close must not be visible before its day has ended"
        );
    }

    #[test]
    fn an_intraday_bar_closes_at_the_end_of_its_own_period() {
        // The look-ahead guard has to scale with the bar, not stay at a day:
        // a five-minute close is knowable five minutes after it opens, and
        // holding it back a whole day would hide a day of information.
        let opens = date(2024, 1, 2).and_hms_opt(14, 30, 0).expect("valid time");
        let ts = close_of_bar(
            opens,
            arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute),
        )
        .expect("representable");

        let expected = opens
            .and_utc()
            .timestamp_nanos_opt()
            .expect("representable")
            + 5 * 60 * 1_000_000_000;
        assert_eq!(
            ts.as_u64(),
            u64::try_from(expected).expect("positive"),
            "a five-minute bar closes five minutes after it opens"
        );
    }


    /// A steadily rising market, so buy-and-hold must show a gain. If the
    /// benchmark comes back flat here, the position is being left open and
    /// unrealised and every comparison drawn against it is worthless.
    fn rising(days: usize) -> Vec<arvo_data::Bar> {
        let mut start = date(2024, 1, 1);
        let mut bars = Vec::with_capacity(days);
        for index in 0..days {
            let close = 100.0 + index as f64 * 0.25;
            bars.push(arvo_data::Bar {
                at: start.and_time(NaiveTime::MIN),
                open: close,
                high: close + 0.5,
                low: close - 0.5,
                close,
                volume: 10_000.0,
            });
            start = start.succ_opt().expect("date stays in range");
        }
        bars
    }

    #[test]
    fn the_benchmark_holds_the_market_rather_than_sitting_in_cash() {
        let bars = rising(120);
        let mut experiment = experiment(params(5.0, 20.0), &bars);
        experiment.strategy = arvo_research::evaluation::benchmark_for(&experiment).strategy;

        let result = provider(bars)
            .run(&experiment)
            .expect("benchmark should run");
        let metrics = arvo_research::Metrics::from_curve(
            &result.equity_curve,
            result.trades,
            arvo_research::evaluation::TRADING_DAYS_PER_YEAR,
        )
        .expect("a held position moves the curve");

        assert!(
            metrics.total_return > 0.0,
            "buy-and-hold in a rising market must gain; got {:?}",
            metrics
        );
    }


    /// The whole dividend path, from a file on disk to a measured gap.
    ///
    /// Every layer of this has unit tests and they would all still pass with
    /// the layers unconnected: the library reads a file nobody asks for, the
    /// engine forwards a call nobody makes, and `Evaluation::dividend_gap` is
    /// `None` forever while `advice` quietly falls back to the old estimate.
    /// This is the only test that fails when the wiring is missing.
    mod dividend_path {
        use super::*;
        use arvo_data::CsvBars;

        /// The fixture written to a real library, because that is the only
        /// provider that reads a distribution series at all.
        fn library(bars: &[arvo_data::Bar], dividends: &[arvo_data::Dividend]) -> tempfile::TempDir {
            let dir = tempfile::tempdir().expect("tempdir");
            let library = CsvBars::new(dir.path());
            library
                .write("AAPL.NASDAQ", arvo_data::BarInterval::DAILY, bars)
                .expect("write bars");
            library
                .write_dividends("AAPL.NASDAQ", dividends)
                .expect("write dividends");
            dir
        }

        fn paid(day_of_year: u32, amount: f64) -> arvo_data::Dividend {
            arvo_data::Dividend {
                ex_date: date(2024, 1, 1) + chrono::Duration::days(i64::from(day_of_year)),
                amount,
            }
        }

        #[test]
        fn a_distribution_series_on_disk_reaches_the_evaluation_as_a_measured_gap() {
            let bars = sawtooth(400);
            let dir = library(&bars, &[paid(50, 0.50), paid(140, 0.50), paid(230, 0.50)]);
            let experiment = experiment(params(10.0, 30.0), &bars);
            let provider = NautilusSimulation::new(CsvBars::new(dir.path()));

            let evidence = arvo_research::evaluate_against_benchmark(
                &provider,
                &experiment,
                &arvo_research::EvaluationCriteria::default(),
            )
            .expect("both runs should complete");

            let gap = evidence
                .evaluation
                .dividend_gap
                .expect("a series is on disk, so the gap is measurable");

            assert_eq!(gap.events, 3, "three payments went ex inside the window");
            assert_eq!(
                (gap.covered, gap.instruments),
                (1, 1),
                "the one instrument this run held has a series"
            );
            assert!(gap.complete());
            assert!(
                gap.benchmark_income > 0.0,
                "buy-and-hold was in the market for every ex-date"
            );
            assert!(
                gap.benchmark_income >= gap.strategy_income,
                "a rule that sits out cannot collect more than one that never does: \
                 benchmark {} against strategy {}",
                gap.benchmark_income,
                gap.strategy_income
            );
            assert!(
                gap.overstatement > 0.0,
                "this rule is out of the market for part of the window, so it must forgo strictly more than buy-and-hold does — a gap of zero here means the measurement is not seeing the ledger: benchmark {} against strategy {}",
                gap.benchmark_income,
                gap.strategy_income
            );
        }

        #[test]
        fn the_basis_the_experiment_declares_reaches_the_gap_that_reads_it() {
            // The gap decides whether it is a correction or a description from
            // the dataset's adjustment, and the only place that is recorded is
            // the experiment. A measurement that read `Split` regardless would
            // pass every other test in this module and subtract dividends from
            // a margin that already contains them.
            let bars = sawtooth(400);
            let dir = library(&bars, &[paid(50, 0.50), paid(140, 0.50)]);
            let mut experiment = experiment(params(10.0, 30.0), &bars);
            experiment.dataset.adjustment = arvo_data::source::Adjustment::TotalReturn;

            let evidence = arvo_research::evaluate_against_benchmark(
                &NautilusSimulation::new(CsvBars::new(dir.path())),
                &experiment,
                &arvo_research::EvaluationCriteria::default(),
            )
            .expect("both runs should complete");

            let gap = evidence
                .evaluation
                .dividend_gap
                .expect("a series is on disk");
            assert_eq!(gap.events, 2, "still measured on a total-return series");
            assert!(!gap.is_a_correction());
            assert_eq!(
                gap.corrected_excess(evidence.evaluation.excess_return),
                None,
                "the distributions are already in the excess return"
            );
        }

        #[test]
        fn a_library_with_no_series_leaves_the_gap_unmeasured_rather_than_zero() {
            // The distinction the whole feature rests on. Zero would claim the
            // bias has been shown not to exist; `None` says nobody looked.
            let bars = sawtooth(400);
            let dir = tempfile::tempdir().expect("tempdir");
            CsvBars::new(dir.path())
                .write("AAPL.NASDAQ", arvo_data::BarInterval::DAILY, &bars)
                .expect("write bars");

            let experiment = experiment(params(10.0, 30.0), &bars);
            let evidence = arvo_research::evaluate_against_benchmark(
                &NautilusSimulation::new(CsvBars::new(dir.path())),
                &experiment,
                &arvo_research::EvaluationCriteria::default(),
            )
            .expect("both runs should complete");

            assert!(evidence.evaluation.dividend_gap.is_none());
        }

        #[test]
        fn an_instrument_that_paid_nothing_is_measured_as_no_bias_at_all() {
            // The good news case, and it must not read as the unmeasured one:
            // the series is there and these instruments pay nothing, so the
            // excess return needs no correction.
            let bars = sawtooth(400);
            let dir = library(&bars, &[]);
            let experiment = experiment(params(10.0, 30.0), &bars);

            let evidence = arvo_research::evaluate_against_benchmark(
                &NautilusSimulation::new(CsvBars::new(dir.path())),
                &experiment,
                &arvo_research::EvaluationCriteria::default(),
            )
            .expect("both runs should complete");

            let gap = evidence
                .evaluation
                .dividend_gap
                .expect("an empty series is still a series");
            assert_eq!(gap.events, 0);
            assert!(gap.overstatement.abs() < 1e-12);
            assert!(!gap.worth_saying());
        }
    }

    #[test]
    fn the_research_loop_runs_end_to_end_and_produces_evidence() {
        let bars = sawtooth(400);
        let experiment = experiment(params(10.0, 30.0), &bars);
        let criteria = arvo_research::EvaluationCriteria::default();

        let evidence =
            arvo_research::evaluate_against_benchmark(&provider(bars), &experiment, &criteria)
                .expect("both runs should complete");

        assert_eq!(evidence.hypothesis, experiment.hypothesis);
        assert_eq!(
            evidence.experiment, experiment,
            "the record pins the whole run"
        );
        assert_eq!(evidence.engine, ENGINE);
        assert_ne!(
            evidence.benchmark, experiment.id,
            "the benchmark is a separate run"
        );
        assert!(
            !evidence.evaluation.reasons.is_empty(),
            "a verdict without a reason is not evidence"
        );
        // The control is not expected to beat the market; what matters is that
        // the loop reached a stated verdict rather than an accident.
        assert!(
            matches!(
                evidence.evaluation.verdict,
                arvo_research::Verdict::NotSupported | arvo_research::Verdict::Inconclusive
            ),
            "a moving-average crossover should not beat buy-and-hold here: {:?}",
            evidence.evaluation
        );
    }

    #[test]
    fn a_family_selects_in_sample_and_is_judged_out_of_sample() {
        use arvo_research::{ExperimentFamily, ParameterGrid};

        let bars = sawtooth(600);
        let template = experiment(params(10.0, 30.0), &bars);
        let family = ExperimentFamily::new(
            template.clone(),
            ParameterGrid::new()
                .axis("fast", vec![5.0, 10.0, 15.0])
                .axis("slow", vec![30.0, 50.0]),
        );

        let found = arvo_research::run_family(
            &provider(bars),
            &family,
            &arvo_research::EvaluationCriteria::default(),
        )
        .expect("the family should run");

        assert_eq!(
            found.selection.trials, 6,
            "every grid point should have run"
        );
        assert!(
            found.in_sample.to < found.out_of_sample.from,
            "the winner must be judged on days it was not chosen on"
        );
        assert_eq!(
            found.selected.window, found.out_of_sample,
            "the reported run is the out-of-sample one"
        );
        assert!(
            found.out_of_sample_evidence.experiment.window == found.out_of_sample,
            "and its evidence agrees"
        );
        assert!(
            !matches!(found.verdict, arvo_research::Verdict::Supported),
            "a moving-average grid should not beat buy-and-hold out of sample: {:?}",
            found.reasons
        );
    }

    #[test]
    fn an_intraday_curve_agrees_with_its_ledger() {
        // The test that was missing. Reconciliation was only checked on daily
        // bars with a small position, where the engine's own returns series
        // was wrong in a way that looked like an ordinary wobble and happened
        // to land on the right endpoint. Put most of the account into one
        // intraday trade and it reported +98% on two losing trades.
        let bars = sessions(10, 40);
        let mut experiment = intraday_experiment(
            OPENING_RANGE,
            BTreeMap::from([
                ("range_bars".to_owned(), 6.0),
                ("target_range_multiple".to_owned(), 2.0),
                ("trade_size".to_owned(), 10.0),
            ]),
            &bars,
        );
        // Risk-sized, so a position is most of the account — the condition
        // that made the old curve absurd rather than merely wrong.
        experiment.risk = arvo_research::RiskModel {
            stop_atr_multiple: Some(2.0),
            atr_period: 14,
            risk_per_trade: Some(0.01),
            max_position_fraction: Some(1.0),
            max_drawdown: None,
            max_concurrent_positions: None,
            max_daily_loss: None,
            correlation_cap: None,
            day_trading: arvo_research::DayTradingRule::Unconstrained,
        };

        let result = intraday_provider(bars.clone()).run(&experiment).expect("runs");
        let stats = arvo_research::TradeStats::from_ledger(&result.ledger);
        assert!(result.trades > 0, "the fixture breaks its range");
        assert_eq!(stats.still_open, 0, "the fixture closes everything");

        let realised: f64 = result.ledger.iter().map(|trade| trade.pnl).sum();
        let moved = result.equity_curve.last().expect("non-empty").equity - experiment.starting_cash;
        assert!(
            (realised - moved).abs() < 1.0,
            "ledger realised {realised} but the curve moved {moved}"
        );
    }

    #[test]
    fn a_curve_has_one_point_per_bar_not_one_per_day() {
        // Metrics annualise by the experiment's own interval. A curve with one
        // point a day, scaled by the five-minute factor, overstated volatility
        // by about nine times — and the number looked like a result.
        let bars = sessions(4, 20);
        let experiment = intraday_experiment(
            VWAP_REVERSION,
            BTreeMap::from([
                ("entry_deviations".to_owned(), 1.0),
                ("trade_size".to_owned(), 10.0),
            ]),
            &bars,
        );

        let result = intraday_provider(bars.clone()).run(&experiment).expect("runs");
        assert_eq!(
            result.equity_curve.len(),
            bars.len() + 1,
            "every bar, plus the opening balance"
        );
    }

    // ---- books: several instruments out of one account -------------------

    /// A provider holding the same sawtooth under several names, so a book's
    /// members are identical by construction and any difference between the
    /// book and a single run is the shared account and nothing else.
    fn book_provider(names: &[&str], bars: &[arvo_data::Bar]) -> NautilusSimulation<InMemoryBars> {
        let mut library = InMemoryBars::new();
        for name in names {
            library = library.with_instrument(name, bars.to_vec());
        }
        NautilusSimulation::new(library)
    }

    /// Every reconciliation invariant, against real engine output.
    ///
    /// The unit tests in `arvo_research::reconcile` prove the checks catch
    /// what they claim to. This proves they do not fire on a correct run —
    /// which is the harder and more important half, and the half this codebase
    /// has got wrong before: the data-quality outlier check reported 159
    /// findings in 5,000 bars of a clean fixture and had to be rewritten.
    ///
    /// Run across every strategy and both cost shapes, because a fee invariant
    /// that holds only for zero costs is not an invariant.
    #[test]
    fn a_real_run_reconciles_with_itself() {
        let bars = sawtooth(200);
        let costs = [
            CostModel::proportional(0.0, 0.0),
            CostModel::proportional(2.0, 5.0),
            CostModel {
                commission_bps: 1.5,
                slippage_bps: 3.0,
                per_fill: 0.65,
                per_unit_sold: 0.000_166,
                sell_notional_bps: 0.278,
                option_spread: None,
            },
        ];

        for (name, params) in [
            (SMA_CROSS, params(10.0, 30.0)),
            (BUY_AND_HOLD, BTreeMap::from([("trade_size".to_owned(), 100.0)])),
        ] {
            for cost in &costs {
                let mut experiment = experiment_named(name, params.clone(), &bars);
                experiment.costs = *cost;

                let result = provider(bars.clone())
                    .run(&experiment)
                    .expect("the backtest should run");

                let found = arvo_research::reconcile(&experiment, &result);
                assert!(
                    found.is_empty(),
                    "{name} at {}bps commission disagreed with itself: {found:#?}",
                    cost.commission_bps,
                );
            }
        }
    }

    #[test]
    fn a_book_reconciles_with_itself_too() {
        // Several instruments settling against one account is where a fee or
        // a curve invariant would most plausibly break, because the ledger now
        // holds trades from more than one price series.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.costs = CostModel::proportional(2.0, 5.0);
        experiment.alongside = vec!["MSFT.NASDAQ".to_owned()];

        let result = book_provider(&["AAPL.NASDAQ", "MSFT.NASDAQ"], &bars)
            .run(&experiment)
            .expect("the book should run");

        let found = arvo_research::reconcile(&experiment, &result);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_books_benchmark_starves_out_of_the_same_account_as_the_strategy() {
        // A book is scored against buy-and-hold of the *same book out of the
        // same account*, not against an equal-weight index of its members. So
        // when the account cannot fund every member, both sides hold fewer
        // than the book names — which keeps the comparison fair and makes it
        // narrower than its title. Both halves are asserted here, because
        // stating the fairness without the narrowness is the misleading half.
        let bars = sawtooth(200);
        let names = ["AAPL.NASDAQ", "MSFT.NASDAQ", "NVDA.NASDAQ"];
        let simulation = book_provider(&names, &bars);

        let held = |result: &arvo_research::SimulationResult| {
            let mut seen: Vec<&str> = result
                .ledger
                .iter()
                .map(|trade| trade.instrument.as_str())
                .collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len()
        };

        let mut roomy = experiment(params(10.0, 30.0), &bars);
        roomy.alongside = names[1..].iter().map(|n| (*n).to_owned()).collect();
        let roomy_bench = arvo_research::evaluation::benchmark_for(&roomy);
        assert_eq!(
            held(&simulation.run(&roomy_bench).expect("runs")),
            3,
            "with room for all three, buy-and-hold holds all three"
        );

        // A tenth of the capital, and the same three names.
        let mut cramped = roomy.clone();
        cramped.starting_cash = 12_000.0;
        let cramped_bench = arvo_research::evaluation::benchmark_for(&cramped);
        assert!(
            held(&simulation.run(&cramped_bench).expect("runs")) < 3,
            "an account with room for one cannot buy and hold three"
        );
    }

    /// Counts how many of a book's instruments were held at the same instant.
    ///
    /// From the ledger's own open/close times rather than from anything the
    /// strategies reported, so it measures what the account actually did.
    fn peak_concurrent(result: &arvo_research::SimulationResult) -> usize {
        let mut edges: Vec<(chrono::NaiveDateTime, i32)> = Vec::new();
        for trade in &result.ledger {
            edges.push((trade.opened, 1));
            if let Some(closed) = trade.closed {
                edges.push((closed, -1));
            }
        }
        // Closes before opens at the same instant: a position that ended on
        // the bar another began did not overlap with it.
        edges.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let (mut live, mut peak) = (0, 0);
        for (_, delta) in edges {
            live += delta;
            peak = peak.max(live);
        }
        usize::try_from(peak).unwrap_or(0)
    }

    #[test]
    fn a_position_cap_binds_across_a_books_separate_strategies() {
        // The control and the cap in one test, because the cap only means
        // something if the uncapped run exceeded it. A book's members are
        // separate strategy instances that know nothing of each other, so a
        // cap counted per strategy would be three caps of one and would bind
        // nothing.
        let bars = sawtooth(200);
        let names = ["AAPL.NASDAQ", "MSFT.NASDAQ", "NVDA.NASDAQ"];
        let simulation = book_provider(&names, &bars);

        let mut uncapped = experiment(params(10.0, 30.0), &bars);
        uncapped.alongside = names[1..].iter().map(|n| (*n).to_owned()).collect();
        let free = simulation.run(&uncapped).expect("runs");
        assert!(
            peak_concurrent(&free) > 1,
            "the fixture has to hold more than one at once for a cap to mean anything"
        );

        let mut capped = uncapped.clone();
        capped.risk.max_concurrent_positions = Some(1);
        let held = simulation.run(&capped).expect("runs");
        assert_eq!(
            peak_concurrent(&held),
            1,
            "a cap of one must hold across every member of the book"
        );
    }

    #[test]
    fn a_cap_does_not_stop_a_rule_managing_what_it_already_holds() {
        // A strategy already holding something is one of the positions being
        // counted. Blocking it would leave a rule unable to stop out or take
        // its target — a cap that traps capital instead of limiting it.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.risk.max_concurrent_positions = Some(1);

        let result = provider(bars).run(&experiment).expect("runs");
        assert!(
            result.trades > 1,
            "a single instrument under a cap of one still trades repeatedly, got {}",
            result.trades
        );
        assert!(
            result.ledger.iter().filter(|t| t.closed.is_some()).count() > 1,
            "positions must still close under a cap"
        );
    }

    #[test]
    fn no_cap_is_the_behaviour_every_recorded_run_had() {
        // `None` has to leave the engine untouched, or every stored finding
        // becomes unreproducible at once.
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);
        assert!(experiment.risk.max_concurrent_positions.is_none());

        let result = provider(bars.clone()).run(&experiment).expect("runs");
        let mut explicit = experiment.clone();
        explicit.risk.max_concurrent_positions = None;
        let again = provider(bars).run(&explicit).expect("runs");

        assert_eq!(result.trades, again.trades);
    }


    /// The correlation cap, end to end through a real backtest.
    ///
    /// Every layer of this has unit tests that would still pass with the
    /// layers unconnected: `RollingCorrelations` correlates a fixture nobody
    /// feeds it, `decide` refuses on a `None` nobody produces, and the engine
    /// passes a source no strategy ever observes into. This is the only test
    /// that fails when the wiring is missing — and the failure it guards
    /// against is *silent*, because an unwired cap refuses every entry and a
    /// backtest reporting zero trades looks like a rule that found no signal.
    mod correlation_cap {
        use super::*;

        fn capped(bars: &[arvo_data::Bar], cap: Option<arvo_research::CorrelationCap>) -> Experiment {
            let mut experiment = experiment(params(10.0, 30.0), bars);
            experiment.alongside = vec!["MSFT.NASDAQ".to_owned()];
            experiment.risk.correlation_cap = cap;
            experiment
        }

        /// Two members fed the identical series: perfectly correlated, so one
        /// bet wearing two names.
        fn identical_book(bars: &[arvo_data::Bar]) -> NautilusSimulation<InMemoryBars> {
            book_provider(&["AAPL.NASDAQ", "MSFT.NASDAQ"], bars)
        }

        fn concurrent_peak(ledger: &[arvo_research::Trade]) -> usize {
            // How many were open at once, at the worst moment.
            let mut events: Vec<(chrono::NaiveDateTime, i32)> = Vec::new();
            for trade in ledger {
                events.push((trade.opened, 1));
                if let Some(closed) = trade.closed {
                    events.push((closed, -1));
                }
            }
            events.sort_by_key(|(at, delta)| (*at, *delta));
            let (mut open, mut peak) = (0, 0);
            for (_, delta) in events {
                open += delta;
                peak = peak.max(open);
            }
            usize::try_from(peak).unwrap_or_default()
        }

        #[test]
        fn a_cap_binds_rather_than_refusing_everything() {
            // The bug this closes: the cap shipped with nothing able to
            // evaluate it, and a cap with no source refuses every entry — so
            // configuring one made a backtest report zero trades with no error
            // anywhere.
            let bars = sawtooth(400);
            let simulation = identical_book(&bars);

            let uncapped = simulation
                .run(&capped(&bars, None))
                .expect("the uncapped book should run");
            assert!(
                concurrent_peak(&uncapped.ledger) >= 2,
                "the control must actually hold both at once, or the cap below \
                 proves nothing"
            );

            let one_bet = simulation
                .run(&capped(
                    &bars,
                    Some(arvo_research::CorrelationCap {
                        above: 0.8,
                        max_positions: 1,
                    }),
                ))
                .expect("the capped book should run");

            assert!(
                !one_bet.ledger.is_empty(),
                "a configured cap must not refuse every entry — that is the \
                 failure this test exists for"
            );
            assert_eq!(
                concurrent_peak(&one_bet.ledger),
                1,
                "two members on identical bars are one bet, so only one may be \
                 held at a time"
            );
        }

        #[test]
        fn a_cap_with_room_for_the_cluster_permits_it() {
            // The other half, and the one that says the cap is *evaluating*
            // rather than blanket-refusing: the same perfectly correlated pair,
            // with room for two, must trade exactly as though no cap were set.
            //
            // Note the threshold is compared with `>=`, so identical series
            // correlating at exactly 1.0 trip a cap of 1.0. There is no "cap
            // nothing can reach" to test with this fixture — `RiskModel::check`
            // rejects a threshold above 1.0 precisely because it could never
            // bind.
            let bars = sawtooth(400);
            let simulation = identical_book(&bars);

            let uncapped = simulation
                .run(&capped(&bars, None))
                .expect("uncapped runs");
            let roomy = simulation
                .run(&capped(
                    &bars,
                    Some(arvo_research::CorrelationCap {
                        above: 0.8,
                        max_positions: 2,
                    }),
                ))
                .expect("a cap with room runs");

            assert_eq!(
                roomy.ledger.len(),
                uncapped.ledger.len(),
                "a cluster inside its limit should change nothing"
            );
            assert!(concurrent_peak(&roomy.ledger) >= 2, "and both may be held");
        }
    }

    /// The pattern-day-trader rule, end to end through a real backtest.
    ///
    /// A backtest that ignores it is backtesting an account nobody can open,
    /// and the failure is silent: the run simply trades more than the law
    /// allows and reports a return drawn from trades that could not happen.
    mod pattern_day_trader {
        use super::*;

        /// Weekday sessions that each rise and then fall back.
        ///
        /// `sessions` alternates its direction per day so neither a breakout
        /// nor a reversion rule gets a one-sided fixture — which means a
        /// long-only crossover enters on every *other* session and can never
        /// breach a three-per-five-days budget. This fixture is one-sided on
        /// purpose: the point is to produce a run that *does* breach, so the
        /// constrained run beside it proves something.
        ///
        /// Weekdays only, because the budget window is counted in business
        /// days and a Saturday session would pack more trades into a window
        /// than the rule ever sees.
        fn daily_round_trips(count: usize, bars_each: usize) -> Vec<arvo_data::Bar> {
            let mut day = date(2024, 1, 2);
            let mut bars = Vec::with_capacity(count * bars_each);
            for _ in 0..count {
                while matches!(
                    chrono::Datelike::weekday(&day),
                    chrono::Weekday::Sat | chrono::Weekday::Sun
                ) {
                    day = day.succ_opt().expect("date stays in range");
                }
                let open = day.and_hms_opt(13, 30, 0).expect("valid");
                for index in 0..bars_each {
                    #[expect(clippy::cast_precision_loss, reason = "short sessions")]
                    let phase = index as f64 / bars_each as f64;
                    // Three up-down cycles inside each session, so a fast
                    // crossover completes several round trips a day. One per
                    // day would leave a daily loss limit nothing to stop: by
                    // the time the loss is realised the session is over.
                    let cycle = (phase * 3.0) % 1.0;
                    let close = 100.0 + if cycle < 0.5 { cycle } else { 1.0 - cycle } * 20.0;
                    bars.push(arvo_data::Bar {
                        at: open + chrono::Duration::minutes(5 * i64::try_from(index).expect("small")),
                        open: close,
                        high: close + 0.2,
                        low: close - 0.2,
                        close,
                        volume: 10_000.0,
                    });
                }
                day = day.succ_opt().expect("date stays in range");
            }
            bars
        }

        fn constrained(bars: &[arvo_data::Bar], cash: f64) -> Experiment {
            // Fast periods on purpose: the rule has to cross often enough to
            // breach the budget, or the constrained run below proves nothing.
            let mut experiment = intraday_experiment(SMA_CROSS, params(2.0, 5.0), bars);
            experiment.starting_cash = cash;
            experiment.risk.day_trading = arvo_research::DayTradingRule::PatternDayTrader;
            experiment
        }

        /// The most day trades in any five-business-day window.
        ///
        /// The rule is a *rolling* count, not a total: thirty sessions may hold
        /// many day trades and still never breach it. Asserting on the total
        /// would be asserting the wrong rule — which is what the first version
        /// of this test did, and it failed against correct behaviour.
        fn worst_window(ledger: &[arvo_research::Trade]) -> usize {
            let dates: Vec<chrono::NaiveDate> = ledger
                .iter()
                .filter_map(|trade| {
                    trade
                        .closed
                        .filter(|closed| closed.date() == trade.opened.date())
                        .map(|closed| closed.date())
                })
                .collect();

            dates
                .iter()
                .map(|end| {
                    let start = arvo_research::risk::business_days_before(
                        *end,
                        arvo_research::PDT_WINDOW_DAYS,
                    );
                    dates.iter().filter(|at| **at >= start && *at <= end).count()
                })
                .max()
                .unwrap_or_default()
        }

        #[test]
        fn a_small_account_is_held_to_its_day_trade_budget() {
            // Intraday bars, so the rule opens and closes inside a session and
            // every round trip counts against the budget.
            // Whole sessions of five-minute bars, so a crossover rule opens
            // and closes inside a day and every round trip counts.
            let bars = daily_round_trips(20, 60);
            let simulation = intraday_provider(bars.clone());

            let unconstrained = simulation
                .run(&{
                    let mut plain = constrained(&bars, 2_000.0);
                    plain.risk.day_trading = arvo_research::DayTradingRule::Unconstrained;
                    plain
                })
                .expect("the unconstrained run works");
            assert!(
                worst_window(&unconstrained.ledger) > arvo_research::PDT_DAY_TRADES,
                "the control must breach the budget or the constraint below proves nothing: worst window held {}",
                worst_window(&unconstrained.ledger)
            );

            let held = simulation
                .run(&constrained(&bars, 2_000.0))
                .expect("the constrained run works");
            assert!(
                worst_window(&held.ledger) <= arvo_research::PDT_DAY_TRADES,
                "a $2,000 margin account may not exceed {} day trades in any five-business-day window, and its worst held {}",
                arvo_research::PDT_DAY_TRADES,
                worst_window(&held.ledger)
            );
        }

        #[test]
        fn the_daily_loss_limit_binds_in_a_backtest_too() {
            // Found while wiring the day-trade budget, and the same root cause:
            // `account_from_positions` read only the live half of the cache, so
            // a closed position's realised loss was invisible. The limit was
            // configured, enforced by the gate, and looking at an empty
            // history — the shape of failure that passes every unit test.
            let bars = daily_round_trips(20, 60);
            let simulation = intraday_provider(bars.clone());

            // Costs heavy enough that every round trip loses, on both runs, so
            // the only difference between them is the limit.
            let losing = |cash: f64| {
                let mut experiment = constrained(&bars, cash);
                experiment.risk.day_trading = arvo_research::DayTradingRule::Unconstrained;
                experiment.costs.commission_bps = 150.0;
                experiment.costs.slippage_bps = 150.0;
                experiment
            };

            let unconstrained = simulation
                .run(&losing(100_000.0))
                .expect("the unconstrained run works");

            let mut limited = losing(100_000.0);
            // Five basis points of the account — fifty dollars. The fixture's
            // worst session loses about ninety, so the limit bites; at a tenth
            // of a percent it did not, and the run was identical, which is the
            // limit correctly declining to bind rather than a broken one.
            limited.risk.max_daily_loss = Some(0.0005);
            let held = simulation.run(&limited).expect("the limited run works");

            assert!(
                held.ledger.len() < unconstrained.ledger.len(),
                "a daily loss limit that binds must cost some trades: {} against {}",
                held.ledger.len(),
                unconstrained.ledger.len()
            );
        }

        #[test]
        fn an_account_above_the_floor_is_not_constrained() {
            // Twenty-five thousand is the line, and above it the rule does not
            // apply at all.
            // Whole sessions of five-minute bars, so a crossover rule opens
            // and closes inside a day and every round trip counts.
            let bars = daily_round_trips(20, 60);
            let simulation = intraday_provider(bars.clone());
            let rich = simulation
                .run(&constrained(&bars, arvo_research::PDT_EQUITY_FLOOR * 2.0))
                .expect("the run works");
            assert!(
                worst_window(&rich.ledger) > arvo_research::PDT_DAY_TRADES,
                "above the floor the budget does not bind"
            );
        }
    }

    #[test]
    fn a_book_trades_every_member_and_the_ledger_says_which() {
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.alongside = vec!["MSFT.NASDAQ".to_owned()];
        let simulation = book_provider(&["AAPL.NASDAQ", "MSFT.NASDAQ"], &bars);

        let result = simulation.run(&experiment).expect("the book should run");

        let instruments: std::collections::BTreeSet<&str> = result
            .ledger
            .iter()
            .map(|trade| trade.instrument.as_str())
            .collect();
        assert!(
            instruments.contains("AAPL.NASDAQ") && instruments.contains("MSFT.NASDAQ"),
            "both members should appear in the ledger, got {instruments:?}"
        );
    }

    #[test]
    fn a_book_with_room_for_everyone_is_the_sum_of_its_members() {
        // The control for the test below. When the account can afford every
        // member at once there is nothing to contend for, and a book of two
        // identical members should be exactly twice one of them. If this ever
        // stops holding, the sharing has introduced an effect of its own and
        // the contention measured below would not be contention.
        let bars = sawtooth(200);
        let alone = experiment(params(10.0, 30.0), &bars);
        let single = provider(bars.clone())
            .run(&alone)
            .expect("the single run should work");

        let mut paired = alone.clone();
        paired.alongside = vec!["MSFT.NASDAQ".to_owned()];
        let book = book_provider(&["AAPL.NASDAQ", "MSFT.NASDAQ"], &bars)
            .run(&paired)
            .expect("the book should run");

        assert!(
            (realised(&book) - 2.0 * realised(&single)).abs() < 1e-6,
            "with room for both, a book of two identical members is twice one: \
             book {:.2}, twice the single run {:.2}",
            realised(&book),
            2.0 * realised(&single),
        );
    }

    #[test]
    fn a_book_too_small_for_all_its_members_gives_the_later_ones_what_is_left() {
        // The measurement a book exists for, and the one no amount of
        // combining separate runs can produce.
        //
        // Each member wants 100 x ~$100 = ~$10,000 and the account holds
        // $12,000. The first to signal takes its full size. The second used to
        // be *denied* — an order for money the account did not have, refused by
        // the venue, and silently absent from the result; that was asserted
        // here as the behaviour. Entries are now sized to the cash on hand, so
        // the second member buys what the remainder affords: it trades, and
        // smaller, and nothing is refused.
        let bars = sawtooth(200);
        let mut alone = experiment(params(10.0, 30.0), &bars);
        alone.starting_cash = 12_000.0;
        let single = provider(bars.clone())
            .run(&alone)
            .expect("the single run should work");

        let mut paired = alone.clone();
        paired.alongside = vec!["MSFT.NASDAQ".to_owned()];
        let book = book_provider(&["AAPL.NASDAQ", "MSFT.NASDAQ"], &bars)
            .run(&paired)
            .expect("the book should run");

        assert!(single.trades > 0, "the fixture has to trade");
        assert_eq!(book.refused, arvo_research::Refused::default(), "nothing asked for money that was not there");

        let size_of = |name: &str| {
            book.ledger
                .iter()
                .filter(|trade| trade.instrument == name)
                .map(|trade| trade.quantity)
                .fold(0.0_f64, f64::max)
        };
        let (first, second) = (size_of("AAPL.NASDAQ"), size_of("MSFT.NASDAQ"));
        assert!(first > 0.0 && second > 0.0, "both members trade: {first} and {second}");
        assert!(
            first.min(second) < 100.0,
            "the later member takes what the remainder affords, not its full size: {first} and {second}"
        );
    }

    fn realised(result: &arvo_research::SimulationResult) -> f64 {
        result.ledger.iter().map(|trade| trade.pnl).sum()
    }

    #[test]
    fn a_single_instrument_run_is_untouched_by_the_book_machinery() {
        // Everything already recorded was produced by the path an empty
        // `alongside` takes, so that path changing would invalidate every
        // stored finding at once.
        let bars = sawtooth(200);
        let experiment = experiment(params(10.0, 30.0), &bars);

        let result = provider(bars).run(&experiment).expect("should run");

        assert!(
            result
                .ledger
                .iter()
                .all(|trade| trade.instrument == "AAPL.NASDAQ"),
            "a single-instrument run trades only its instrument"
        );
        assert!(result.trades > 0, "the fixture is meant to trade");
    }

    #[test]
    fn an_instrument_held_alongside_itself_is_refused() {
        // Never what was meant, and it would double the rule's exposure to one
        // name while reporting the trade count of a diversified book.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.alongside = vec!["AAPL.NASDAQ".to_owned()];

        let err = provider(bars)
            .run(&experiment)
            .expect_err("a duplicate member is not a book");
        assert!(
            err.to_string().contains("appears twice"),
            "the reason should name the problem, got {err}"
        );
    }

    #[test]
    fn a_book_spanning_two_venues_is_refused_rather_than_silently_split() {
        // Nautilus accounts are per venue, so this would be two balances
        // wearing one name — the exact opposite of what a book is for, and it
        // would report contention that never happened.
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.alongside = vec!["MSFT.NYSE".to_owned()];
        let simulation = book_provider(&["AAPL.NASDAQ", "MSFT.NYSE"], &bars);

        let err = simulation
            .run(&experiment)
            .expect_err("a shared account cannot span venues");
        assert!(
            err.to_string().contains("cannot span venues"),
            "got {err}"
        );
    }

    #[test]
    fn a_member_with_no_data_fails_the_whole_book_by_name() {
        let bars = sawtooth(200);
        let mut experiment = experiment(params(10.0, 30.0), &bars);
        experiment.alongside = vec!["NVDA.NASDAQ".to_owned()];
        // Only the head instrument is in the library.
        let simulation = provider(bars);

        let err = simulation
            .run(&experiment)
            .expect_err("a member with no bars is not runnable");
        assert!(
            err.to_string().contains("NVDA.NASDAQ"),
            "the failure should name the member that caused it, got {err}"
        );
    }
}
