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
//! [`crate::plan::Plan::from_spec`].
//!
//! A strategy here is a Nautilus component, which is why it lives on this side
//! of the boundary. `arvo-research` names it by string in `StrategySpec` and
//! never sees the type.

mod cross_sectional;
mod indicator;
mod breakout;
mod put_spread;
mod rules;

use nautilus_common::actor::DataActorNative;
use std::collections::BTreeMap;

use nautilus_core::UnixNanos;
use nautilus_model::{data::Bar, enums::OrderSide, identifiers::InstrumentId, types::Quantity};
use nautilus_trading::strategy::{Strategy, StrategyNative};

pub(crate) use cross_sectional::CrossSectionalMomentum;
pub(crate) use breakout::{Rule as BreakoutRule, ZeroDteBreakout};
pub(crate) use put_spread::{PutSpread, Rule as PutSpreadRule};
pub(crate) use rules::{
    BuyAndHold, MomentumBreakout, OpeningRange, SellAndHold, SmaCross, VolatilityBreakout,
    VwapReversion,
};

/// Tags stamped on a closing order to say why it was sent.
///
/// Read back by [`crate::ledger`], which is the other half of this contract:
/// change a spelling here and the ledger silently reclassifies every exit.
pub(crate) const EXIT_STOP: &str = "arvo:exit=stop";
pub(crate) const EXIT_SIGNAL: &str = "arvo:exit=signal";
pub(crate) const EXIT_HALT: &str = "arvo:exit=halt";

/// What the engine's own rules call themselves when they propose a trade.
///
/// The same field a live alert pipeline fills in, so a rejection can say which
/// path was refused and a fill can be attributed afterwards.
pub(crate) const PROPOSER: &str = "engine";

/// What a strategy does to protect a position.
///
/// # Why this is the experiment's own `RiskModel` and not a copy of it
///
/// It used to be six mirrored fields. That made the engine's risk policy a
/// second implementation of the same rules, and a second implementation is a
/// second thing that can be wrong — while every stored finding went on claiming
/// to describe the system that would actually trade.
///
/// So the model travels whole, and every limit in it is enforced by
/// [`arvo_research::decide`], which is the same function a live session calls.
/// What stays here is the part that genuinely needs bars: turning an ATR into a
/// stop distance.
#[derive(Debug, Clone)]
pub(crate) struct Risk {
    pub(crate) model: arvo_research::RiskModel,
    /// What a fill costs on top of its price, from the experiment, so an entry
    /// is sized at what the account can pay all in.
    pub(crate) costs: arvo_research::CostModel,
    /// Opening balance. Every fractional limit in the model is a fraction of
    /// this, resolved at decision time rather than up front so the engine and a
    /// live gate divide the same numbers the same way.
    pub(crate) starting_cash: f64,
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
    /// Pairwise correlation over the bars this run has already seen.
    ///
    /// Shared: a book's members are separate strategy instances over one
    /// account, and a correlation cap has to see all of them. An `Arc` rather
    /// than a field on `Risk`, because `Risk` is `Copy` and a handle is not.
    correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    /// The instant of the most recent bar, which is this run's "now".
    ///
    /// Recorded because [`arvo_research::decide`] is given a clock rather than
    /// reading one — that is what lets the same function run identically here,
    /// where time is a bar timestamp, and in a live session where it is not.
    last_bar_at: Option<chrono::NaiveDateTime>,
    /// The highest account equity seen so far, and whether the drawdown limit
    /// has since been reached.
    ///
    /// Peak-to-trough against a running maximum, which is what a drawdown is:
    /// measuring against *starting* capital instead would let a rule give back
    /// every gain it ever made without once registering a fall.
    peak_equity: Option<f64>,
    halted: bool,
}

impl Position {
    pub(crate) fn new(
        risk: Risk,
        default_size: Quantity,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        Self {
            risk,
            default_size,
            correlations,
            stop: None,
            target: None,
            held: None,
            last_bar_at: None,
            peak_equity: None,
            halted: false,
        }
    }

    /// Records what was just bought.
    ///
    /// Named rather than assigned in place, because the two callers that do it
    /// are in different modules now and a field poked from two directions is a
    /// field that acquires a third meaning.
    pub(crate) const fn hold(&mut self, size: Quantity) {
        self.held = Some(size);
    }

