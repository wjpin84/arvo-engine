//! The first rule here that looks at more than one instrument.
//!
//! Every other strategy in [`super::rules`] asks a question of one price
//! series: is this above its average, has this broken its range, is this far
//! from its session's mean. Whether the instrument is the *best available* is a
//! question none of them can state, because none of them can see another one.
//!
//! This one ranks. At every bar it scores each instrument on its return over a
//! lookback, holds the top few, and sells everything else. That is a different
//! shape of idea rather than another parameterisation of the same shape, and it
//! is the family the book work — several instruments, one account — was the
//! prerequisite for.
//!
//! # Deciding on a complete picture
//!
//! Ranking needs every instrument's number *from the same instant*. Bars arrive
//! one at a time, so acting on each arrival would rank a fresh number against
//! stale ones and, worse, would rank instruments in whatever order the engine
//! happened to deliver them.
//!
//! So nothing is decided while a timestamp is being filled in. The rebalance
//! runs when the *next* timestamp's first bar arrives, on the set that
//! completed — which uses only closed bars, in a fixed order, and never sees a
//! price before it has printed. An instrument that does not report at some
//! instant simply keeps its last score, which is what happens to a holding on a
//! day its exchange is shut.
//!
//! # Sizing
//!
//! Each position is capped at the account's per-position limit divided by the
//! number held, so a rule told to hold three does not size each as though it
//! were the only one. Without that the first two fills would take the account
//! and the third would be refused — the crowding-out a book makes visible, here
//! caused by the rule rather than discovered by it.

use std::collections::{BTreeMap, HashMap};

use nautilus_common::actor::data_actor::DataActor;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::Bar,
    enums::OrderSide,
    identifiers::InstrumentId,
    types::Quantity,
};
use nautilus_trading::strategy::{Strategy, StrategyCore};

use super::{
    indicator::{Atr, Momentum},
    Position, Risk, EXIT_SIGNAL, EXIT_STOP,
};

/// Holds the best few of a set, by return over a lookback.
///
/// # The stop is the record's, not this rule's
///
/// The ranking is the exit: an instrument that falls out of the top few is
/// sold whatever it is doing. That made a stop look redundant, so the first
/// version of this passed no ATR at all.
///
/// It was wrong, and not in the harmless direction. `Position::plan` refuses
/// to size at all when a stop is configured and no ATR is supplied — the
/// deliberate refusal that stops a rule trading unprotected while its
/// indicator warms up. Every configuration in the search declared a stop, so
/// every one of them bought nothing, produced a flat curve, and was reported
/// as six failures of the idea rather than one failure of this file.
///
/// The risk model is pinned in the experiment record and the record is the
/// claim about what ran. A rule that quietly ignored the stop would make the
/// record a lie; a rule that refuses because of it makes every run mean
/// nothing. So it honours it: sized against the stop distance, closed when a
/// bar trades through it.
pub(crate) struct CrossSectionalMomentum {
    core: StrategyCore,
    bar_types: Vec<nautilus_model::data::BarType>,
    /// One score and one position per instrument.
    ///
    /// `BTreeMap` rather than `HashMap`: the ranking breaks ties by iteration
    /// order, and a hash map's order is neither stable across runs nor part of
    /// anything this crate can pin. A run that reordered its own ties would
    /// fail its own replay check.
    scores: BTreeMap<InstrumentId, Momentum>,
    positions: BTreeMap<InstrumentId, Position>,
    atrs: BTreeMap<InstrumentId, Atr>,
    /// Pairwise correlation over the bars this run has already seen, shared
    /// with every other strategy in the run. See `super::Position`.
    correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    /// The most recent score, close and ATR per instrument, as of the
    /// timestamp being filled in.
    ///
    /// The ATR travels with the close rather than being read at rebalance
    /// time, so the distance a position is sized against is the one measured
    /// at the instant it was ranked.
    latest: HashMap<InstrumentId, Reading>,
    /// The timestamp currently arriving. The rebalance fires when this changes.
    filling: Option<UnixNanos>,
    hold_top: usize,
}

/// One instrument's state as of a completed timestamp.
#[derive(Clone, Copy)]
struct Reading {
    score: f64,
    close: f64,
    /// `None` until the ATR has warmed. Sizing refuses rather than guesses.
    atr: Option<f64>,
}

