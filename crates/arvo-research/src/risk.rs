//! The one thing allowed to say yes to an order.
//!
//! # Why a gate rather than checks at each call site
//!
//! Because there is about to be more than one way to place an order. A signal
//! from the technical engine and a signal from an alert pipeline are two
//! proposers into *one account*, and neither can see the other's positions. If
//! each sized its own trade against its own idea of the balance, a rule and an
//! alert firing on correlated names would take two full-sized positions in
//! what is economically one bet, and nothing in either path would know.
//!
//! So: paths propose, the gate disposes. [`RiskGate`] holds the live position
//! book, the day's realised loss and the equity peak, and it is the only thing
//! that returns an executable quantity. A proposer that wants to bypass it has
//! to be written to bypass it, which is the point.
//!
//! # Why it lives beside `RiskModel` and not in the executor
//!
//! Because the same gate has to run in the backtest. The platform's whole claim
//! is that a stored finding describes the system that will actually trade — and
//! that is false the moment live risk is different code from backtested risk. A
//! second risk engine on the live side would not be a refinement; it would make
//! every stored verdict a statement about a system that does not exist.
//!
//! [`RiskGate`] therefore takes a [`crate::RiskModel`] — the same one pinned
//! into every [`crate::Experiment`] — and nothing else. It knows nothing about
//! brokers, venues, order types or wire formats.
//!
//! # What it deliberately does not do
//!
//! It does not decide *direction* and it does not invent a price. A proposer
//! says "I want to be long this much of this instrument, on a signal from this
//! instant"; the gate answers with a quantity or a reason. Sizing rules that
//! need a stop distance need it supplied, because the gate has no bars.

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::RiskModel;

/// How stale a signal may be before the gate refuses to act on it.
///
/// # Why this is a risk control and not a networking detail
///
/// Because entering a momentum break late is a loss, not a delay. A signal that
/// took two seconds to arrive describes a market that no longer exists, and the
/// fill it gets is the one everybody faster already took the other side of. The
/// worst version is an alert pipeline whose latency varies: it is right when the
/// network is quiet and catastrophically late exactly when news is breaking and
/// everything is queued.
///
/// Half a second, defaulted, because that is roughly the point past which an
/// intraday signal on a liquid instrument is describing history. It is a field
/// rather than a constant because the right answer depends on the horizon: a
/// daily-rebalance proposal is not stale at five seconds, and a scalp is stale
/// at fifty milliseconds.
pub const DEFAULT_MAX_SIGNAL_AGE_MS: i64 = 500;

/// Two instruments this correlated count as one bet.
///
/// # Why a cluster cap rather than a portfolio-level number
///
/// A single "maximum portfolio correlation" figure is one number describing a
/// matrix, and it hides the case that actually ends accounts: five positions
/// that are each mildly correlated with the index and almost perfectly
/// correlated with *each other*. Capping positions within a cluster of
/// mutually-correlated names says the thing that matters — you may hold this
/// many bets, not this many tickers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CorrelationCap {
    /// Above this pairwise correlation, two instruments are the same bet.
    pub above: f64,
    /// The most positions allowed among instruments that correlated.
    pub max_positions: usize,
}

/// Pairwise correlations, however the caller happens to know them.
///
/// A trait rather than a matrix because the gate must not decide where
/// correlations come from: a backtest has the whole window's bars, a live
/// session has a rolling estimate, and a first deployment may have neither.
///
/// `None` means *unknown*, not *uncorrelated*. The gate treats those
/// differently and refuses rather than assuming — see
/// [`Rejection::CorrelationUnknown`].
pub trait Correlations: Send + Sync {
    /// Correlation between two instruments' returns, in `-1.0..=1.0`.
    fn between(&self, first: &str, second: &str) -> Option<f64>;
}

