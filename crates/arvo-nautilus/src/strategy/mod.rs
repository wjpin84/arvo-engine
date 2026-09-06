//! Strategies Arvo can ask Nautilus to run.
//!
//! # Why there are six
//!
//! [`SmaCross`] is the control: a moving-average crossover is not a good
//! trading idea, it is a good *instrument* for testing whether the research
//! loop works. [`BuyAndHold`] exists so evaluation has something to score
//! against — absolute return mostly measures whether the market went up, so a
//! result with no benchmark is not a result.
//!
//! The other four are rules people actually run, and they are here because the
//! evaluation machinery is the point of this platform and had almost nothing
//! to judge. Each is a distinct *shape* of edge rather than a variation on one:
//!
//! * [`OpeningRange`] — the day's first minutes set a range; trade the break.
//! * [`VolatilityBreakout`] — a thrust measured in ATRs, not points, so the
//!   same rule means the same thing on a $10 stock and a $500 one.
//! * [`VwapReversion`] — stretch away from the session's average price is
//!   expected to close. The only mean-reverting rule here; the others are all
//!   trend-following, and a library of one shape cannot tell you which shape
//!   an instrument rewards.
//! * [`MomentumBreakout`] — Donchian channel break with a trailing channel
//!   exit. The classic long-horizon trend rule.
//!
//! Every one of them is **long-only**. The venue is a cash account, so there is
//! nothing to borrow and no short to model, and a rule that quietly assumed
//! otherwise would produce a curve that could not have been traded.
//!
//! # Session-anchored rules refuse daily bars
//!
//! [`OpeningRange`] and [`VwapReversion`] are defined against a trading
//! session. On daily bars a session is one bar: the opening range is the whole
//! day and a session VWAP is that day's typical price. Both would still *run*
//! and produce numbers. Refusing them is the point — see
//! [`crate::Plan::from_spec`].
//!
//! A strategy here is a Nautilus component, which is why it lives on this side
//! of the boundary. `arvo-research` names it by string in `StrategySpec` and
//! never sees the type.

mod indicator;
mod rules;

use nautilus_model::{enums::OrderSide, identifiers::InstrumentId, types::Quantity};
use nautilus_trading::strategy::{Strategy, StrategyNative};

pub(crate) use rules::{
    BuyAndHold, MomentumBreakout, OpeningRange, SmaCross, VolatilityBreakout, VwapReversion,
};

/// Tags stamped on a closing order to say why it was sent.
///
/// Read back by [`crate::ledger`], which is the other half of this contract:
/// change a spelling here and the ledger silently reclassifies every exit.
pub(crate) const EXIT_STOP: &str = "arvo:exit=stop";
pub(crate) const EXIT_SIGNAL: &str = "arvo:exit=signal";

/// What a strategy does to protect a position, resolved from the experiment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Risk {
    pub(crate) stop_atr_multiple: Option<f64>,
    pub(crate) atr_period: usize,
    /// Capital-at-risk per trade, already in currency rather than a fraction.
    pub(crate) risk_amount: Option<f64>,
    /// The most one position may be worth, in currency.
    pub(crate) max_position_value: Option<f64>,
}

/// The long position a strategy is managing, and the levels around it.
///
/// Every rule in [`rules`] needs the same four things — size the entry, place
/// a stop, notice the stop was hit, close what is *actually* held — and none
/// of that is what distinguishes one rule from another. Extracted so a new
/// strategy is its signal and nothing else; the alternative was six copies of
/// the sizing logic, five of which would eventually disagree with the first.
#[derive(Debug)]
pub(crate) struct Position {
    risk: Risk,
    /// Quantity to trade when no risk-based sizing applies.
    default_size: Quantity,
    /// Where this position gets out if it goes wrong. `None` when flat, or
    /// when no stop was configured.
    stop: Option<f64>,
    /// Where it gets out if it goes right. `None` unless the rule sets one.
    target: Option<f64>,
    /// What is actually held, so an exit closes the position rather than a
    /// default quantity.
    ///
    /// Once sizing varies per trade these are no longer the same number, and
    /// exiting with the default silently leaves a remainder on the book — a
    /// position that outlives the signal that opened it and keeps losing after
    /// the stop was supposed to have ended it.
    held: Option<Quantity>,
}

impl Position {
    pub(crate) const fn new(risk: Risk, default_size: Quantity) -> Self {
        Self {
            risk,
            default_size,
            stop: None,
            target: None,
            held: None,
        }
    }

    pub(crate) const fn is_open(&self) -> bool {
        self.held.is_some()
    }

    /// The quantity traded when no risk-based sizing applies.
    pub(crate) const fn default_size(&self) -> Quantity {
        self.default_size
    }

    /// Whether the bar traded through the stop.
    ///
    /// Against the bar's *low*, not its close. A strategy that only checks the
    /// close lets a position that traded through its stop mid-bar survive to
    /// the end of it, which is not the strategy that was specified.
    pub(crate) fn stopped_out(&self, low: f64) -> bool {
        self.stop.is_some_and(|stop| low <= stop)
    }