impl CrossSectionalMomentum {
    pub(crate) fn new(
        core: StrategyCore,
        bar_types: Vec<nautilus_model::data::BarType>,
        trade_size: Quantity,
        lookback: usize,
        hold_top: usize,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        // At least one, or the rule holds nothing and reports it as a finding
        // about momentum rather than about its own configuration.
        let hold_top = hold_top.max(1);

        // Room for all of them at once. See the module note on sizing.
        #[expect(clippy::cast_precision_loss, reason = "holding counts are small")]
        let shared = Risk {
            model: arvo_research::RiskModel {
                // Room for all of them at once. See the module note on sizing:
                // a ranking rule holding the top N out of one account must size
                // each to a fraction of the ceiling, or the first fills and the
                // rest are rejected for want of cash.
                max_position_fraction: risk
                    .model
                    .max_position_fraction
                    .map(|fraction| fraction / hold_top as f64),
                ..risk.model
            },
            ..risk
        };

        let mut scores = BTreeMap::new();
        let mut positions = BTreeMap::new();
        let mut atrs = BTreeMap::new();
        for bar_type in &bar_types {
            let id = bar_type.instrument_id();
            scores.insert(id, Momentum::new(lookback));
            positions.insert(id, Position::new(shared, trade_size, correlations.clone()));
            atrs.insert(id, Atr::new(risk.model.atr_period));
        }

        Self {
            core,
            bar_types,
            scores,
            positions,
            atrs,
            correlations,
            latest: HashMap::new(),
            filling: None,
            hold_top,
        }
    }

    /// The instruments this would hold, best first.
    ///
    /// Pure, and separate from acting on it, because the ranking is the part
    /// worth testing and an engine is a poor place to test anything.
    fn wanted(&self) -> Vec<InstrumentId> {
        let mut ranked: Vec<(InstrumentId, f64)> = self
            .latest
            .iter()
            .map(|(id, reading)| (*id, reading.score))
            .collect();
        // By score, then by instrument, so a tie resolves the same way twice.
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
            .into_iter()
            // Only instruments actually going up. "The best of a falling set"
            // is still a falling set, and a long-only rule that holds it has
            // confused ranking with an opinion.
            .filter(|(_, score)| *score > 0.0)
            .take(self.hold_top)
            .map(|(id, _)| id)
            .collect()
    }

    /// Sells what is no longer wanted, then buys what is.
    ///
    /// Sells first, deliberately: the proceeds are what pays for the buys, and
    /// on a cash account a rule that bought first would be refused for funds it
    /// was about to have.
    fn rebalance(&mut self) -> anyhow::Result<()> {
        let wanted = self.wanted();

        // The account, once, before any of the entries below. Read from the
        // engine so the limits see every member of the book rather than one
        // instrument's private tally.
        let Some(now) = self.filling.and_then(super::nanos_to_instant) else {
            return Ok(());
        };
        let (positions, realised_today) = super::account_from_positions(
            self.cache().positions(None, None, None, None, None),
            now.date(),
        );

        let held: Vec<InstrumentId> = self
            .positions
            .iter()
            .filter(|(_, position)| position.is_open())
            .map(|(id, _)| *id)
            .collect();
        for id in held {
            if !wanted.contains(&id) {
                self.close(id)?;
            }
        }

        for id in wanted {
            if self.positions.get(&id).is_some_and(Position::is_open) {
                continue;
            }
            let Some(reading) = self.latest.get(&id).copied() else {
                continue;
            };
            let Some(position) = self.positions.get_mut(&id) else {
                continue;
            };
            // Sized against the stop distance the record asked for, and
            // refused outright when that distance is not measurable yet. See
            // the type note: passing `None` here bought nothing, ever.
            let Ok(stop_distance) = position.stop_distance(reading.atr) else {
                continue;
            };
            // The same policy a live session runs, over the same account. A
            // ranking rule holds N positions out of one balance, so the daily
            // loss limit and the position cap have to see all of them — which
            // they do, because the account is read from the engine rather than
            // tallied per instrument.
            let decision = super::decide_entry(
                position.risk(),
                position.default_size(),
                &id.to_string(),
                reading.close,
                stop_distance,
                now,
                &positions,
                realised_today,
                false,
                Some(self.correlations.as_ref()),
            );
            let arvo_research::Decision::Accept { quantity } = decision else {
                continue;
            };
            let Ok(size) = Quantity::new_checked(quantity, 0) else {
                continue;
            };
            position.stop = stop_distance.map(|distance| reading.close - distance);
            position.hold(size);
            self.send(id, OrderSide::Buy, size, None)?;
        }
        Ok(())
    }

