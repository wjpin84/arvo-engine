//! The live risk authority: an account's own book, answered through `decide`.

use std::collections::BTreeMap;

use arvo_data::Instrument;
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use super::decide::{decide, AccountState, Decision, Position, Proposal};
use super::pdt::{business_days_before, PDT_WINDOW_DAYS};
use super::{Correlations, DEFAULT_MAX_SIGNAL_AGE_MS};
use crate::RiskModel;

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
    /// What the sources said about the instruments this account trades
    /// (#186). Anything not here is sized on what its name says.
    instruments: BTreeMap<String, Instrument>,
    /// When each held position was opened, so a close on the same day can be
    /// recognised as a day trade. Kept beside the book rather than on
    /// `Position`, which is also what the engine hands in and has no opening
    /// date to give.
    opened_on: BTreeMap<String, NaiveDate>,
    /// Dates on which a round trip opened and closed. The pattern-day-trader
    /// count is these, inside the trailing window.
    day_trades: Vec<NaiveDate>,
    /// Realised profit and loss booked today. Negative is a loss.
    realised_today: f64,
    today: NaiveDate,
    halted: Option<Halt>,
    max_signal_age_ms: i64,
    /// What a fill costs on top of its price, so an entry is sized all in.
    /// `None` sizes on price alone.
    costs: Option<crate::CostModel>,
}

/// How close to a limit counts as near it (#191).
///
/// One band, not a scale: at four fifths of any limit the gate says so,
/// on the record and on the status, so a session can be watched before it
/// is stopped. Below it there is nothing to say; at the limit the gate
/// refuses or halts, which is the record's business already.
pub const WARNING_FRACTION: f64 = 0.8;

/// A limit the account is near, with how much of it is used (#191).
///
/// Not a refusal and not a halt: the gate keeps accepting. It is the one
/// thing a person can act on before the gate acts for them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    /// `drawdown`, `daily loss`, `positions` or `day trades`.
    pub limit: String,
    /// What is used, in the limit's own unit: a fraction for the drawdown,
    /// money for the daily loss, a count for the rest.
    pub used: f64,
    pub allowed: f64,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.limit.as_str() {
            "drawdown" => write!(f, "drawdown {:.1}% of a {:.1}% limit", self.used * 100.0, self.allowed * 100.0),
            "daily loss" => write!(f, "daily loss {:.2} of a {:.2} limit", self.used, self.allowed),
            other => write!(f, "{other} {} of {}", self.used, self.allowed),
        }
    }
}