    /// Gives up what was held, and the levels that went with it.
    ///
    /// The levels go together with the size deliberately: a stop left behind
    /// after the position it protected is gone will fire against the next one,
    /// at a price chosen for a trade that already ended.
    pub(crate) const fn release(&mut self) -> Option<Quantity> {
        self.stop = None;
        self.target = None;
        self.held.take()
    }

    /// Whether the account's drawdown limit has been reached.
    ///
    /// Once true it stays true. Nothing else is coherent: a rule that has
    /// stopped trading cannot recover the equity that would let it resume.
    pub(crate) const fn is_halted(&self) -> bool {
        self.halted
    }

    /// Records the account's equity and says whether the limit has just been
    /// breached.
    fn observe(&mut self, equity: f64) -> bool {
        let limit = match self.risk.model.max_drawdown {
            Some(limit) if !self.halted => limit,
            _ => return false,
        };
        let peak = self.peak_equity.map_or(equity, |peak| peak.max(equity));
        self.peak_equity = Some(peak);

        if peak > 0.0 && (peak - equity) / peak >= limit {
            self.halted = true;
            return true;
        }
        false
    }

    pub(crate) const fn risk(&self) -> &Risk {
        &self.risk
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

    /// The stop distance this entry would use, or `None` if there is no stop.
    ///
    /// The one part of sizing that genuinely belongs to the engine: it needs an
    /// ATR, which needs bars, which `arvo_research` does not have. Everything
    /// downstream of the distance — how many shares that buys, whether the cap
    /// binds, whether the account may take the trade at all — goes through
    /// [`arvo_research::decide`].
    ///
    /// `Err(())` means a stop was configured and the ATR has not warmed up.
    /// Entering unprotected would be running a different strategy for the first
    /// few trades, and those trades are in the record — so it refuses rather
    /// than falling back.
    fn stop_distance(&self, atr: Option<f64>) -> Result<Option<f64>, ()> {
        match self.risk.model.stop_atr_multiple {
            None => Ok(None),
            Some(multiple) => atr.map(|atr| Some(atr * multiple)).ok_or(()),
        }
    }
}

/// A Nautilus timestamp as a civil instant, or `None` if it cannot be one.
///
/// `None` rather than a saturating fallback: a nonsensical timestamp that
/// became `NaiveDateTime::MAX` would silently make every subsequent signal
/// look stale and every closed position look like it was booked on a different
/// day. Refusing to decide on a clock that makes no sense is the safe answer.
fn nanos_to_instant(at: UnixNanos) -> Option<chrono::NaiveDateTime> {
    i64::try_from(at.as_u64())
        .ok()
        .map(|nanos| chrono::DateTime::from_timestamp_nanos(nanos).naive_utc())
}

fn nanos_to_date(at: UnixNanos) -> Option<chrono::NaiveDate> {
    nanos_to_instant(at).map(|instant| instant.date())
}

/// The account as the engine's own cache already has it.
///
/// Read rather than tracked. A book's members are separate strategy instances
/// sharing one account and knowing nothing of each other, so a private tally
/// would give N accounts of one and cap nothing — and a second running total of
/// something the engine already holds is a second thing that can be wrong,
/// which this codebase has paid for once already.
///
/// Returns what is open and what was realised on `today`.
/// Every position the engine knows about, live and finished.
///
/// **Both sources.** A cycle that closed leaves the live record reset and empty
/// behind it, so `positions()` alone does not contain finished round trips —
/// `ledger::from_cache` has always combined the two for exactly this reason, and
/// reading only the live half here meant the daily loss limit never saw a
/// realised loss and the day-trade budget never saw a completed day trade. Both
/// limits were wired, enforced, and looking at an empty history.
pub(crate) fn account_from_cache(
    cache: &nautilus_common::cache::CacheApi<'_>,
    today: chrono::NaiveDate,
) -> (BTreeMap<String, arvo_research::Position>, f64, usize) {
    let mut all = cache.position_snapshots(None, None);
    all.extend(cache.positions(None, None, None, None, None));
    account_from_positions(all, today)
}

pub(crate) fn account_from_positions(
    held: impl IntoIterator<Item = nautilus_model::position::Position>,
    today: chrono::NaiveDate,
) -> (BTreeMap<String, arvo_research::Position>, f64, usize) {
    let mut positions = BTreeMap::new();
    let mut realised_today = 0.0;
    // Round trips opened and closed on one day, for the pattern-day-trader
    // count. Gathered here because this is already walking every position the
    // engine holds, and a second walk would be a second chance to disagree.
    let mut day_trades: Vec<chrono::NaiveDate> = Vec::new();

    for position in held {
        if position.is_open() {
            positions.insert(
                position.instrument_id.to_string(),
                arvo_research::Position {
                    // Signed: a short's collateral is read from its sign.
                    quantity: position.signed_qty,
                    entry: position.avg_px_open,
                },
            );
        } else if position
            .ts_closed
            .is_some_and(|closed| nanos_to_date(closed) == Some(today))
        {
            // Realised *today* on the run's own clock: a backtest's day is the
            // bar's date, never the machine's.
            realised_today += position.realized_pnl.map_or(0.0, |money| money.as_f64());
        }

        match (
            nanos_to_date(position.ts_opened),
            position.ts_closed.and_then(nanos_to_date),
        ) {
            // A completed round trip inside one day.
            (Some(opened), Some(closed)) if opened == closed => day_trades.push(closed),
            // Still open and opened today: it becomes a day trade the moment it
            // closes, and closing cannot be refused, so the budget has to have
            // accounted for it already.
            (Some(opened), None) if opened == today => day_trades.push(opened),
            _ => {}
        }
    }

    let earliest = arvo_research::risk::business_days_before(today, arvo_research::PDT_WINDOW_DAYS);
    let day_trades_used = day_trades
        .iter()
        .filter(|at| **at >= earliest && **at <= today)
        .count();
    (positions, realised_today, day_trades_used)
}

/// Free cash in the account at `venue`, as the engine holds it.
///
/// `None` before the account exists, which leaves the cash ceiling off for
/// the one entry that cannot need it: nothing has been spent yet.
///
/// Read as the account's only currency rather than `balance_free(None)`, which
/// panics on an account with no base currency — every backtest account here.
///
/// ponytail: `None` for a multi-currency account, which has no single cash
/// figure; price the balances in one currency if a run ever holds two.
pub(crate) fn spendable(
    cache: &nautilus_common::cache::CacheApi<'_>,
    venue: &nautilus_model::identifiers::Venue,
) -> Option<f64> {
    use nautilus_model::accounts::Account as _;
    let free = cache.account_for_venue(venue)?.balances_free();
    let mut balances = free.values();
    match (balances.next(), balances.next()) {
        (Some(only), None) => Some(only.as_f64()),
        _ => None,
    }
}

/// Puts one entry to the same risk policy a live session uses.
///
/// # Why the engine asks rather than deciding
///
/// Because a stored finding claims to describe the system that will actually
/// trade, and that claim is false the moment the backtest enforces its own
/// version of the limits. The position cap, the daily loss limit, the
/// correlation cap and the sizing are all [`arvo_research::decide`] — the same
/// function `arvo_execution`'s live gate calls with its own book.
#[expect(clippy::too_many_arguments, reason = "it is one call, spelled out")]
pub(crate) fn decide_entry(
    opens_short: bool,
    risk: &Risk,
    default_size: Quantity,
    instrument: &str,
    price: f64,
    stop_distance: Option<f64>,
    now: chrono::NaiveDateTime,
    positions: &BTreeMap<String, arvo_research::Position>,
    realised_today: f64,
    halted: bool,
    equity: f64,
    day_trades_used: usize,
    spendable: Option<f64>,
    correlations: Option<&dyn arvo_research::Correlations>,
) -> arvo_research::Decision {
    let proposal = arvo_research::Proposal {
        instrument: instrument.to_owned(),
        proposer: PROPOSER.to_owned(),
        // Signalled at the bar it was read from. A backtest has no network
        // between the signal and the order, so the staleness check cannot fire
        // here — which is exactly the gap paper trading exists to measure and a
        // backtest cannot measure about itself.
        signalled_at: now,
        reference_price: price,
        stop_distance,
        // What the rule trades absent risk sizing, so the engine's fixed trade
        // size survives the move to a shared policy.
        desired_quantity: Some(default_size.as_f64()),
        opens_short,
    };

    arvo_research::decide(
        &risk.model,
        &arvo_research::AccountState {
            positions,
            realised_today,
            starting_cash: risk.starting_cash,
            equity,
            day_trades_used,
            halted: halted.then_some("the account drawdown limit was reached"),
            spendable,
        },
        &proposal,
        now,
        // Never stale. See `signalled_at` above.
        i64::MAX,
        correlations,
        Some(&risk.costs),
    )
}

/// Position management, shared by every rule.
///
/// A trait rather than free functions because the bodies need the strategy's
/// own `order()`, `submit_order()` and `portfolio()`, which only exist on a
/// registered Nautilus component.
pub(crate) trait Managed: Strategy + StrategyNative + DataActorNative {
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
        let Ok(stop_distance) = self.position().stop_distance(atr) else {
            // A stop was asked for and the ATR has not warmed up.
            return Ok(false);
        };

