//! The risk policy, as one function over a proposal and a stated account.

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use super::pdt::{DayTradingRule, PDT_DAY_TRADES, PDT_EQUITY_FLOOR};
use super::sizing::size;
use super::{CorrelationCap, Correlations, SectorCap};
use crate::{CostModel, RiskModel};

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
    /// Whether this sells to open rather than buys (#84).
    ///
    /// Only an option contract may be sold short in a cash account, and only
    /// with the cash to cover what it can lose at expiry already in hand — see
    /// [`crate::collateral`].
    pub opens_short: bool,
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
    /// The account stopped trading: either it breached
    /// [`RiskModel::max_drawdown`], or someone pulled the kill switch. Which
    /// one decides whether it can resume — see [`Halt::manual`].
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
    /// The sector this name is in already holds as many positions as it may.
    Sector {
        sector: String,
        with: Vec<String>,
        limit: usize,
    },
    /// A sector cap is configured and has no sector for this name, or for one
    /// already held.
    ///
    /// Refused for the reason [`Self::CorrelationUnknown`] is: a name with no
    /// label could be in any sector, including the full one.
    SectorUnknown { instrument: String },
    /// Opening this would risk a fourth day trade in five business days on an
    /// account below the pattern-day-trader floor.
    ///
    /// # Why this refuses an entry rather than the exit that would trip it
    ///
    /// Because the trade that flags an account is the *closing* one, and an
    /// exit may never be refused — a gate that blocked a close would leave the
    /// account holding something it had decided to be out of, which is the
    /// failure [`RiskGate`] exists to prevent, not cause.
    ///
    /// So the constraint bites earlier: with the budget used up, do not open
    /// what you may need to close today. That refuses some positions that would
    /// have been held overnight and never counted, which is the conservative
    /// direction and the only one available given the exit rule.
    PatternDayTrader {
        used: usize,
        limit: usize,
        equity: f64,
        floor: f64,
    },
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
    /// Selling this to open would leave an expiration net short calls, which
    /// loses without limit and so cannot be secured by any amount of cash.
    Uncovered {
        underlying: String,
        expiration: NaiveDate,
    },
    /// A cash account cannot borrow shares to sell. Only option contracts are
    /// sold to open, against cash.
    CannotShort { instrument: String },
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Send this many units, and no other number.
    Accept { quantity: f64 },
    Reject(Rejection),
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
    /// What the account is worth now, marked to market.
    ///
    /// Separate from `starting_cash` because the pattern-day-trader rule tests
    /// *current* equity against its floor, not the balance you opened with.
    pub equity: f64,
    /// Day trades already **committed** in the trailing window: round trips
    /// opened and closed on one day, plus positions opened today that are
    /// still open.
    ///
    /// The open ones count because each becomes a day trade the moment it is
    /// closed, and closing cannot be refused. Counting only completed round
    /// trips lets two positions opened before the budget filled each become a
    /// day trade on the way out — which is how a $2,000 account reached four in
    /// a window while every individual entry was permitted.
    pub day_trades_used: usize,
    /// Why the account stopped trading, if it has.
    pub halted: Option<&'a str>,
    /// Cash the account could pay with right now. `None` when the caller
    /// cannot say, which leaves the cash ceiling unapplied.
    ///
    /// Sizing comes off `starting_cash` on purpose — see [`size`] — so without
    /// this an account that had lost anything kept proposing entries it could
    /// not pay for. The venue refused them, and on two years of five-minute
    /// AAPL an opening-range rule took 3 of 72 breakouts. Risk still sizes off
    /// the opening balance; this only stops a proposal asking for money that
    /// is not there.
    pub spendable: Option<f64>,
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
/// `costs` is what a fill is assumed to cost on top of its price, so an entry
/// is never sized at a notional the account can reach and its commission and
/// slippage cannot. `None` sizes on price alone.
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
    costs: Option<&CostModel>,
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

    // Before the position cap and after the daily loss limit: this is a rule
    // about what the account is *allowed* to do rather than what it can afford,
    // and a reader who hit both should hear the legal one.
    if model.day_trading == DayTradingRule::PatternDayTrader
        && account.equity < PDT_EQUITY_FLOOR
        && account.day_trades_used >= PDT_DAY_TRADES
    {
        return Decision::Reject(Rejection::PatternDayTrader {
            used: account.day_trades_used,
            limit: PDT_DAY_TRADES,
            equity: account.equity,
            floor: PDT_EQUITY_FLOOR,
        });
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

    if let Some(cap) = &model.sector_cap {
        if let Some(rejection) = sector_check(account.positions, &proposal.instrument, cap) {
            return Decision::Reject(rejection);
        }
    }

    size(model, account, proposal, costs)
}

/// Whether this instrument would over-fill its sector.
fn sector_check(
    positions: &BTreeMap<String, Position>,
    instrument: &str,
    cap: &SectorCap,
) -> Option<Rejection> {
    let sector_of = |id: &str| cap.sectors.get(arvo_data::source::symbol_of(id));
    let unknown = |id: &str| Rejection::SectorUnknown {
        instrument: id.to_owned(),
    };

    let Some(sector) = sector_of(instrument) else {
        return Some(unknown(instrument));
    };
    let mut with = Vec::new();
    for held in positions.keys() {
        match sector_of(held) {
            Some(theirs) if theirs == sector => with.push(held.clone()),
            Some(_) => {}
            None => return Some(unknown(held)),
        }
    }
    (with.len() >= cap.max_positions).then(|| Rejection::Sector {
        sector: sector.clone(),
        with,
        limit: cap.max_positions,
    })
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
