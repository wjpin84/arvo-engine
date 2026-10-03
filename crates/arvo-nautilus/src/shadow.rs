//! The same rule, one bar at a time, for a session that trades somewhere real.
//!
//! # Why a shadow rather than a second signal engine
//!
//! Every rule is a Nautilus strategy (`strategy::rules`), and a live session
//! needs its signals. Re-implementing each rule as a function over a window of
//! bars would be a second copy of the thing under test — and ADR-0009's
//! argument against a second risk engine applies to a second signal engine
//! word for word: every stored verdict would describe a system that does not
//! trade.
//!
//! So the backtest engine itself runs beside the session. It is built exactly
//! as a backtest is (`backtest::build`), warmed over the history the person
//! chose, and then fed each bar as it arrives. The orders the rule sends are
//! filled by the simulated venue — so the rule's own position and stop carry
//! on as they would in a backtest — and reported here as [`Signal`]s for the
//! session to put to its gate and its real venue. The shadow's fills and the
//! venue's fills will differ; that gap is what paper trading measures
//! (`arvo_execution::Divergence`), not a defect.
//!
//! # What crosses
//!
//! A signal is an Arvo value: instrument name, side, size, the price the rule
//! decided at, the stop it was sized against, and — for an exit — why. No
//! order, no position, no Nautilus type.

use std::collections::{BTreeMap, HashSet};

use arvo_research::{Experiment, SimulationError};
use nautilus_backtest::engine::BacktestEngine;
use nautilus_model::{
    data::{BarType, Data},
    enums::OrderSide,
    identifiers::ClientOrderId,
    orders::Order as _,
};

use crate::backtest;
use crate::chain::Settlement;
use crate::convert::to_nautilus_bar;
use crate::plan::Plan;
use crate::strategy::{ENTRY_LEVEL, ENTRY_REGIME, ENTRY_RULE, ENTRY_SIGNAL, ENTRY_STOP};

/// Which way a signal goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// One order the rule sent, as a proposal for the real venue.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    /// The Arvo instrument name the rule trades, e.g. `MSFT.RH`.
    pub instrument: String,
    pub side: Side,
    /// What the rule asked for, already sized by the shared policy against
    /// the shadow's account. A live gate sizes it again against the real one.
    pub quantity: f64,
    /// The close of the bar the rule decided on.
    pub reference_price: f64,
    /// The distance the entry was sized against, when the model stops.
    pub stop_distance: Option<f64>,
    /// The bar's instant: when the signal was generated, not when it was read.
    pub signalled_at: chrono::NaiveDateTime,
    /// `Some(why)` when this closes a position — `stop`, `signal`, `halt` —
    /// which a session sends without asking the gate (ADR-0009).
    pub exit: Option<String>,
    /// The condition that fired, in the rule's words, on an entry (#190).
    pub rule: Option<String>,
    /// The value it was judged on.
    pub signal: Option<f64>,
    /// The regime the rule saw the instrument in, when it had seen enough.
    pub regime: Option<String>,
    /// The prices the rule decided against, by name, on an entry.
    pub levels: Vec<(String, f64)>,
}

/// A backtest engine kept alive and fed one bar at a time.
///
/// Bound to the thread that built it: Nautilus's message bus is thread-local
/// (ADR-0001), so a session owns one of these on a thread of its own.
pub struct Shadow {
    engine: BacktestEngine,
    /// The bar type each instrument's bars must be stamped with, by name.
    bar_types: BTreeMap<String, BarType>,
    interval: arvo_data::BarInterval,
    /// Every order seen so far, so a push reports only what it caused.
    seen: HashSet<ClientOrderId>,
}

impl std::fmt::Debug for Shadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shadow")
            .field("instruments", &self.bar_types.keys().collect::<Vec<_>>())
            .field("orders", &self.seen.len())
            .finish_non_exhaustive()
    }
}

impl Shadow {
    /// Builds the engine as a backtest would and runs it over `book`, which is
    /// the history the rule warms up on. Orders sent during warm-up are the
    /// backtest's, not the session's, and are not reported.
    pub(crate) fn start(
        experiment: &Experiment,
        plan: &Plan,
        book: &[(nautilus_model::identifiers::InstrumentId, String, Vec<arvo_data::Bar>)],
        settlement: Option<&Settlement>,
    ) -> Result<Self, SimulationError> {
        let built = backtest::build(experiment, plan, book, settlement)?;
        let bar_types = book
            .iter()
            .map(|(_, name, _)| name.clone())
            .zip(built.bar_types.iter().copied())
            .collect();
        let mut shadow = Self {
            engine: built.engine,
            bar_types,
            interval: experiment.interval,
            seen: HashSet::new(),
        };
        shadow.step(Some(experiment.id.to_string()))?;
        shadow.seen = shadow.order_ids();
        Ok(shadow)
    }

