//! The rules themselves.
//!
//! Each one is its signal and nothing else — sizing, stops and closing live in
//! [`super::Position`] and [`super::Managed`]. Every rule is long-only, and
//! every one checks its stop before its signal, on the bar's low rather than
//! its close.

use nautilus_common::actor::DataActor;
use nautilus_model::{
    data::{Bar, BarType},
    enums::OrderSide,
    identifiers::InstrumentId,
    types::Quantity,
};
use nautilus_trading::strategy::StrategyCore;

use super::{
    indicator::{Atr, Donchian, Session, SessionVwap, Sma},
    managed_strategy, Managed, Position, Risk, EXIT_SIGNAL,
};

/// Bars of a session that must pass before a VWAP deviation means anything.
///
/// Named rather than a parameter: it is a numerical guard on the statistic,
/// not a knob a strategy should be searched over. Tuning it would be tuning
/// the definition of the indicator rather than the rule.
const MIN_SESSION_BARS: usize = 5;

// ---------------------------------------------------------------- control ---

/// Buys when the fast average crosses above the slow one, sells when it
/// crosses back below.
///
/// The control. Not a good trading idea and not meant to be one — it is a rule
/// nobody disputes, which is the right instrument for testing whether the
/// research loop works.
pub(crate) struct SmaCross {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    atr: Atr,
    fast: Sma,
    slow: Sma,
    /// `None` until both averages have filled, so the first crossing observed
    /// is a real crossing rather than an artefact of starting up.
    previous_fast_above: Option<bool>,
}

impl SmaCross {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        fast_period: usize,
        slow_period: usize,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            position: Position::new(risk, trade_size, correlations),
            atr: Atr::new(risk.model.atr_period),
            fast: Sma::new(fast_period),
            slow: Sma::new(slow_period),
            previous_fast_above: None,
        }
    }
}

managed_strategy!(SmaCross);

impl DataActor for SmaCross {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self.atr.update(high, low, close);

        self.observe_bar(bar);
        if self.exit_on_levels(high, low)?.is_some() || self.halt_if_drawn_down()? {
            // Reset the crossover memory too: after a stop the next entry
            // should need a fresh signal, not the stale one that is still
            // technically in force.
            self.previous_fast_above = None;
            return Ok(());
        }
        if self.position.is_halted() {
            return Ok(());
        }

        let (Some(fast), Some(slow)) = (self.fast.update(close), self.slow.update(close)) else {
            return Ok(());
        };

        let fast_above = fast > slow;
        match self.previous_fast_above.replace(fast_above) {
            Some(false) if fast_above => self.enter_long(close, atr, None).map(|_| ()),
            Some(true) if !fast_above => self.close(EXIT_SIGNAL),
            _ => Ok(()),
        }
    }
}

/// Buys once and holds. The benchmark every other rule is scored against.
pub(crate) struct BuyAndHold {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    entered: bool,
}

impl BuyAndHold {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        starting_cash: f64,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            // Deliberately unstopped: the benchmark is "what the market did",
            // and a stopped benchmark is a strategy.
            position: Position::new(
                Risk {
                    // No stop, no sizing rule, and — the important one — no
                    // halt. The benchmark is "what the market did"; one that
                    // stopped trading partway through would be a strategy, and
                    // every excess return measured against it would be measured
                    // against the wrong thing.
                    model: arvo_research::RiskModel {
                        atr_period: 1,
                        ..arvo_research::RiskModel::default()
                    },
                    // Unused: the benchmark buys its fixed size directly and
                    // never asks the gate, so there is no entry to size.
                    costs: arvo_research::CostModel::proportional(0.0, 0.0),
                    starting_cash,
                },
                trade_size,
                correlations,
            ),
            entered: false,
        }
    }
}

managed_strategy!(BuyAndHold);

impl DataActor for BuyAndHold {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, _bar: &Bar) -> anyhow::Result<()> {
        if self.entered {
            return Ok(());
        }
        self.entered = true;
        let size = self.position.default_size();
        self.send(OrderSide::Buy, size, None)
    }
}

// ------------------------------------------------------------- the rules ---

/// Opening range breakout: the session's first bars set a range, and a break
/// above it is the entry.
///
/// The premise is that overnight news is priced in during the first minutes
/// and whatever direction survives that has the day behind it.
///
/// # Three choices that decide what this actually measures
///
/// **One trade per session.** After an exit the rule sits out until tomorrow.
/// Re-entering on the same range turns one signal into several correlated
/// bets on the same premise, which inflates the trade count — the number the
/// evaluation criteria use to decide whether there is enough evidence.
///
/// **A profit target in units of the range.** The range width is the session's
/// own measure of how much it moves, so a target of `target_range_multiple`
/// times it means the same thing on a quiet day and a violent one.
///
/// **The session exit fills at the next session's open, not at this close.**
/// Nothing in a bar stream says which bar is the last of its session — that
/// needs an exchange calendar. So a position still open when the day changes
/// is closed on the first bar of the next one, which is a real deviation from
/// an intraday-flat ORB and carries genuine overnight risk. A tight target and
/// stop make it the rare case rather than the common one, but it is not
/// nothing, and it is why the ledger's holding periods for this rule will
/// occasionally span a night.
pub(crate) struct OpeningRange {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    atr: Atr,
    session: Session,
    /// Bars forming the opening range.
    range_bars: usize,
    target_range_multiple: f64,
    /// The range so far this session, and how many bars have gone into it.
    range: Option<(f64, f64)>,
    bars_this_session: usize,
    /// Whether this session has already had its one trade.
    traded_today: bool,
}