/// Why an account stopped trading, and whether it can be started again.
///
/// Two halts share one field because [`decide`] must treat them identically —
/// a stopped account is stopped, whatever stopped it. They differ only in who
/// may lift them, which is [`RiskGate::release`]'s problem and nothing else's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Halt {
    pub reason: String,
    /// Whether a person pulled it, and can therefore put it back.
    ///
    /// The drawdown halt is deliberately permanent for the session: a gate that
    /// stopped trading cannot recover the equity that would let it resume, so
    /// "halt until recovered" either never resumes or has to keep trading to
    /// find out. A kill switch is the opposite — someone decided, and someone
    /// can decide again — and conflating the two would let a person clear a
    /// drawdown halt by pressing the button that arms and then releases.
    pub manual: bool,
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
            instruments: BTreeMap::new(),
            opened_on: BTreeMap::new(),
            day_trades: Vec::new(),
            realised_today: 0.0,
            today,
            halted: None,
            max_signal_age_ms: DEFAULT_MAX_SIGNAL_AGE_MS,
            costs: None,
        }
    }

    /// Sizes entries against what a fill is assumed to cost, as the backtest
    /// that produced the strategy did. See ADR-0015.
    #[must_use]
    pub const fn with_costs(mut self, costs: crate::CostModel) -> Self {
        self.costs = Some(costs);
        self
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
        self.halted.as_ref().map(|halt| halt.reason.as_str())
    }

    /// The halt itself, for a caller that needs to know whether it can lift.
    #[must_use]
    pub const fn halt(&self) -> Option<&Halt> {
        self.halted.as_ref()
    }

    /// Stops the account on a person's instruction — the kill switch.
    ///
    /// Refuses every subsequent proposal through the ordinary halt check, so
    /// there is no second code path to keep in step with the drawdown halt.
    ///
    /// **It does not flatten.** Arming and exiting are separate because the
    /// gate cannot reach a venue; `arvo_execution::Session::kill` does both, in
    /// that order. Arming first is the point: anything racing in behind the
    /// button is refused, and the exits do not go through the gate anyway.
    ///
    /// An account already halted is left as it was. Arming on top of a drawdown
    /// halt would otherwise make that halt releasable by pressing one button
    /// twice.
    pub fn kill(&mut self, reason: &str) {
        if self.halted.is_none() {
            self.halted = Some(Halt {
                reason: reason.to_owned(),
                manual: true,
            });
        }
    }

    /// Lifts a kill switch. Returns whether it lifted.
    ///
    /// `false` means the account is halted for a reason a person did not choose
    /// and cannot unchoose — read [`Self::halted`] for it. Returning `false`
    /// rather than lifting anyway is the whole reason [`Halt::manual`] exists.
    pub fn release(&mut self) -> bool {
        if self.halted.as_ref().is_some_and(|halt| halt.manual) {
            self.halted = None;
            return true;
        }
        false
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
        self.propose_within(proposal, now, correlations, None)
    }

    /// Tells the gate what an instrument's source says it is — its lot,
    /// tick, hours — so proposals on it are sized against that rather than
    /// against its name.
    pub fn learn(&mut self, instrument: Instrument) {
        self.instruments.insert(instrument.id.clone(), instrument);
    }

    /// As [`Self::propose`], with the cash the account can spend right now.
    ///
    /// The book is the gate's; the cash is the venue's. A gate that tracked
    /// cash itself would be a second running total of something the broker
    /// already holds — so the caller asks the broker and hands it in, and
    /// `None` leaves the ceiling off.
    #[must_use]
    pub fn propose_within(
        &self,
        proposal: &Proposal,
        now: NaiveDateTime,
        correlations: Option<&dyn Correlations>,
        spendable: Option<f64>,
    ) -> Decision {
        let instrument = self
            .instruments
            .get(&proposal.instrument)
            .cloned()
            .unwrap_or_else(|| Instrument::of(&proposal.instrument));
        decide(
            &self.model,
            &AccountState {
                positions: &self.positions,
                realised_today: self.realised_on(now.date()),
                starting_cash: self.starting_cash,
                equity: self.equity,
                day_trades_used: self.day_trades_used(now.date()),
                halted: self.halted(),
                spendable,
            },
            proposal,
            &instrument,
            now,
            self.max_signal_age_ms,
            correlations,
            self.costs.as_ref(),
        )
    }

    /// Records that a position was opened.
    ///
    /// Separate from [`Self::propose`] because a proposal that is accepted may
    /// still not fill. Book what the broker confirms, never what was sent — a
    /// gate that assumed its own acceptances became positions would refuse
    /// trades on exposure the account does not have.
    pub fn opened(&mut self, instrument: &str, quantity: f64, entry: f64, on: NaiveDate) {
        self.positions
            .insert(instrument.to_owned(), Position { quantity, entry });
        self.opened_on.insert(instrument.to_owned(), on);
    }

    /// Day trades committed inside the trailing window.
    ///
    /// Completed round trips plus positions opened today and still open. See
    /// [`AccountState::day_trades_used`] for why the open ones count.
    #[must_use]
    pub fn day_trades_used(&self, on: NaiveDate) -> usize {
        let earliest = business_days_before(on, PDT_WINDOW_DAYS);
        let completed = self
            .day_trades
            .iter()
            .filter(|at| **at >= earliest && **at <= on)
            .count();
        let committed = self
            .opened_on
            .values()
            .filter(|opened| **opened == on)
            .count();
        completed + committed
    }

    /// Records that a position closed, booking its realised profit or loss.
    ///
    /// Feeds both the daily loss limit and the drawdown halt, which is why it
    /// must be called on every close including a stop-out.
    pub fn closed(&mut self, instrument: &str, pnl: f64, on: NaiveDate) {
        self.positions.remove(instrument);
        // Opened and closed the same day is a day trade, whatever the rule
        // decides to do about it.
        if self.opened_on.remove(instrument) == Some(on) {
            self.day_trades.push(on);
        }
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
                    self.halted = Some(Halt {
                        reason: format!(
                            "account fell {:.1}% below its peak, against a {:.1}% limit",
                            depth * 100.0,
                            limit * 100.0
                        ),
                        manual: false,
                    });
                }
            }
        }
    }

    /// The limits the account is within [`WARNING_FRACTION`] of, on `day`
    /// (#191). Empty while halted: a halt is past warning.
    ///
    /// Read against the same numbers [`decide`] refuses on, so a warning
    /// here and a refusal there cannot disagree about where the line is.
    #[must_use]
    pub fn warnings(&self, day: NaiveDate) -> Vec<Warning> {
        let mut found = Vec::new();
        if self.halted.is_some() {
            return found;
        }
        // A hair under the band counts as in it, so 8% of 10% is not decided by
        // the last bit of a float.
        let near = |used: f64, allowed: f64| allowed > 0.0 && used >= allowed * WARNING_FRACTION - 1e-9;
        if let Some(limit) = self.model.max_drawdown {
            if self.peak_equity > 0.0 {
                let depth = (self.peak_equity - self.equity) / self.peak_equity;
                if near(depth, limit) {
                    found.push(Warning { limit: "drawdown".to_owned(), used: depth, allowed: limit });
                }
            }
        }
        if let Some(limit) = self.model.max_daily_loss {
            let allowed = self.starting_cash * limit;
            let lost = -self.realised_on(day);
            if near(lost, allowed) {
                found.push(Warning { limit: "daily loss".to_owned(), used: lost, allowed });
            }
        }
        if self.model.day_trading == super::pdt::DayTradingRule::PatternDayTrader && self.equity < super::pdt::PDT_EQUITY_FLOOR {
            let used = self.day_trades_used(day);
            if near(used as f64, super::pdt::PDT_DAY_TRADES as f64) {
                found.push(Warning { limit: "day trades".to_owned(), used: used as f64, allowed: super::pdt::PDT_DAY_TRADES as f64 });
            }
        }
        if let Some(limit) = self.model.max_concurrent_positions {
            let held = self.positions.len();
            if near(held as f64, limit as f64) {
                found.push(Warning { limit: "positions".to_owned(), used: held as f64, allowed: limit as f64 });
            }
        }
        found
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