        let Some(decision) = self.ask_risk(price, stop_distance, false) else {
            return Ok(false);
        };
        let arvo_research::Decision::Accept { quantity } = decision else {
            // A refusal is a normal outcome, not an error. The reason is
            // already recorded in the rejection; nothing here needs to
            // re-derive it.
            return Ok(false);
        };
        let Ok(size) = Quantity::new_checked(quantity, 0) else {
            return Ok(false);
        };

        {
            let position = self.position_mut();
            position.stop = stop_distance.map(|distance| price - distance);
            position.target = target;
            position.hold(size);
        }
        self.send(OrderSide::Buy, size, None)?;
        Ok(true)
    }

    /// Sells an option contract to open, sized by the cash its worst case at
    /// expiry needs (#84).
    ///
    /// Unstopped and untargeted: [`Self::exit_on_levels`] reads a long's levels,
    /// and a rule that sells manages its own exit. The gate refuses anything
    /// that is not an option, and any call it could not cover.
    fn enter_short(&mut self, price: f64) -> anyhow::Result<bool> {
        let Some(arvo_research::Decision::Accept { quantity }) = self.ask_risk(price, None, true)
        else {
            return Ok(false);
        };
        let Ok(size) = Quantity::new_checked(quantity, 0) else {
            return Ok(false);
        };
        self.position_mut().hold(size);
        self.send(OrderSide::Sell, size, None)?;
        Ok(true)
    }