impl OpeningRange {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        range_bars: usize,
        target_range_multiple: f64,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            position: Position::new(risk, trade_size, correlations),
            atr: Atr::new(risk.model.atr_period),
            session: Session::default(),
            range_bars,
            target_range_multiple,
            range: None,
            bars_this_session: 0,
            traded_today: false,
        }
    }
}

managed_strategy!(OpeningRange);

impl DataActor for OpeningRange {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self.atr.update(high, low, close);

        if self.session.advance(bar.ts_event) {
            // A position carried into a new session is closed on this bar,
            // before the new range starts forming. See the type docs: this
            // fills at the open, not at yesterday's close.
            if self.position.is_open() {
                self.close(EXIT_SIGNAL)?;
            }
            self.range = None;
            self.bars_this_session = 0;
            self.traded_today = false;
        }

        // Risk before signal, always: a rule that has spent its drawdown
        // budget should not be reading its entry condition at all.
        self.observe_bar(bar);
        if self.exit_on_levels(high, low)?.is_some() || self.halt_if_drawn_down()? {
            return Ok(());
        }
        if self.position.is_halted() {
            return Ok(());
        }

        self.bars_this_session += 1;
        if self.bars_this_session <= self.range_bars {
            let (range_high, range_low) = self.range.unwrap_or((high, low));
            self.range = Some((range_high.max(high), range_low.min(low)));
            return Ok(());
        }

        let Some((range_high, range_low)) = self.range else {
            return Ok(());
        };
        if self.traded_today || self.position.is_open() || close <= range_high {
            return Ok(());
        }

        let width = range_high - range_low;
        // A session whose first bars never moved has no range to break out of.
        // Trading it anyway means treating a rounding error as a signal.
        if width <= 0.0 {
            return Ok(());
        }
        let target = close + width * self.target_range_multiple;
        if self.enter_long(close, atr, Some(target))? {
            self.traded_today = true;
        }
        Ok(())
    }
}

/// Volatility breakout: a move of more than `entry_atr_multiple` ATRs above
/// the previous close is the entry.
///
/// Measuring the thrust in ATRs rather than points or percent is the whole
/// idea — it makes "a big move" mean the same thing on a $10 stock and a $500
/// one, and on a calm week and a violent one. A fixed-percentage version of
/// this rule fires constantly in high volatility and never in low, so what it
/// really trades is the volatility regime rather than the breakout.
///
/// The exit is symmetric: a thrust of the same size back down. Symmetric so
/// that the rule has one parameter rather than two, and a two-parameter
/// version would double the search space the deflation check has to survive.
pub(crate) struct VolatilityBreakout {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    atr: Atr,
    entry_atr_multiple: f64,
    /// The close this bar's move is measured against.
    previous_close: Option<f64>,
}

impl VolatilityBreakout {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        entry_atr_multiple: f64,
        atr_period: usize,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            position: Position::new(risk, trade_size, correlations),
            atr: Atr::new(atr_period),
            entry_atr_multiple,
            previous_close: None,
        }
    }
}

managed_strategy!(VolatilityBreakout);

impl DataActor for VolatilityBreakout {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self.atr.update(high, low, close);
        let reference = self.previous_close.replace(close);

        // Risk before signal, always: a rule that has spent its drawdown
        // budget should not be reading its entry condition at all.
        self.observe_bar(bar);
        if self.exit_on_levels(high, low)?.is_some() || self.halt_if_drawn_down()? {
            return Ok(());
        }
        if self.position.is_halted() {
            return Ok(());
        }

        let (Some(reference), Some(atr)) = (reference, atr) else {
            return Ok(());
        };
        let thrust = atr * self.entry_atr_multiple;

        if self.position.is_open() {
            if close < reference - thrust {
                return self.close(EXIT_SIGNAL);
            }
            return Ok(());
        }

        if close > reference + thrust {
            // The ATR used for the stop is this crate's `Position` reading the
            // same indicator, so the entry threshold and the stop distance
            // move together by construction.
            self.enter_long(close, Some(atr), None)?;
        }
        Ok(())
    }
}

