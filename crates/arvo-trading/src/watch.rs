//! The book, audited against the venue each time round the loop.
//!
//! Round trips are built from fills rather than counted separately, so the
//! divergence between what the backtest assumed and what the venue did cannot
//! drift from the fills it was derived from.

use std::collections::BTreeMap;

use arvo_execution::{Divergence, Execution};
use arvo_research::live::{judge, Expectation, Live, Observed};
use arvo_risk::Warning;

use crate::state::Warned;

/// The session's own ledger, kept for its verdict (#221).
///
/// Built from the fills as they settle: a buy opens or adds to the one
/// position a session holds, a sell realises against its average entry,
/// and the position going flat closes a round trip. The regime on each
/// entry is the one its signal carried, remembered by order id when the
/// order was sent. A sell of something this session did not open (an
/// adopted position) is not a round trip of the rule's and is not counted.
pub(crate) struct Watch {
    pub(crate) expected: Option<Expectation>,
    pub(crate) seen: Observed,
    /// Order id to the regime its signal was decided in.
    pub(crate) regimes: BTreeMap<String, Option<String>>,
    /// How many of the session's executions are already folded in.
    pub(crate) folded: usize,
    pub(crate) quantity: f64,
    /// Average entry of what is held.
    pub(crate) entry: f64,
    pub(crate) realised: f64,
    /// Realised so far in the open round trip.
    pub(crate) round_trip: f64,
    pub(crate) peak: f64,
    pub(crate) starting_cash: f64,
    /// What the account is worth at the last mark.
    pub(crate) equity: f64,
    pub(crate) verdict: Live,
    /// The limits the session was near at the last look, by name (#191).
    pub(crate) near: Vec<String>,
}

impl Watch {
    pub(crate) fn new(expected: Option<Expectation>, starting_cash: f64) -> Self {
        Self {
            expected,
            seen: Observed::default(),
            regimes: BTreeMap::new(),
            folded: 0,
            quantity: 0.0,
            entry: 0.0,
            realised: 0.0,
            round_trip: 0.0,
            peak: starting_cash,
            starting_cash,
            equity: starting_cash,
            verdict: Live::Inconclusive,
            near: Vec::new(),
        }
    }

    pub(crate) fn equity(&self) -> f64 {
        self.equity
    }

    /// The change in which limits the session is near, if any. Compared
    /// by limit, not by figure: the figure moves on every bar, and a record
    /// that logged each move would bury the one line that matters.
    pub(crate) fn warned(&mut self, warnings: &[Warning]) -> Option<Warned> {
        let names: Vec<String> = warnings
            .iter()
            .map(|warning| warning.limit.clone())
            .collect();
        if names == self.near {
            return None;
        }
        let entered = names
            .iter()
            .filter(|name| !self.near.contains(name))
            .cloned()
            .collect();
        let cleared = self
            .near
            .iter()
            .filter(|name| !names.contains(name))
            .cloned()
            .collect();
        self.near = names;
        Some(Warned {
            entered,
            cleared,
            now: warnings.iter().map(ToString::to_string).collect(),
        })
    }

    pub(crate) fn entered(&mut self, order: String, regime: Option<String>) {
        self.regimes.insert(order, regime);
    }

    pub(crate) fn bar(&mut self, close: f64) {
        self.seen.bars += 1;
        self.mark(close);
    }

    pub(crate) fn settle(
        &mut self,
        executions: &[Execution],
        divergence: &Divergence,
        close: Option<f64>,
    ) {
        for execution in &executions[self.folded.min(executions.len())..] {
            match execution.side {
                arvo_execution::Side::Buy => {
                    if self.quantity <= 0.0 {
                        self.seen
                            .entries
                            .push(self.regimes.remove(&execution.order.to_string()).flatten());
                        self.round_trip = 0.0;
                    }
                    let total = self.quantity + execution.quantity;
                    self.entry = (self.entry * self.quantity
                        + execution.fill_price * execution.quantity)
                        / total;
                    self.quantity = total;
                }
                arvo_execution::Side::Sell => {
                    if self.quantity <= 0.0 {
                        continue;
                    }
                    let sold = execution.quantity.min(self.quantity);
                    let pnl = (execution.fill_price - self.entry) * sold;
                    self.realised += pnl;
                    self.round_trip += pnl;
                    self.quantity -= sold;
                    if self.quantity <= 1e-9 {
                        self.quantity = 0.0;
                        self.seen.pnls.push(self.round_trip);
                        self.round_trip = 0.0;
                    }
                }
            }
        }
        self.folded = executions.len();
        self.seen.fills = divergence.fills;
        self.seen.mean_slippage_bps = divergence.mean_slippage_bps;
        if let Some(close) = close {
            self.mark(close);
        }
    }

    /// Marks what is held at the last close, so the drawdown sees an open
    /// loss and not only realised ones.
    pub(crate) fn mark(&mut self, close: f64) {
        let equity = self.starting_cash + self.realised + self.quantity * (close - self.entry);
        self.equity = equity;
        self.peak = self.peak.max(equity);
        if self.starting_cash > 0.0 {
            self.seen.drawdown = self
                .seen
                .drawdown
                .max((self.peak - equity) / self.starting_cash);
        }
    }

    /// The verdict, when it changed.
    pub(crate) fn judge(&mut self) -> Option<&Live> {
        let now = self
            .expected
            .as_ref()
            .map_or(Live::Inconclusive, |expected| judge(expected, &self.seen));
        if now == self.verdict {
            return None;
        }
        self.verdict = now;
        Some(&self.verdict)
    }
}