    /// This strategy's entry, put to the shared policy.
    ///
    /// `None` when there is no bar yet, which is before anything can trade.
    fn ask_risk(
        &self,
        price: f64,
        stop_distance: Option<f64>,
        opens_short: bool,
    ) -> Option<arvo_research::Decision> {
        let now = self.position().last_bar_at?;
        // Bound, not inlined: `positions` borrows from the cache handle, and a
        // temporary would be dropped at the end of the expression.
        let (positions, realised_today, day_trades_used) =
            account_from_cache(&self.cache(), now.date());
        // Marked to market, because the pattern-day-trader floor tests what the
        // account is worth now. Before the first fill there is no account, and
        // the opening balance is the honest stand-in.
        let equity = self
            .portfolio()
            .equity(&self.instrument().venue, None)
            .values()
            .next()
            .map(nautilus_model::types::Money::as_f64)
            .unwrap_or(self.position().risk.starting_cash);
        Some(decide_entry(
            opens_short,
            &self.position().risk,
            self.position().default_size,
            &self.instrument().to_string(),
            price,
            stop_distance,
            now,
            &positions,
            realised_today,
            self.position().is_halted(),
            equity,
            day_trades_used,
            spendable(&self.cache(), &self.instrument().venue),
            Some(self.position().correlations.as_ref()),
        ))
    }

