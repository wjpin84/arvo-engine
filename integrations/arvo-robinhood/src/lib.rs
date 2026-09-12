//! Robinhood, as a data source and as a venue.
//!
//! Everything here is a fact about *this vendor*. What is not — the
//! inspect/compare/write pipeline, the risk gate, the session loop — lives in
//! [`arvo_data::source`] and `arvo_execution`, shared with every other
//! integration. Four things are genuinely Robinhood's, and each has a module:
//!
//! | | |
//! |---|---|
//! | `auth` | the endpoint, the OAuth, the keychain |
//! | `source` | what it is called, what it serves, how it asks |
//! | `parse` | the shape of the reply |
//! | `execution` | placing an order and finding out what became of it |
//!
//! This file is the list and the re-exports, and nothing else. Anything with a
//! decision in it belongs in one of the four. The modules are private and their
//! public items re-exported, so every name has exactly one path.
//!
//! # Signing in
//!
//! OAuth 2.1 with PKCE and dynamic client registration, via [`arvo_oauth`]. No
//! pasted token, and nothing about Robinhood is embedded beyond the endpoint —
//! every URL comes from the server's own discovery document.
//!
//! What is stored is a client id and a token pair, in the OS keychain. The
//! access token is refreshed on use when it has run out, and the refreshed pair
//! is written back, so an unattended sync survives an expiry.
//!
//! # One token, two kinds of call
//!
//! `source` reads and `execution` trades, on the same credential. The scope
//! asked for is whatever the server advertises — one scope, `internal`, and the
//! same one the endpoint requires for any call at all — so the token has never
//! been the thing standing between this crate and an order.
//!
//! What stands there is shape. [`RobinhoodExecutor`] is an
//! `arvo_execution::Executor`, and the only public path to `Executor::submit`
//! is `arvo_execution::Session::propose` — so nothing reaches
//! `place_equity_order` without having been sized by the risk gate and checked
//! against the halt, the day-trade budget, the loss limit and the staleness
//! window. Nothing in the app constructs one yet.
//!
//! # No dividends
//!
//! [`arvo_data::source::Source::dividends`] is left at its default, which
//! reports that this vendor does not serve them. The MCP server exposes no
//! distribution tool, and reporting an empty list would be a claim that the
//! instrument paid nothing. Fetch dividend-sensitive work from `arvo_yfinance`,
//! which does serve them.

mod auth;
mod execution;
mod parse;
mod source;

pub use auth::{access_token, begin_sign_in, complete_sign_in, disconnect, is_connected};
pub use execution::{RobinhoodExecutor, DEFAULT_MAX_ORDER_AGE_SECS};
pub use source::{Robinhood, SOURCE_ID, VENUE};
