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
//!
//! | module | what it holds |
//! |---|---|
//! | `model` | [`RiskModel`], the limits an experiment pins |
//! | `decide` | [`decide`]: a proposal against a stated account |
//! | `sizing` | how much of an accepted proposal to send |
//! | `gate` | [`RiskGate`], a live account's own book around `decide` |
//! | `pdt` | the pattern-day-trader rule |

mod decide;
mod gate;
mod model;
mod pdt;
mod sizing;

pub use decide::{decide, AccountState, Decision, Position, Proposal, Rejection};
pub use gate::{Halt, RiskGate};
pub use model::RiskModel;
pub use pdt::{
    business_days_before, day_trades_in_window, DayTradingRule, PDT_DAY_TRADES, PDT_EQUITY_FLOOR,
    PDT_WINDOW_DAYS,
};

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests;