    /// Records what this bar says, before any decision is taken on it.
    ///
    /// Called first by every rule, on every bar. Two things depend on it and
    /// both fail silently when it is missed: the run's clock, which every risk
    /// decision needs, and the correlation estimate, which answers *unknown*
    /// for a pair it has not been fed — so a configured correlation cap would
    /// refuse every entry rather than quietly allowing them.
    ///
    /// Separate from [`Self::halt_if_drawn_down`] because observing is not
    /// halting, and one function doing both would have a name that lies about
    /// half of what it does.
    fn observe_bar(&mut self, bar: &Bar) {
        let at = nanos_to_instant(bar.ts_event);
        self.position_mut().last_bar_at = at;

        if let Some(at) = at {
            let instrument = self.instrument().to_string();
            self.position()
                .correlations
                .observe(&instrument, at, bar.close.as_f64());
        }
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
        let intended = self.position_mut().release();

        let instrument = self.instrument();
        let actual = f64::try_from(self.portfolio().net_position(&instrument)).unwrap_or(0.0);
        // A short is closed by buying it back. The venue's sign decides, since
        // this strategy's own record holds a size and not a side.
        let (side, size) = if actual < 0.0 {
            (OrderSide::Buy, Quantity::new_checked(-actual, 0).ok())
        } else if actual > 0.0 {
            (OrderSide::Sell, Quantity::new_checked(actual, 0).ok())
        } else {
            (OrderSide::Sell, intended)
        };

        let Some(size) = size else {
            return Ok(());
        };
        self.send(side, size, Some(reason))
    }