    /// Feeds one instant's bars — every instrument that has one at that
    /// instant, together — and reports what the rule sent in response.
    ///
    /// # Errors
    ///
    /// A bar for an instrument the run does not hold, a bar the engine cannot
    /// represent, or an engine failure.
    pub fn push(&mut self, bars: &[(String, arvo_data::Bar)]) -> Result<Vec<Signal>, SimulationError> {
        let mut data = Vec::with_capacity(bars.len());
        for (name, bar) in bars {
            let bar_type = *self.bar_types.get(name).ok_or_else(|| {
                SimulationError::Rejected(format!("{name} is not in this run"))
            })?;
            data.push(Data::Bar(to_nautilus_bar(
                bar_type,
                bar,
                self.interval,
                crate::convert::Precision::named(&bar_type.instrument_id().to_string()),
            )?));
        }
        if data.is_empty() {
            return Ok(Vec::new());
        }
        self.engine.clear_data();
        self.engine
            .add_data(data, None, true, true)
            .map_err(|err| SimulationError::Rejected(format!("adding bars: {err}")))?;
        self.step(None)?;

        // The close each rule decided on, by instrument, for the reference
        // price: the order itself carries no price, being a market order.
        let closes: BTreeMap<&str, (f64, chrono::NaiveDateTime)> = bars
            .iter()
            .map(|(name, bar)| (name.as_str(), (bar.close, bar.at)))
            .collect();

        let cache = self.engine.kernel_mut().cache.borrow();
        let mut signals = Vec::new();
        for order in cache.orders(None, None, None, None, None) {
            if !self.seen.insert(order.client_order_id()) {
                continue;
            }
            let instrument = order.instrument_id().to_string();
            let Some(&(reference_price, signalled_at)) = closes.get(instrument.as_str()) else {
                // Sent on a bar this push did not carry: a settlement print or
                // a timer. Not a decision a session can act on at a price.
                continue;
            };
            let tags = order.tags().unwrap_or_default();
            let exit = tags
                .iter()
                .find_map(|tag| tag.as_str().strip_prefix("arvo:exit="))
                .map(str::to_owned);
            let tagged = |prefix: &str| tags.iter().find_map(|tag| tag.as_str().strip_prefix(prefix).map(str::to_owned));
            let stop_distance = tagged(ENTRY_STOP).and_then(|distance| distance.parse().ok());
            let rule = tagged(ENTRY_RULE);
            let value = tagged(ENTRY_SIGNAL).and_then(|value| value.parse().ok());
            let regime = tagged(ENTRY_REGIME);
            let levels = tags
                .iter()
                .filter_map(|tag| tag.as_str().strip_prefix(ENTRY_LEVEL))
                .filter_map(|level| level.split_once('='))
                .filter_map(|(name, price)| Some((name.to_owned(), price.parse().ok()?)))
                .collect();
            signals.push(Signal {
                instrument,
                side: match order.order_side() {
                    OrderSide::Buy => Side::Buy,
                    _ => Side::Sell,
                },
                quantity: order.quantity().as_f64(),
                reference_price,
                stop_distance,
                signalled_at,
                exit,
                rule,
                signal: value,
                regime,
                levels,
            });
        }
        Ok(signals)
    }

    fn step(&mut self, run_id: Option<String>) -> Result<(), SimulationError> {
        self.engine
            .run(None, None, run_id, true)
            .map_err(|err| SimulationError::Engine(Box::new(ShadowFailed(err.to_string()))))
    }

    fn order_ids(&mut self) -> HashSet<ClientOrderId> {
        self.engine
            .kernel_mut()
            .cache
            .borrow()
            .orders(None, None, None, None, None)
            .into_iter()
            .map(|order| order.client_order_id())
            .collect()
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        // Finalises the streaming run and tears the kernel down; a shadow
        // that stops is a backtest that ended.
        let _ = self.engine.end();
        self.engine.dispose();
    }
}

/// Wraps an engine failure so it crosses the boundary as a plain error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ShadowFailed(String);