    /// Whether the bar traded through the profit target.
    pub(crate) fn target_met(&self, high: f64) -> bool {
        self.target.is_some_and(|target| high >= target)
    }

    /// How many shares to buy so that a stop-out costs about the stated risk.
    ///
    /// Rounded down to whole shares, and `None` when that rounds to zero —
    /// buying a share anyway would silently risk more than the model allows,
    /// which is the failure this sizing exists to prevent.
    fn sized(&self, stop_distance: f64, price: f64) -> Option<Quantity> {
        let risk_amount = self.risk.risk_amount?;
        if stop_distance <= 0.0 || price <= 0.0 {
            return None;
        }
        let mut shares = (risk_amount / stop_distance).floor();

        // A tighter stop asks for a bigger position, so this is where an
        // intraday stop of a dollar tries to buy several accounts' worth.
        // Capping is what turns that into a smaller trade rather than a
        // rejected order and a silently empty backtest.
        if let Some(cap) = self.risk.max_position_value {
            shares = shares.min((cap / price).floor());
        }

        if shares < 1.0 {
            return None;
        }
        Quantity::new_checked(shares, 0).ok()
    }

    /// Works out the size and stop for an entry, or refuses the trade.
    ///
    /// `None` means do not enter, and there are two distinct reasons for it,
    /// both of which have to refuse rather than fall back:
    ///
    /// * a stop was configured but the ATR has not warmed up — entering
    ///   unprotected would be running a different strategy for the first few
    ///   trades, and those trades are in the record;
    /// * no size keeps the loss inside the risk budget — taking the trade
    ///   anyway breaks the one rule the risk model exists to enforce.
    fn plan(&self, price: f64, atr: Option<f64>) -> Option<(Quantity, Option<f64>)> {
        let Some(multiple) = self.risk.stop_atr_multiple else {
            return Some((self.default_size, None));
        };
        let distance = atr? * multiple;
        let size = match self.risk.risk_amount {
            None => self.default_size,
            Some(_) => self.sized(distance, price)?,
        };
        Some((size, Some(price - distance)))
    }
}

/// Position management, shared by every rule.
///
/// A trait rather than free functions because the bodies need the strategy's
/// own `order()`, `submit_order()` and `portfolio()`, which only exist on a
/// registered Nautilus component.
pub(crate) trait Managed: Strategy + StrategyNative {
    fn position(&self) -> &Position;
    fn position_mut(&mut self) -> &mut Position;
    fn instrument(&self) -> InstrumentId;

    /// Opens a long position at `price`, sized and stopped by the risk model.
    ///
    /// Returns whether it actually entered: a refusal is a normal outcome, not
    /// an error, and a caller that wants to reset signal state needs to know
    /// which happened.
    fn enter_long(
        &mut self,
        price: f64,
        atr: Option<f64>,
        target: Option<f64>,
    ) -> anyhow::Result<bool> {
        let Some((size, stop)) = self.position().plan(price, atr) else {
            return Ok(false);
        };
        {
            let position = self.position_mut();
            position.stop = stop;
            position.target = target;
            position.held = Some(size);
        }
        self.send(OrderSide::Buy, size, None)?;
        Ok(true)
    }

    /// Closes whatever is held. Does nothing when flat.
    ///
    /// The `reason` is stamped on the closing order as a tag, and that is the
    /// only place it survives. A stop here is a market order the strategy
    /// sends when it sees the level breached, not a resting stop order the
    /// venue holds, so nothing about the order itself says why it was sent —
    /// which means a ledger reading order *types* would report every exit as
    /// a signal and never as a stop. It would have been wrong silently.
    ///
    /// Asks the engine what the position actually is rather than trusting the
    /// quantity this strategy asked for. The two diverge whenever an order is
    /// rejected or partially filled — insufficient funds, for one — and a
    /// strategy that sold its *intended* size would then either leave a
    /// remainder that outlives its stop or flip short without ever deciding
    /// to. Its own record is the fallback, for a venue that reports nothing.
    fn close(&mut self, reason: &'static str) -> anyhow::Result<()> {
        let intended = {
            let position = self.position_mut();
            position.stop = None;
            position.target = None;
            position.held.take()
        };

        let instrument = self.instrument();
        let actual = self.portfolio().net_position(&instrument);
        let size = match f64::try_from(actual).ok().filter(|held| *held > 0.0) {
            Some(held) => Quantity::new_checked(held, 0).ok(),
            None => intended,
        };

        let Some(size) = size else {
            return Ok(());
        };
        self.send(OrderSide::Sell, size, Some(reason))
    }