    /// Squares what this strategy believes it holds with the venue, after the
    /// venue refused an order.
    ///
    /// A position is recorded when its entry is *sent*, so a refused entry
    /// left the strategy holding nothing and believing otherwise: it sat out
    /// the session, then sent an exit for shares it never bought. Released
    /// only when the venue is flat — a refused *exit* leaves the shares held,
    /// and the gate reads those from the engine rather than from here. Counted
    /// after the run, from the engine's order records; see `ledger::refused`.
    fn refused(&mut self) {
        let instrument = self.instrument();
        let flat = f64::try_from(self.portfolio().net_position(&instrument))
            .is_ok_and(|held| held == 0.0);
        if flat {
            self.position_mut().release();
        }
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

    /// Stops the rule if the account has fallen too far from its own peak.
    ///
    /// Called every bar by every rule, before any signal is read. A per-trade
    /// stop bounds one loss; this bounds their sum, which is the number that
    /// actually ends accounts — twenty consecutive stop-outs at one percent
    /// each is a well-behaved rule and a twenty percent hole.
    ///
    /// The equity comes from the engine's own portfolio, marked to market,
    /// rather than from anything this strategy tracks itself. A second running
    /// total of the same quantity is a second thing that can be wrong, and the
    /// one that disagreed with the account would be this one.
    ///
    /// Returns whether the halt fired on this bar.
    fn halt_if_drawn_down(&mut self) -> anyhow::Result<bool> {
        if self.position().risk.model.max_drawdown.is_none() || self.position().is_halted() {
            return Ok(false);
        }
        let venue = self.instrument().venue;
        let equity = self
            .portfolio()
            .equity(&venue, None)
            .values()
            .next()
            .map(nautilus_model::types::Money::as_f64);

        // No account yet — before the first fill there is nothing to measure.
        let Some(equity) = equity else {
            return Ok(false);
        };
        if !self.position_mut().observe(equity) {
            return Ok(false);
        }

        // Flatten. A halt that left a position open would be a risk limit that
        // stops you adding to the thing already losing money.
        self.close(EXIT_HALT)?;
        Ok(true)
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
        nautilus_trading::nautilus_strategy!($ty, {
            fn on_order_denied(&mut self, _event: nautilus_model::events::OrderDenied) {
                crate::strategy::Managed::refused(self);
            }
            fn on_order_rejected(&mut self, _event: nautilus_model::events::OrderRejected) {
                crate::strategy::Managed::refused(self);
            }
        });

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
        Position::new(risk, quantity(100.0), std::sync::Arc::default())
    }

    /// A risk model with nothing switched on, and a $100k account.
    const UNSTOPPED: Risk = Risk {
        costs: arvo_research::CostModel::proportional(0.0, 0.0),
        model: arvo_research::RiskModel {
            stop_atr_multiple: None,
            atr_period: 14,
            risk_per_trade: None,
            max_position_fraction: None,
            max_drawdown: None,
            max_concurrent_positions: None,
            max_daily_loss: None,
            correlation_cap: None,
            sector_cap: None,
            day_trading: arvo_research::DayTradingRule::Unconstrained,
        },
        starting_cash: 100_000.0,
    };

    fn today() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 11)
            .expect("valid")
            .and_hms_opt(14, 30, 0)
            .expect("valid")
    }

    /// What the engine would decide for one entry, through the shared policy.
    ///
    /// Deliberately routed through [`decide_entry`] rather than a local sizing
    /// helper. These used to test the engine's own arithmetic, which is exactly
    /// the second implementation that has now been removed — testing it here
    /// again would re-create the thing the shared policy exists to prevent.
    fn entry(risk: Risk, price: f64, atr: Option<f64>) -> arvo_research::Decision {
        let position = position(risk.clone());
        let stop_distance = position
            .stop_distance(atr)
            .expect("the caller knows whether the ATR is ready");
        decide_entry(
            false,
            &risk,
            quantity(100.0),
            "MSFT.NASDAQ",
            price,
            stop_distance,
            today(),
            &BTreeMap::new(),
            0.0,
            false,
            UNSTOPPED.starting_cash,
            0,
            None,
            None,
        )
    }

    #[test]
    fn without_a_stop_the_default_size_is_traded() {
        assert_eq!(
            entry(UNSTOPPED, 100.0, None),
            arvo_research::Decision::Accept { quantity: 100.0 }
        );
        assert_eq!(
            position(UNSTOPPED).stop_distance(None),
            Ok(None),
            "and no stop is placed"
        );
    }

    #[test]
    fn a_configured_stop_is_refused_rather_than_skipped_before_the_atr_warms_up() {
        // Entering unprotected because the indicator is not ready yet would run
        // a different strategy for the first few trades — and those trades end
        // up in the evidence.
        let risk = Risk {
            model: arvo_research::RiskModel {
                stop_atr_multiple: Some(2.0),
                ..UNSTOPPED.model
            },
            ..UNSTOPPED
        };
        assert!(position(risk).stop_distance(None).is_err());
    }

    #[test]
    fn risk_sizing_is_capped_by_what_the_account_can_hold() {
        // The bug this cap exists for: a tight stop asks for a bigger position,
        // so an intraday stop of a dollar orders several accounts' worth, every
        // order is rejected, and the backtest reports zero trades with no error
        // anywhere.
        let risk = Risk {
            model: arvo_research::RiskModel {
                stop_atr_multiple: Some(1.0),
                risk_per_trade: Some(0.01),
                max_position_fraction: Some(1.0),
                ..UNSTOPPED.model
            },
            ..UNSTOPPED
        };
        // A $0.10 stop on a $500 share: unbounded sizing asks for 10,000
        // shares, which is $5m against a $100k account.
        assert_eq!(
            entry(risk.clone(), 500.0, Some(0.1)),
            arvo_research::Decision::Accept { quantity: 200.0 },
            "capped at one account's worth of a $500 share"
        );
        assert_eq!(
            position(risk).stop_distance(Some(0.1)),
            Ok(Some(0.1)),
            "and the stop sits a tenth below the entry"
        );
    }

    #[test]
    fn a_stop_too_tight_to_size_refuses_the_trade() {
        let risk = Risk {
            model: arvo_research::RiskModel {
                stop_atr_multiple: Some(2.0),
                // $100 of risk on a $100k account.
                risk_per_trade: Some(0.001),
                max_position_fraction: Some(1.0),
                ..UNSTOPPED.model
            },
            ..UNSTOPPED
        };
        // $100 of risk against a $50 stop distance is two shares... but at $500
        // a share that is within the cap, so widen the stop until it rounds to
        // nothing: $100 against a $200 distance is half a share.
        assert!(
            matches!(
                entry(risk, 500.0, Some(100.0)),
                arvo_research::Decision::Reject(arvo_research::Rejection::TooSmall { .. })
            ),
            "rounding up to one share would breach the risk budget"
        );
    }

    #[test]
    fn the_daily_loss_limit_binds_in_a_backtest() {
        // The whole point of routing the engine through the shared policy. This
        // limit existed on `RiskModel` and no backtest had ever enforced it, so
        // every stored finding claimed a control it did not run under.
        let risk = Risk {
            model: arvo_research::RiskModel {
                max_daily_loss: Some(0.02),
                max_position_fraction: Some(1.0),
                ..UNSTOPPED.model
            },
            ..UNSTOPPED
        };
        let refused = decide_entry(
            false,
            &risk,
            quantity(100.0),
            "MSFT.NASDAQ",
            100.0,
            None,
            today(),
            &BTreeMap::new(),
            // $2,100 lost against a $2,000 limit on $100k.
            -2_100.0,
            false,
            UNSTOPPED.starting_cash,
            0,
            None,
            None,
        );
        assert!(matches!(
            refused,
            arvo_research::Decision::Reject(arvo_research::Rejection::DailyLossLimit { .. })
        ));
    }

    #[test]
    fn the_position_cap_counts_every_member_of_a_book() {
        // Counted from the engine's account rather than per strategy: a book's
        // members are separate instances sharing one balance, so a private
        // tally would give N caps of one and cap nothing at all.
        let risk = Risk {
            model: arvo_research::RiskModel {
                max_concurrent_positions: Some(2),
                max_position_fraction: Some(1.0),
                ..UNSTOPPED.model
            },
            ..UNSTOPPED
        };
        let mut held = BTreeMap::new();
        for id in ["AAPL.NASDAQ", "NVDA.NASDAQ"] {
            held.insert(
                id.to_owned(),
                arvo_research::Position {
                    quantity: 10.0,
                    entry: 100.0,
                },
            );
        }

        let refused = decide_entry(
            false,
            &risk,
            quantity(100.0),
            "MSFT.NASDAQ",
            100.0,
            None,
            today(),
            &held,
            0.0,
            false,
            UNSTOPPED.starting_cash,
            0,
            None,
            None,
        );
        assert!(matches!(
            refused,
            arvo_research::Decision::Reject(arvo_research::Rejection::TooManyPositions {
                held: 2,
                limit: 2
            })
        ));
    }

    #[test]
    fn a_backtest_signal_is_never_stale() {
        // A bar's signal is generated at the bar's own instant and there is no
        // network in between. The check is still on the same path a live
        // session runs — which is the point — and the fact that it cannot fire
        // here is precisely the gap paper trading exists to measure.
        let accepted = decide_entry(
            false,
            &UNSTOPPED,
            quantity(100.0),
            "MSFT.NASDAQ",
            100.0,
            None,
            today(),
            &BTreeMap::new(),
            0.0,
            false,
            UNSTOPPED.starting_cash,
            0,
            None,
            None,
        );
        assert!(matches!(
            accepted,
            arvo_research::Decision::Accept { .. }
        ));
    }

    fn halting() -> Risk {
        Risk {
            model: arvo_research::RiskModel {
                max_drawdown: Some(0.10),
                ..UNSTOPPED.model
            },
            ..UNSTOPPED
        }
    }

    #[test]
    fn drawdown_is_measured_from_the_peak_not_from_the_start() {
        // Measuring against starting capital would let a rule give back every
        // gain it ever made without once registering a fall.
        let mut position = position(halting());
        assert!(!position.observe(100_000.0));
        assert!(!position.observe(200_000.0), "a new peak is not a drawdown");
        // 185k is 7.5% below the 200k peak, and 85% *above* the start.
        assert!(!position.observe(185_000.0));
        assert!(position.observe(179_000.0), "10.5% below the peak");
    }

    #[test]
    fn the_halt_is_permanent() {
        // Nothing else is coherent: a rule that has stopped trading cannot
        // recover the equity that would let it resume.
        let mut position = position(halting());
        position.observe(100_000.0);
        assert!(position.observe(89_000.0));
        assert!(position.is_halted());

        // Even back above the old peak.
        assert!(!position.observe(150_000.0), "it does not fire twice");
        assert!(position.is_halted(), "and it does not un-fire");
    }

    #[test]
    fn without_a_limit_nothing_ever_halts() {
        let mut position = position(UNSTOPPED);
        assert!(!position.observe(100_000.0));
        assert!(!position.observe(1.0), "a 99.999% fall, and no limit to hit");
        assert!(!position.is_halted());
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