/// What a proposer wants to do.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub instrument: String,
    /// Which path asked, so a rejection can name it and a fill can be
    /// attributed. Two proposers into one account is the reason this gate
    /// exists; a book that could not say which one opened a position would
    /// make that impossible to audit afterwards.
    pub proposer: String,
    /// When the signal was *generated*, not when it arrived. The difference
    /// between those two is the whole of [`Rejection::Stale`].
    pub signalled_at: NaiveDateTime,
    /// Price the proposer expects to transact near, for sizing only. The gate
    /// does not send it to a broker and does not treat it as a limit.
    pub reference_price: f64,
    /// Distance from entry to the protective stop, in price. Required when the
    /// model sizes by risk, because capital-at-risk over stop distance is the
    /// only sizing rule that means anything — and with no distance there is
    /// nothing to divide by.
    pub stop_distance: Option<f64>,
    /// How much the proposer wants, when the model is not sizing by risk.
    ///
    /// `None` means "whatever the position cap allows".
    ///
    /// On the proposal rather than in [`RiskModel`] because it is the
    /// proposer's intent, not the account's policy: the gate's job is to
    /// *bound* a requested size, never to invent one. It is also what the
    /// backtest engine has always done — a fixed trade size when no risk
    /// sizing applies — and omitting it here would have made the same rule
    /// trade the whole account live and a hundred shares in the backtest.
    pub desired_quantity: Option<f64>,
}

/// A position the account is actually holding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub quantity: f64,
    pub entry: f64,
}

/// Why the gate refused.
///
/// Enumerated rather than a string because a rejection is a fact worth
/// counting: an alert pipeline that is refused for staleness nine times out of
/// ten has a latency problem, and one refused for the daily loss limit has had
/// a bad day. Those need different responses and a log line conflates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rejection {
    /// The account stopped trading and does not resume. See
    /// [`RiskModel::max_drawdown`].
    Halted { reason: String },
    /// Today's realised losses have reached the limit.
    DailyLossLimit { lost: f64, limit: f64 },
    /// The account already holds as many positions as it may.
    TooManyPositions { held: usize, limit: usize },
    /// Enough correlated positions are already open that this would be adding
    /// to a bet rather than making a new one.
    Correlated {
        with: Vec<String>,
        above: f64,
        limit: usize,
    },
    /// A correlation cap is configured and nothing can evaluate it.
    ///
    /// Refusing rather than passing, deliberately. An unenforceable limit that
    /// silently allows everything is worse than no limit, because the operator
    /// believes they have one.
    CorrelationUnknown { instrument: String, against: String },
    /// The signal is describing a market that has moved on.
    Stale { age_ms: i64, limit_ms: i64 },
    /// Sizing by risk with nothing to measure the risk against.
    NoStop,
    /// The position this would open rounds to nothing.
    ///
    /// Named rather than returned as a zero quantity, which every caller would
    /// have to remember to check and one of them would not.
    TooSmall { affordable: f64 },
    /// Already in this instrument. The gate does not add to positions: scaling
    /// in is a strategy decision with its own sizing rules, and treating a
    /// second signal as "buy the same amount again" is how one idea silently
    /// becomes three.
    AlreadyHeld { quantity: f64 },
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Send this many units, and no other number.
    Accept { quantity: f64 },
    Reject(Rejection),
}

/// The live risk authority.
///
/// Construct one per account, hand every proposer a handle, and let nothing
/// else emit an order.
#[derive(Debug, Clone)]
pub struct RiskGate {
    model: RiskModel,
    starting_cash: f64,
    equity: f64,
    peak_equity: f64,
    positions: BTreeMap<String, Position>,
    /// Realised profit and loss booked today. Negative is a loss.
    realised_today: f64,
    today: NaiveDate,
    halted: Option<String>,
    max_signal_age_ms: i64,
}

impl RiskGate {
    /// A gate over an account that has not traded yet.
    #[must_use]
    pub fn new(model: RiskModel, starting_cash: f64, today: NaiveDate) -> Self {
        Self {
            model,
            starting_cash,
            equity: starting_cash,
            peak_equity: starting_cash,
            positions: BTreeMap::new(),
            realised_today: 0.0,
            today,
            halted: None,
            max_signal_age_ms: DEFAULT_MAX_SIGNAL_AGE_MS,
        }
    }

    /// Overrides the staleness window. See [`DEFAULT_MAX_SIGNAL_AGE_MS`].
    #[must_use]
    pub const fn with_max_signal_age_ms(mut self, ms: i64) -> Self {
        self.max_signal_age_ms = ms;
        self
    }

