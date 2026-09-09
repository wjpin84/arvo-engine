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

use super::{indicator::Momentum, Position, Risk, EXIT_SIGNAL};

/// Holds the best few of a set, by return over a lookback.
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
    /// The most recent score and close per instrument, as of the timestamp
    /// being filled in.
    latest: HashMap<InstrumentId, (f64, f64)>,
    /// The timestamp currently arriving. The rebalance fires when this changes.
    filling: Option<UnixNanos>,
    hold_top: usize,
}

impl CrossSectionalMomentum {
    pub(crate) fn new(
        core: StrategyCore,
        bar_types: Vec<nautilus_model::data::BarType>,
        trade_size: Quantity,
        lookback: usize,
        hold_top: usize,
        risk: Risk,
    ) -> Self {
        // At least one, or the rule holds nothing and reports it as a finding
        // about momentum rather than about its own configuration.
        let hold_top = hold_top.max(1);

        // Room for all of them at once. See the module note on sizing.
        #[expect(clippy::cast_precision_loss, reason = "holding counts are small")]
        let shared = Risk {
            max_position_value: risk
                .max_position_value
                .map(|cap| cap / hold_top as f64),
            ..risk
        };

        let mut scores = BTreeMap::new();
        let mut positions = BTreeMap::new();
        for bar_type in &bar_types {
            let id = bar_type.instrument_id();
            scores.insert(id, Momentum::new(lookback));
            positions.insert(id, Position::new(shared, trade_size));
        }

        Self {
            core,
            bar_types,
            scores,
            positions,
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
            .map(|(id, (score, _))| (*id, *score))
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
            let Some((_, close)) = self.latest.get(&id).copied() else {
                continue;
            };
            let Some(position) = self.positions.get_mut(&id) else {
                continue;
            };
            // No ATR here: this rule's exit is the ranking, not a stop, and a
            // stop distance would be a second exit nobody asked for.
            let Some((size, _)) = position.plan(close, None) else {
                continue;
            };
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
        let intended = self.positions.get_mut(&id).and_then(Position::release);
        let actual = self.portfolio().net_position(&id);
        let size = match f64::try_from(actual).ok().filter(|held| *held > 0.0) {
            Some(held) => Quantity::new_checked(held, 0).ok(),
            None => intended,
        };
        let Some(size) = size else {
            return Ok(());
        };
        self.send(id, OrderSide::Sell, size, Some(EXIT_SIGNAL))
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

        let id = bar.bar_type.instrument_id();
        let close = bar.close.as_f64();
        if let Some(score) = self.scores.get_mut(&id).and_then(|m| m.update(close)) {
            self.latest.insert(id, (score, close));
        }
        Ok(())
    }
}
