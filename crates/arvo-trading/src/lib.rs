//! Live sessions: a finding's rule, running against a venue.
//!
//! # Why this is a crate and not a module in the engine binary
//!
//! This loop is the only code in Arvo that sends real orders with real money
//! behind them, and it lived in `app/arvo-engine` — a binary — until 2026-09-26.
//! Nothing required that: it touches no transport, no tonic and no proto, only
//! the libraries below it. What it cost was testability, and the bill arrived
//! twice in one week. A mid-day start replayed yesterday's bars and *sent* the
//! entry (#224). Exits stamped with the bar's time went stale before reaching
//! the venue, so the engine cancelled its own sells while the rule believed it
//! was flat (#231). Both were found by watching a live session, not by a test.
//!
//! [`arvo_execution`] already states the principle this crate follows —
//! "a trading session must be runnable without a window" — and this is the
//! half of it that was outside. Here the loop is a library: depended on,
//! tested, and built by CI on both platforms.
//!
//! # What a session is
//!
//! One finding, one executor, one thread. The finding's experiment is rebuilt
//! as a shadow engine (`arvo_nautilus::Shadow`) warmed on the library up to
//! today; from then on the venue's own source is asked for bars at the
//! experiment's resolution, each completed bar is pushed through the shadow,
//! and what the rule sends comes back as signals. An entry goes to the risk
//! gate and, if it survives, to the venue; an exit goes straight to the venue
//! (ADR-0009: nothing may stop you shedding risk). The same policy, the same
//! rule, the same bars a backtest would have seen — that is the whole point.
//!
//! # Paper is a broker's paper account
//!
//! `alpaca-paper` is Alpaca's paper endpoint: real API, real fills and
//! latency, simulated money. That is what makes its divergence a measurement
//! rather than an assumption. `alpaca-live` and `robinhood` are real money and
//! say so by name.
//!
//! # Streamed when the source can, polled when it cannot
//!
//! An intraday rule asks the source for a live feed (`Source::stream`, #185)
//! and takes each bar as it closes; the poll still runs once a minute
//! underneath, catching up what the library lacked at start and anything a
//! feed dropped. A feed that goes dark freezes the session the way a book
//! disagreement does — entries wait, exits go — and the freeze lifts on its
//! own when the feed is back, because nothing about the book is in doubt.
//! A daily rule polls, as it always did; a minute either way is nothing to
//! a bar that closes overnight.
//!
//! # A bar is accepted only once it is over
//!
//! A vendor serves today's daily bar while today is still trading. Pushing it
//! would decide on a close that has not happened. A bar is pushed only when
//! `at + interval` is in the past, which for a daily rule means the signal
//! fires overnight and fills at the open — the fill the backtest assumed
//! (ADR-0010).
//!
//! # A disagreement with the venue is an incident, not a warning
//!
//! Every poll the gate's book is audited against the venue's (#187). A
//! position the venue reports and the gate does not, or the other way round,
//! means something traded that this rule did not decide, and the rule can no
//! longer size against a book it trusts. The session *freezes*: entries are
//! refused, exits still go out (ADR-0009), bars keep flowing so the rule
//! stays current, and the record names the disagreement. A person reconciles
//! — the gate is made to agree with the venue, since the venue holds the
//! money — and then resumes, and both are events in the record. Nothing
//! resumes on its own: a freeze that lifted itself would be a warning.
//!
//! # The record is a chain
//!
//! Each event names what caused it (#188): a signal its bar, an order its
//! signal, a fill its order and the position it left. `explain` walks it
//! backwards from a position to the bar it was decided on.
//!
//! # Nothing here writes the library
//!
//! Bars fetched for a session are pushed and forgotten. The library is fetched
//! files with a content hash (ADR-0008), and a session's bars have no place in
//! it.

mod bar;
mod promotion;
mod record;
mod run;
mod sessions;
mod state;
mod status;
mod watch;

#[cfg(test)]
mod tests;

pub use promotion::{Promotion, EXECUTORS, PAPER_MINIMUM_DAYS, SUBDIR};
pub use record::record_path;
pub use sessions::Sessions;
pub use status::Status;