    /// Whether the account has stopped trading, and why.
    #[must_use]
    pub fn halted(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    #[must_use]
    pub fn positions(&self) -> &BTreeMap<String, Position> {
        &self.positions
    }

    #[must_use]
    pub const fn equity(&self) -> f64 {
        self.equity
    }

    /// Answers one proposal against this gate's own book.
    ///
    /// A thin wrapper over [`decide`], which is where the policy lives. The
    /// split is the point: the backtest engine calls the same function with the
    /// engine's book, so neither side has a private copy of the rules.
    #[must_use]
    pub fn propose(
        &self,
        proposal: &Proposal,
        now: NaiveDateTime,
        correlations: Option<&dyn Correlations>,
    ) -> Decision {
        decide(
            &self.model,
            &AccountState {
                positions: &self.positions,
                realised_today: self.realised_on(now.date()),
                starting_cash: self.starting_cash,
                halted: self.halted.as_deref(),
            },
            proposal,
            now,
            self.max_signal_age_ms,
            correlations,
        )
    }

    /// Records that a position was opened.
    ///
    /// Separate from [`Self::propose`] because a proposal that is accepted may
    /// still not fill. Book what the broker confirms, never what was sent — a
    /// gate that assumed its own acceptances became positions would refuse
    /// trades on exposure the account does not have.
    pub fn opened(&mut self, instrument: &str, quantity: f64, entry: f64) {
        self.positions
            .insert(instrument.to_owned(), Position { quantity, entry });
    }

    /// Records that a position closed, booking its realised profit or loss.
    ///
    /// Feeds both the daily loss limit and the drawdown halt, which is why it
    /// must be called on every close including a stop-out.
    pub fn closed(&mut self, instrument: &str, pnl: f64, on: NaiveDate) {
        self.positions.remove(instrument);
        self.roll_day(on);
        self.realised_today += pnl;
        self.mark(self.equity + pnl);
    }

    /// Updates the account's equity, which is what the drawdown halt watches.
    ///
    /// Marked rather than derived: the halt has to see open positions move
    /// against the account, not only closed ones. A halt that only counted
    /// realised losses would let an account fall to nothing while holding.
    pub fn mark(&mut self, equity: f64) {
        self.equity = equity;
        self.peak_equity = self.peak_equity.max(equity);

        if self.halted.is_some() {
            return;
        }
        if let Some(limit) = self.model.max_drawdown {
            if self.peak_equity > 0.0 {
                let depth = (self.peak_equity - equity) / self.peak_equity;
                if depth >= limit {
                    // Permanent for the session. Nothing else is coherent: a
                    // gate that stopped trading cannot recover the equity that
                    // would let it resume, so "halt until recovered" would
                    // either never resume or would have to keep trading to
                    // find out — which is not a halt.
                    self.halted = Some(format!(
                        "account fell {:.1}% below its peak, against a {:.1}% limit",
                        depth * 100.0,
                        limit * 100.0
                    ));
                }
            }
        }
    }

    /// What was realised on `day`, which is zero for any day but the booked one.
    ///
    /// Read against the *proposal's* day rather than against whatever date the
    /// last close happened to carry. Rolling the day only on a close — which is
    /// what this did first — meant the limit never lifted on a day that opened
    /// no position and closed nothing, so a session that stopped out on Friday
    /// was still refused on Monday. A limit that silently becomes permanent is
    /// worse than no limit; the test that caught it is
    /// `the_daily_limit_lifts_the_next_day_and_the_drawdown_halt_does_not`.
    ///
    /// Kept as a read rather than a reset so [`Self::propose`] stays
    /// non-mutating: a gate that changed state when merely asked a question
    /// would give different answers depending on how often it was polled.
    fn realised_on(&self, day: NaiveDate) -> f64 {
        if day == self.today {
            self.realised_today
        } else {
            0.0
        }
    }

    /// Resets the day's realised loss when the date changes.
    ///
    /// The daily limit is the one control that is *supposed* to lift. The
    /// drawdown halt is not, and does not.
    fn roll_day(&mut self, on: NaiveDate) {
        if on != self.today {
            self.today = on;
            self.realised_today = 0.0;
        }
    }
}

/// The account, as whoever is asking already knows it.
///
/// # Why the decision takes state rather than owning it
///
/// Because the backtest engine already has a position book and a realised P&L,
/// and so does a live session. If [`decide`] kept its own copy there would be
/// two running totals of each quantity — and this codebase has already been
/// bitten by exactly that: `returns_series` was a second version of the equity
/// curve, and it was wrong for months while every total agreed.
///
/// So the rules live in one place and the book stays wherever it already lived.
/// [`RiskGate`] supplies this from its own state for a live session;
/// `arvo-nautilus` supplies it from the engine's own cache and portfolio.
/// Neither copies the other, and both get the same answer because it is the
/// same function.
#[derive(Debug, Clone, Copy)]
pub struct AccountState<'a> {
    /// What is held right now, by instrument.
    pub positions: &'a BTreeMap<String, Position>,
    /// Profit and loss booked *today*, negative for a loss.
    ///
    /// The caller decides what "today" means, because only it knows whether the
    /// clock it runs on is a wall clock or a bar timestamp.
    pub realised_today: f64,
    /// Opening balance, which is what every fractional limit is a fraction of.
    pub starting_cash: f64,
    /// Why the account stopped trading, if it has.
    pub halted: Option<&'a str>,
}