/// VWAP reversion: buys when price is stretched below the session's
/// volume-weighted average, exits when it gets back.
///
/// The one mean-reverting rule in this library, and it is here for that reason
/// rather than because it is the best of its kind. Every other rule follows
/// trends; a library of one shape cannot tell you which shape an instrument
/// rewards, and "this strategy worked" is a much weaker claim than "the
/// trend-following ones worked and the reverting one did not".
///
/// Stretch is measured in volume-weighted standard deviations of price around
/// the session VWAP, not in percent, for the same reason the volatility
/// breakout uses ATRs: a fixed percentage is a different rule in a quiet
/// session than in a violent one.
///
/// Long-only makes this asymmetric in a way worth stating: it trades the
/// downside stretch and ignores the upside one, so it is half of the rule a
/// margin account would run.
pub(crate) struct VwapReversion {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    atr: Atr,
    session: Session,
    vwap: SessionVwap,
    entry_deviations: f64,
}

impl VwapReversion {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        entry_deviations: f64,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            position: Position::new(risk, trade_size, correlations),
            atr: Atr::new(risk.model.atr_period),
            session: Session::default(),
            vwap: SessionVwap::default(),
            entry_deviations,
        }
    }
}

managed_strategy!(VwapReversion);

impl DataActor for VwapReversion {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self.atr.update(high, low, close);

        if self.session.advance(bar.ts_event) {
            // The premise is reversion to *today's* average price. Carrying a
            // position into a session whose VWAP has not formed yet would be
            // holding it against a level that no longer exists.
            if self.position.is_open() {
                self.close(EXIT_SIGNAL)?;
            }
            self.vwap.reset();
        }

        // Risk before signal, always: a rule that has spent its drawdown
        // budget should not be reading its entry condition at all.
        self.observe_bar(bar);
        if self.exit_on_levels(high, low)?.is_some() || self.halt_if_drawn_down()? {
            return Ok(());
        }
        if self.position.is_halted() {
            return Ok(());
        }

        self.vwap.push(high, low, close, bar.volume.as_f64());
        let Some(vwap) = self.vwap.value() else {
            return Ok(());
        };

        if self.position.is_open() {
            // Reverted. The target is the VWAP itself, which moves as the
            // session goes on — so it is checked here rather than fixed at
            // entry the way a range target is.
            if close >= vwap {
                return self.close(EXIT_SIGNAL);
            }
            return Ok(());
        }

        let Some(deviation) = self.vwap.deviation(MIN_SESSION_BARS) else {
            return Ok(());
        };
        if close < vwap - deviation * self.entry_deviations {
            self.enter_long(close, atr, Some(vwap))?;
        }
        Ok(())
    }
}

/// Momentum breakout: buys a new `entry_period` high, exits on a new
/// `exit_period` low.
///
/// The long-horizon trend rule, in the Donchian shape the original trend
/// followers used. Its distinguishing feature is that the exit is a *trailing*
/// channel rather than a fixed level, so a winning position is given room to
/// keep running while the exit walks up behind it.
///
/// `exit_period` is normally shorter than `entry_period`: a rule that needs
/// the same evidence to leave as it needed to enter gives most of a trend
/// back before it admits the trend ended.
pub(crate) struct MomentumBreakout {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    atr: Atr,
    entry: Donchian,
    exit: Donchian,
}

impl MomentumBreakout {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        entry_period: usize,
        exit_period: usize,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            position: Position::new(risk, trade_size, correlations),
            atr: Atr::new(risk.model.atr_period),
            entry: Donchian::new(entry_period),
            exit: Donchian::new(exit_period),
        }
    }
}

managed_strategy!(MomentumBreakout);

impl DataActor for MomentumBreakout {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let (high, low, close) = (bar.high.as_f64(), bar.low.as_f64(), bar.close.as_f64());
        let atr = self.atr.update(high, low, close);

        // Read both channels *before* pushing this bar in. A close compared
        // against a channel that already contains this bar's own high can
        // never be a breakout — the rule would silently stop being one.
        let entry_channel = self.entry.channel();
        let exit_channel = self.exit.channel();
        self.entry.push(high, low);
        self.exit.push(high, low);

        // Risk before signal, always: a rule that has spent its drawdown
        // budget should not be reading its entry condition at all.
        self.observe_bar(bar);
        if self.exit_on_levels(high, low)?.is_some() || self.halt_if_drawn_down()? {
            return Ok(());
        }
        if self.position.is_halted() {
            return Ok(());
        }

        if self.position.is_open() {
            if let Some((_, trailing_low)) = exit_channel {
                if close < trailing_low {
                    return self.close(EXIT_SIGNAL);
                }
            }
            return Ok(());
        }

        if let Some((breakout_high, _)) = entry_channel {
            if close > breakout_high {
                self.enter_long(close, atr, None)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // The rules themselves are exercised end to end through the engine in
    // `crate::tests`, because what matters about them is what Nautilus does
    // with the orders — sizing, fills, stops and the ledger they produce.
    // Their pure parts live in `super::indicator` and are tested there.
    //
    // What is worth pinning here is the constant the ledger contract depends
    // on, since it is a string shared across two files.
    use super::super::{EXIT_SIGNAL, EXIT_STOP};

    #[test]
    fn exit_tags_are_distinct_and_namespaced() {
        assert_ne!(EXIT_STOP, EXIT_SIGNAL);
        assert!(EXIT_STOP.starts_with("arvo:"));
        assert!(EXIT_SIGNAL.starts_with("arvo:"));
    }
}
