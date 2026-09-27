//! How a session reaches a venue.
//!
//! A session is named `finding@executor`, and something has to turn that second
//! half into a real source of bars and a real place to send orders. Until
//! 2026-09-26 [`crate::run`] did it itself, with a `match` over executor names
//! that constructed Alpaca and Robinhood clients inline. That is why the loop
//! could not be tested: there was no way to run it without reaching a broker,
//! so every bug in the *wiring* — as opposed to the helpers, which were
//! unit-tested and correct — had to be found in a live session. Two were, in
//! one week:
//!
//! * **#224** — nothing consulted [`crate::bar::caught_up`] before sending, so a
//!   mid-day start replayed yesterday's bars and sent the entry. `caught_up`
//!   itself was correct and tested.
//! * **#231** — the loop stamped an exit with the *bar's* time instead of the
//!   moment it was sent, so the order was already past the venue's staleness
//!   limit on arrival and the engine cancelled its own sells. The timestamp
//!   helper was correct too.
//!
//! Both were failures of what calls what. Both are now tests, because this
//! trait lets a fake venue stand where a broker stood.
//!
//! # What is deliberately *not* here
//!
//! Whether an executor is paper. [`crate::promotion::is_paper`] decides that
//! from the name, inside this crate, and it stays there: an implementation of
//! this trait that could declare itself paper could walk a live account past
//! the promotion gate. A venue says what it is called and where it sends. It
//! does not get a say in whether real money needs a paper record first.

use arvo_execution::Executor;

/// The venues a session may be run against.
///
/// Implemented once for real brokers, in the engine that owns the broker
/// credentials, and once per test for a fake.
#[async_trait::async_trait]
pub trait Venues: Send + Sync {
    /// Whether `executor` is a name this set serves.
    ///
    /// Asked before a thread starts, so an unknown name is refused as a failed
    /// call rather than as a session that dies a moment later.
    #[must_use]
    fn serves(&self, executor: &str) -> bool;

    /// Every name this set serves, for the refusal message and the UI.
    #[must_use]
    fn names(&self) -> Vec<String>;

    /// Where `venue`'s bars come from.
    ///
    /// # Errors
    ///
    /// Returns the reason when nothing serves that venue.
    fn source(&self, venue: &str) -> Result<Box<dyn arvo_data::source::Source>, String>;

    /// Where `executor`'s orders go.
    ///
    /// Async because resolving one can take a round trip — a Robinhood account
    /// number is looked up from the last four digits in the name.
    ///
    /// # Errors
    ///
    /// Returns the reason when the executor cannot be reached or does not exist.
    async fn executor(&self, executor: &str) -> Result<Box<dyn Executor>, String>;
}