/// Answers one proposal against a stated account.
///
/// The whole of the risk policy, as a function. Every caller — live session,
/// backtest engine, test — comes through here, and that is what makes a stored
/// finding a statement about the system that will actually trade.
///
/// `now` is passed rather than read from a clock so this behaves identically in
/// a backtest, where "now" is a bar timestamp. A function calling `Utc::now()`
/// internally would be untestable and would differ between the two places it
/// has to be the same.
///
/// `correlations` may be `None` when no cap is configured. When one *is*
/// configured and this is `None`, the proposal is refused — see
/// [`Rejection::CorrelationUnknown`].
///
/// Checks run cheapest-first and in the order a person would ask them, so the
/// reported reason is the most fundamental one rather than whichever check
/// happened to be written last.
#[must_use]
pub fn decide(
    model: &RiskModel,
    account: &AccountState<'_>,
    proposal: &Proposal,
    now: NaiveDateTime,
    max_signal_age_ms: i64,
    correlations: Option<&dyn Correlations>,
) -> Decision {
    if let Some(reason) = account.halted {
        return Decision::Reject(Rejection::Halted {
            reason: reason.to_owned(),
        });
    }

    // Before anything else: a stale signal is not a smaller opportunity, it is
    // a different market. Sizing it correctly would be sizing the wrong trade
    // correctly.
    //
    // In a backtest this never fires, because a bar's signal is generated at
    // the bar's own instant and there is no network in between. That is not the
    // check being useless — it is the precise reason paper trading exists, and
    // the gap a backtest cannot measure about itself.
    let age_ms = (now - proposal.signalled_at).num_milliseconds();
    if age_ms > max_signal_age_ms {
        return Decision::Reject(Rejection::Stale {
            age_ms,
            limit_ms: max_signal_age_ms,
        });
    }

    if let Some(held) = account.positions.get(&proposal.instrument) {
        return Decision::Reject(Rejection::AlreadyHeld {
            quantity: held.quantity,
        });
    }

    if let Some(limit) = model.max_daily_loss {
        let allowed = account.starting_cash * limit;
        let lost = -account.realised_today;
        if lost >= allowed {
            return Decision::Reject(Rejection::DailyLossLimit {
                lost,
                limit: allowed,
            });
        }
    }

    if let Some(limit) = model.max_concurrent_positions {
        if account.positions.len() >= limit {
            return Decision::Reject(Rejection::TooManyPositions {
                held: account.positions.len(),
                limit,
            });
        }
    }

    if let Some(cap) = model.correlation_cap {
        if let Some(rejection) =
            correlation_check(account.positions, &proposal.instrument, cap, correlations)
        {
            return Decision::Reject(rejection);
        }
    }

    size(model, account.starting_cash, proposal)
}

