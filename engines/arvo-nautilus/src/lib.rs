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

mod backtest;
mod chain;
mod convert;
mod fee;
mod fill;
mod ledger;
mod plan;
mod strategy;

use std::str::FromStr;

use arvo_data::BarProvider;
use arvo_research::{
    Experiment, SimulationError, SimulationProvider, SimulationResult, StrategySpec,
};
use nautilus_model::identifiers::InstrumentId;

use backtest::run_backtest;
use plan::Plan;

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

#[cfg(test)]
mod tests;