    /// The exit every rule shares: stop first, then target.
    ///
    /// Stop before target, and both against the bar's extremes rather than its
    /// close. When a single bar touches both — which is exactly what a violent
    /// bar does — the order decides the result, and taking the stop is the
    /// pessimistic reading. Assuming the target filled first because the bar
    /// closed up is how a backtest turns its worst bars into its best ones.
    ///
    /// Returns the tag it exited on, or `None` if the position is still open.
    fn exit_on_levels(&mut self, high: f64, low: f64) -> anyhow::Result<Option<&'static str>> {
        if !self.position().is_open() {
            return Ok(None);
        }
        if self.position().stopped_out(low) {
            self.close(EXIT_STOP)?;
            return Ok(Some(EXIT_STOP));
        }
        if self.position().target_met(high) {
            self.close(EXIT_SIGNAL)?;
            return Ok(Some(EXIT_SIGNAL));
        }
        Ok(None)
    }

    fn send(
        &mut self,
        side: OrderSide,
        size: Quantity,
        reason: Option<&'static str>,
    ) -> anyhow::Result<()> {
        let instrument = self.instrument();
        let order = self.order().market(
            instrument,
            side,
            size,
            None, // time_in_force
            None, // reduce_only
            None, // quote_quantity
            None, // exec_algorithm_id
            None, // exec_algorithm_params
            reason.map(|reason| vec![ustr::Ustr::from(reason)]),
            None, // client_order_id
        );
        self.submit_order(order, None, None, None)
    }
}

/// Wires a strategy into [`Managed`] and Nautilus's actor plumbing.
///
/// Six strategies each need the same four accessors and the same two trait
/// impls; written out they are sixty lines of code whose only content is which
/// field holds what.
macro_rules! managed_strategy {
    ($ty:ident) => {
        nautilus_trading::nautilus_strategy!($ty);

        impl crate::strategy::Managed for $ty {
            fn position(&self) -> &crate::strategy::Position {
                &self.position
            }
            fn position_mut(&mut self) -> &mut crate::strategy::Position {
                &mut self.position
            }
            fn instrument(&self) -> nautilus_model::identifiers::InstrumentId {
                self.instrument_id
            }
        }

        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    .field("instrument_id", &self.instrument_id)
                    .field("position", &self.position)
                    .finish_non_exhaustive()
            }
        }
    };
}

pub(crate) use managed_strategy;

#[cfg(test)]
mod tests {
    use super::*;

    fn quantity(value: f64) -> Quantity {
        Quantity::new_checked(value, 0).expect("valid")
    }

    fn position(risk: Risk) -> Position {
        Position::new(risk, quantity(100.0))
    }

    const UNSTOPPED: Risk = Risk {
        stop_atr_multiple: None,
        atr_period: 14,
        risk_amount: None,
        max_position_value: None,
    };

    #[test]
    fn without_a_stop_the_default_size_is_traded() {
        let (size, stop) = position(UNSTOPPED).plan(100.0, None).expect("no stop needed");
        assert_eq!(size, quantity(100.0));
        assert_eq!(stop, None);
    }

    #[test]
    fn a_configured_stop_is_refused_rather_than_skipped_before_the_atr_warms_up() {
        // Entering unprotected because the indicator is not ready yet would
        // run a different strategy for the first few trades — and those trades
        // end up in the evidence.
        let risk = Risk {
            stop_atr_multiple: Some(2.0),
            ..UNSTOPPED
        };
        assert!(position(risk).plan(100.0, None).is_none());
    }

    #[test]
    fn risk_sizing_is_capped_by_what_the_account_can_hold() {
        // The bug this cap exists for: a tight stop asks for a bigger
        // position, so an intraday stop of a dollar orders several accounts'
        // worth, every order is rejected, and the backtest reports zero trades
        // with no error anywhere.
        let risk = Risk {
            stop_atr_multiple: Some(1.0),
            risk_amount: Some(1_000.0),
            max_position_value: Some(100_000.0),
            ..UNSTOPPED
        };
        // A $0.10 stop on a $500 share: unbounded sizing asks for 10,000
        // shares, which is $5m against a $100k cap.
        let (size, stop) = position(risk).plan(500.0, Some(0.1)).expect("sized");
        assert_eq!(size, quantity(200.0), "capped at 100k of a 500 share");
        assert!((stop.expect("stopped") - 499.9).abs() < 1e-9);
    }

    #[test]
    fn a_stop_too_tight_to_size_refuses_the_trade() {
        let risk = Risk {
            stop_atr_multiple: Some(2.0),
            risk_amount: Some(10.0),
            max_position_value: Some(100_000.0),
            ..UNSTOPPED
        };
        // $10 of risk against a $50 stop distance is a fifth of a share.
        assert!(
            position(risk).plan(500.0, Some(25.0)).is_none(),
            "rounding up to one share would breach the risk budget"
        );
    }

    #[test]
    fn a_stop_is_measured_against_the_low_and_a_target_against_the_high() {
        let mut open = position(UNSTOPPED);
        open.stop = Some(99.0);
        open.target = Some(105.0);

        assert!(!open.stopped_out(99.5), "the bar never reached the stop");
        assert!(open.stopped_out(98.0), "traded through it mid-bar");
        assert!(!open.target_met(104.0));
        assert!(open.target_met(105.0), "touching the target is meeting it");
    }
}