    /// Closes whatever is held in one instrument.
    ///
    /// Asks the engine what the position actually is rather than trusting what
    /// this asked for, for the reason [`super::Managed::close`] gives: the two
    /// diverge whenever an order is rejected or partly filled, and selling the
    /// intended size would leave a remainder or flip short.
    fn close(&mut self, id: InstrumentId) -> anyhow::Result<()> {
        self.close_tagged(id, EXIT_SIGNAL)
    }

    /// As [`Self::close`], saying why, so a stop-out is distinguishable from a
    /// sale in the ledger.
    fn close_tagged(&mut self, id: InstrumentId, reason: &'static str) -> anyhow::Result<()> {
        let intended = self.positions.get_mut(&id).and_then(Position::release);
        let actual = self.portfolio().net_position(&id);
        let size = match f64::try_from(actual).ok().filter(|held| *held > 0.0) {
            Some(held) => Quantity::new_checked(held, 0).ok(),
            None => intended,
        };
        let Some(size) = size else {
            return Ok(());
        };
        self.send(id, OrderSide::Sell, size, Some(reason))
    }

    /// ponytail: the order mechanics `super::Managed` already has, taking an
    /// instrument. Unify the two when a second multi-instrument rule appears;
    /// generalising a trait six single-instrument strategies implement to serve
    /// one new caller is the larger change, and the sizing that actually
    /// matters is shared already through `Position::plan`.
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

nautilus_trading::nautilus_strategy!(CrossSectionalMomentum);

impl std::fmt::Debug for CrossSectionalMomentum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossSectionalMomentum")
            .field("instruments", &self.positions.len())
            .field("hold_top", &self.hold_top)
            .finish_non_exhaustive()
    }
}

impl DataActor for CrossSectionalMomentum {
    fn on_start(&mut self) -> anyhow::Result<()> {
        for bar_type in self.bar_types.clone() {
            self.subscribe_bars(bar_type, None, None);
        }
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        for bar_type in self.bar_types.clone() {
            self.unsubscribe_bars(bar_type, None, None);
        }
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        // A new timestamp means the previous one is complete. Acting here uses
        // only bars that have closed, and in an order that does not depend on
        // which instrument the engine delivered first.
        if self.filling.is_some_and(|filling| filling != bar.ts_event) {
            self.rebalance()?;
        }
        self.filling = Some(bar.ts_event);

        // The ranking rule is the one place a correlation estimate has every
        // member's bars in hand, so feeding it here is what makes a cap over a
        // book evaluable at all.
        if let Some(at) = super::nanos_to_instant(bar.ts_event) {
            self.correlations.observe(
                &bar.bar_type.instrument_id().to_string(),
                at,
                bar.close.as_f64(),
            );
        }

        let id = bar.bar_type.instrument_id();
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self
            .atrs
            .get_mut(&id)
            .and_then(|atr| atr.update(high, low, close));
        if let Some(score) = self.scores.get_mut(&id).and_then(|m| m.update(close)) {
            self.latest.insert(id, Reading { score, close, atr });
        }

        // Against this bar's low, on this bar, for the reason
        // `Position::stopped_out` gives: a position that traded through its
        // stop mid-bar did not survive to the close.
        //
        // A stopped-out instrument that is still top-ranked is bought again at
        // the next rebalance. That is what the ranking means — the stop
        // bounds one trade, it does not express an opinion about the name —
        // and a cooldown would be a third rule nobody specified.
        if self
            .positions
            .get(&id)
            .is_some_and(|position| position.is_open() && position.stopped_out(low))
        {
            self.close_tagged(id, EXIT_STOP)?;
        }
        Ok(())
    }
}