/// Whether this instrument would over-fill a correlated cluster.
fn correlation_check(
    positions: &BTreeMap<String, Position>,
    instrument: &str,
    cap: CorrelationCap,
    correlations: Option<&dyn Correlations>,
) -> Option<Rejection> {
    let Some(correlations) = correlations else {
        // A configured cap with no way to evaluate it. Refusing is the only
        // honest answer: passing would leave the operator believing a limit is
        // in force that has never once been checked.
        return Some(Rejection::CorrelationUnknown {
            instrument: instrument.to_owned(),
            against: positions
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "nothing held".to_owned()),
        });
    };

    let mut clustered = Vec::new();
    for held in positions.keys() {
        match correlations.between(instrument, held) {
            Some(rho) if rho.abs() >= cap.above => clustered.push(held.clone()),
            Some(_) => {}
            // Unknown for this *pair* specifically. Same argument as above, and
            // it names the pair so it can be fixed.
            None => {
                return Some(Rejection::CorrelationUnknown {
                    instrument: instrument.to_owned(),
                    against: held.clone(),
                })
            }
        }
    }

    (clustered.len() >= cap.max_positions).then_some(Rejection::Correlated {
        with: clustered,
        above: cap.above,
        limit: cap.max_positions,
    })
}

/// How much of it to buy.
///
/// # Why this sizes off the opening balance and not current equity
///
/// Because the backtest does. `arvo-nautilus` resolves `risk_per_trade` and
/// `max_position_fraction` into currency amounts **once, before the run**, as
/// fractions of `starting_cash`. A live gate that sized off a moving equity
/// figure would compound where the backtest did not, and the two systems would
/// then differ in the one number that decides how much money is at stake —
/// which is exactly the divergence this whole design exists to prevent.
///
/// It also means an account that has lost money keeps proposing the same size.
/// That is the backtest's behaviour and it is deliberate here; the control that
/// is *supposed* to respond to losses is the drawdown halt, not a quiet
/// shrinking of every position.
///
/// The cap is not a refinement: position size is capital-at-risk over stop
/// distance, so a *tight* stop buys a *bigger* position, and without a ceiling a
/// five-minute ATR asks for several times the account. Whether the account can
/// actually pay is settled at the venue, in a backtest by the engine rejecting
/// the order and live by the broker doing the same.
fn size(model: &RiskModel, starting_cash: f64, proposal: &Proposal) -> Decision {
    if proposal.reference_price <= 0.0 {
        return Decision::Reject(Rejection::TooSmall { affordable: 0.0 });
    }

    let by_risk = match (model.risk_per_trade, proposal.stop_distance) {
        (Some(_), None | Some(0.0)) => return Decision::Reject(Rejection::NoStop),
        (Some(fraction), Some(distance)) => Some(starting_cash * fraction / distance),
        (None, _) => None,
    };

    let ceiling = model.max_position_fraction.unwrap_or(1.0);
    let by_cap = starting_cash * ceiling / proposal.reference_price;

    // Risk sizing wins where it applies; otherwise what was asked for; and the
    // cap bounds either. A proposer's desired size is a request, never a
    // permission.
    let wanted = by_risk.or(proposal.desired_quantity).unwrap_or(by_cap);
    let quantity = wanted.min(by_cap).floor();
    if quantity < 1.0 {
        return Decision::Reject(Rejection::TooSmall { affordable: by_cap });
    }
    Decision::Accept { quantity }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, d).expect("valid")
    }

    fn at(d: u32, hour: u32, minute: u32, second: u32) -> NaiveDateTime {
        day(d).and_hms_opt(hour, minute, second).expect("valid")
    }

    fn model() -> RiskModel {
        RiskModel {
            max_position_fraction: Some(1.0),
            ..RiskModel::default()
        }
    }

    fn gate(model: RiskModel) -> RiskGate {
        RiskGate::new(model, 2_000.0, day(9))
    }

    fn proposal(instrument: &str) -> Proposal {
        Proposal {
            instrument: instrument.to_owned(),
            proposer: "technical".to_owned(),
            signalled_at: at(9, 14, 30, 0),
            reference_price: 100.0,
            stop_distance: Some(2.0),
            desired_quantity: None,
        }
    }

    /// The instant a proposal is judged at: the moment it was signalled.
    fn immediately(proposal: &Proposal) -> NaiveDateTime {
        proposal.signalled_at
    }

    struct Fixed(f64);
    impl Correlations for Fixed {
        fn between(&self, _: &str, _: &str) -> Option<f64> {
            Some(self.0)
        }
    }

    struct Unknown;
    impl Correlations for Unknown {
        fn between(&self, _: &str, _: &str) -> Option<f64> {
            None
        }
    }

    #[test]
    fn an_ordinary_proposal_is_sized_and_accepted() {
        let gate = gate(model());
        let proposal = proposal("MSFT.RH");
        let Decision::Accept { quantity } = gate.propose(&proposal, immediately(&proposal), None)
        else {
            panic!("a first proposal on an untouched account should pass");
        };
        // $2,000 at $100, capped at one whole account.
        assert!((quantity - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_signal_older_than_the_window_is_refused_rather_than_traded_late() {
        // Entering a momentum break late is a loss, not a delay. This is the
        // control that stops an LLM or an alert pipeline trading a market that
        // has already moved on.
        let gate = gate(model());
        let proposal = proposal("MSFT.RH");
        let late = proposal.signalled_at + chrono::Duration::milliseconds(501);

        assert_eq!(
            gate.propose(&proposal, late, None),
            Decision::Reject(Rejection::Stale {
                age_ms: 501,
                limit_ms: 500,
            })
        );
    }

    #[test]
    fn a_signal_inside_the_window_still_passes() {
        let gate = gate(model());
        let proposal = proposal("MSFT.RH");
        let just_in_time = proposal.signalled_at + chrono::Duration::milliseconds(499);
        assert!(matches!(
            gate.propose(&proposal, just_in_time, None),
            Decision::Accept { .. }
        ));
    }

    #[test]
    fn the_staleness_window_is_configurable_because_horizons_differ() {
        // A daily-rebalance proposal is not stale at five seconds and a scalp
        // is stale at fifty milliseconds.
        let gate = gate(model()).with_max_signal_age_ms(50);
        let proposal = proposal("MSFT.RH");
        let late = proposal.signalled_at + chrono::Duration::milliseconds(51);
        assert!(matches!(
            gate.propose(&proposal, late, None),
            Decision::Reject(Rejection::Stale { .. })
        ));
    }

    #[test]
    fn two_proposers_cannot_both_open_the_same_instrument() {
        // The hazard the gate exists for. Each path sizes against its own idea
        // of the account, so without one authority this is two full positions
        // in one name.
        let mut gate = gate(model());
        let first = proposal("MSFT.RH");
        let Decision::Accept { quantity } = gate.propose(&first, immediately(&first), None) else {
            panic!("first proposal passes");
        };
        gate.opened("MSFT.RH", quantity, 100.0);

        let mut second = proposal("MSFT.RH");
        second.proposer = "alert".to_owned();
        assert_eq!(
            gate.propose(&second, immediately(&second), None),
            Decision::Reject(Rejection::AlreadyHeld { quantity: 20.0 })
        );
    }

    #[test]
    fn the_concurrent_position_cap_counts_what_the_account_holds() {
        let mut gate = gate(RiskModel {
            max_concurrent_positions: Some(2),
            ..model()
        });
        gate.opened("MSFT.RH", 5.0, 100.0);
        gate.opened("AAPL.RH", 5.0, 100.0);

        let third = proposal("NVDA.RH");
        assert_eq!(
            gate.propose(&third, immediately(&third), None),
            Decision::Reject(Rejection::TooManyPositions { held: 2, limit: 2 })
        );
    }

    #[test]
    fn the_daily_loss_limit_stops_trading_for_the_day() {
        let mut gate = gate(RiskModel {
            max_daily_loss: Some(0.02),
            ..model()
        });
        gate.opened("MSFT.RH", 20.0, 100.0);
        // $40 lost against a $40 limit on $2,000.
        gate.closed("MSFT.RH", -40.0, day(9));

        let next = proposal("AAPL.RH");
        assert_eq!(
            gate.propose(&next, immediately(&next), None),
            Decision::Reject(Rejection::DailyLossLimit {
                lost: 40.0,
                limit: 40.0,
            })
        );
    }

    #[test]
    fn the_daily_limit_lifts_the_next_day_and_the_drawdown_halt_does_not() {
        // The distinction between the two controls. One is a rule about how bad
        // a day may get; the other is a conclusion that the idea is wrong.
        let mut gate = gate(RiskModel {
            max_daily_loss: Some(0.02),
            max_drawdown: Some(0.10),
            ..model()
        });
        gate.opened("MSFT.RH", 20.0, 100.0);
        gate.closed("MSFT.RH", -40.0, day(9));

        let tomorrow = Proposal {
            signalled_at: at(10, 14, 30, 0),
            ..proposal("AAPL.RH")
        };
        assert!(
            matches!(
                gate.propose(&tomorrow, immediately(&tomorrow), None),
                Decision::Accept { .. }
            ),
            "a new day resets the daily limit"
        );

        // Now breach the drawdown halt instead.
        gate.mark(1_700.0);
        assert!(gate.halted().is_some());
        let after = Proposal {
            signalled_at: at(11, 14, 30, 0),
            ..proposal("NVDA.RH")
        };
        assert!(
            matches!(
                gate.propose(&after, immediately(&after), None),
                Decision::Reject(Rejection::Halted { .. })
            ),
            "the halt does not lift with the date"
        );
    }

    #[test]
    fn the_drawdown_halt_sees_open_positions_move_against_the_account() {
        // A halt counting only realised losses would let an account fall to
        // nothing while holding.
        let mut gate = gate(RiskModel {
            max_drawdown: Some(0.10),
            ..model()
        });
        gate.opened("MSFT.RH", 20.0, 100.0);
        assert!(gate.halted().is_none());
        gate.mark(1_799.0);
        assert!(gate.halted().is_some(), "unrealised losses count");
    }

    #[test]
    fn correlated_names_count_as_one_bet() {
        let mut gate = gate(RiskModel {
            correlation_cap: Some(CorrelationCap {
                above: 0.8,
                max_positions: 1,
            }),
            ..model()
        });
        gate.opened("QQQ.RH", 5.0, 100.0);

        let tqqq = proposal("TQQQ.RH");
        let Decision::Reject(Rejection::Correlated { with, .. }) =
            gate.propose(&tqqq, immediately(&tqqq), Some(&Fixed(0.97)))
        else {
            panic!("a 0.97-correlated name is the same bet");
        };
        assert_eq!(with, vec!["QQQ.RH".to_owned()]);
    }

    #[test]
    fn an_uncorrelated_name_is_a_new_bet_and_passes() {
        let mut gate = gate(RiskModel {
            correlation_cap: Some(CorrelationCap {
                above: 0.8,
                max_positions: 1,
            }),
            ..model()
        });
        gate.opened("QQQ.RH", 5.0, 100.0);

        let gold = proposal("GLD.RH");
        assert!(matches!(
            gate.propose(&gold, immediately(&gold), Some(&Fixed(0.05))),
            Decision::Accept { .. }
        ));
    }

    #[test]
    fn a_cap_that_cannot_be_evaluated_refuses_rather_than_waving_through() {
        // An unenforceable limit that silently allows everything is worse than
        // no limit, because the operator believes they have one.
        let mut gate = gate(RiskModel {
            correlation_cap: Some(CorrelationCap {
                above: 0.8,
                max_positions: 1,
            }),
            ..model()
        });
        gate.opened("QQQ.RH", 5.0, 100.0);
        let next = proposal("TQQQ.RH");

        assert!(matches!(
            gate.propose(&next, immediately(&next), None),
            Decision::Reject(Rejection::CorrelationUnknown { .. })
        ));
        assert!(
            matches!(
                gate.propose(&next, immediately(&next), Some(&Unknown)),
                Decision::Reject(Rejection::CorrelationUnknown { .. })
            ),
            "a source that does not know this pair is not a source that says zero"
        );
    }

    #[test]
    fn no_cap_configured_needs_no_correlation_source() {
        let gate = gate(model());
        let next = proposal("MSFT.RH");
        assert!(matches!(
            gate.propose(&next, immediately(&next), None),
            Decision::Accept { .. }
        ));
    }

    #[test]
    fn risk_sizing_without_a_stop_is_refused_rather_than_invented() {
        let gate = gate(RiskModel {
            risk_per_trade: Some(0.01),
            stop_atr_multiple: Some(2.0),
            ..model()
        });
        let mut naked = proposal("MSFT.RH");
        naked.stop_distance = None;
        assert_eq!(
            gate.propose(&naked, immediately(&naked), None),
            Decision::Reject(Rejection::NoStop)
        );
    }

    #[test]
    fn a_tight_stop_does_not_buy_more_than_the_account() {
        // The bug the position cap exists for: size is capital-at-risk over
        // stop distance, so a tight stop asks for a bigger position. On
        // five-minute bars this asks for several times the account.
        let gate = gate(RiskModel {
            risk_per_trade: Some(0.01),
            stop_atr_multiple: Some(2.0),
            max_position_fraction: Some(1.0),
            ..model()
        });
        let mut scalp = proposal("MSFT.RH");
        scalp.stop_distance = Some(0.01);

        let Decision::Accept { quantity } = gate.propose(&scalp, immediately(&scalp), None) else {
            panic!("it should still trade, just not for more than it has");
        };
        assert!(
            quantity * scalp.reference_price <= 2_000.0,
            "sized {quantity} at {} = {}, more than the account",
            scalp.reference_price,
            quantity * scalp.reference_price
        );
    }

    #[test]
    fn a_desired_size_is_honoured_up_to_the_cap_and_no_further() {
        // The backtest engine trades a fixed quantity when no risk sizing
        // applies. Without this the same rule would trade 100 shares in a
        // backtest and the whole account live, which is the divergence the
        // shared policy exists to prevent.
        let gate = gate(model());
        let modest = Proposal {
            desired_quantity: Some(5.0),
            ..proposal("MSFT.RH")
        };
        assert_eq!(
            gate.propose(&modest, immediately(&modest), None),
            Decision::Accept { quantity: 5.0 }
        );

        let greedy = Proposal {
            desired_quantity: Some(1_000.0),
            ..proposal("MSFT.RH")
        };
        let Decision::Accept { quantity } = gate.propose(&greedy, immediately(&greedy), None) else {
            panic!("it is capped, not refused");
        };
        assert!((quantity - 20.0).abs() < 1e-9, "the cap bounds the request");
    }

    #[test]
    fn an_account_too_small_for_one_unit_is_told_so() {
        // Named rather than returned as a zero quantity, which every caller
        // would have to remember to check and one of them would not.
        let gate = RiskGate::new(model(), 2_000.0, day(9));
        let expensive = Proposal {
            reference_price: 5_000.0,
            ..proposal("BRK-A.RH")
        };
        assert!(matches!(
            gate.propose(&expensive, immediately(&expensive), None),
            Decision::Reject(Rejection::TooSmall { .. })
        ));
    }

    #[test]
    fn the_gate_books_fills_rather_than_its_own_acceptances() {
        // A proposal that is accepted may still not fill. A gate that assumed
        // otherwise would refuse trades on exposure the account does not have.
        let gate = gate(RiskModel {
            max_concurrent_positions: Some(1),
            ..model()
        });
        let first = proposal("MSFT.RH");
        assert!(matches!(
            gate.propose(&first, immediately(&first), None),
            Decision::Accept { .. }
        ));

        // Nothing filled, so nothing is held, so a second name still passes.
        let second = proposal("AAPL.RH");
        assert!(matches!(
            gate.propose(&second, immediately(&second), None),
            Decision::Accept { .. }
        ));
        assert!(gate.positions().is_empty());
    }

    #[test]
    fn a_limit_that_can_never_bind_or_always_binds_is_refused() {
        // Every other field in the model is validated; these two were not, so a
        // daily limit of 150% or a correlation cap of zero would have been
        // accepted and then quietly done nothing, or refused everything.
        let bad_daily = RiskModel {
            max_daily_loss: Some(1.5),
            ..RiskModel::default()
        };
        assert!(bad_daily.check().is_err());

        let zero_cap = RiskModel {
            correlation_cap: Some(CorrelationCap {
                above: 0.8,
                max_positions: 0,
            }),
            ..RiskModel::default()
        };
        assert!(
            zero_cap.check().is_err(),
            "a cap of zero refuses every correlated trade; remove the cap instead"
        );

        let impossible_rho = RiskModel {
            correlation_cap: Some(CorrelationCap {
                above: 1.7,
                max_positions: 1,
            }),
            ..RiskModel::default()
        };
        assert!(impossible_rho.check().is_err(), "no pair correlates above 1");
    }

    #[test]
    fn a_workable_limit_passes_validation() {
        let model = RiskModel {
            max_daily_loss: Some(0.02),
            correlation_cap: Some(CorrelationCap {
                above: 0.8,
                max_positions: 1,
            }),
            ..RiskModel::default()
        };
        assert!(model.check().is_ok());
    }
}
