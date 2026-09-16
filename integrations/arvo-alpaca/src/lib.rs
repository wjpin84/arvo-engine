//! Alpaca: bars, distributions, and a venue that trades on simulated money.
//!
//! # What this is here for
//!
//! The dividends, mostly. Alpaca's corporate-actions endpoint is a documented,
//! supported distribution feed with ex-date, rate and payable date — the first
//! proper one in this codebase. Yahoo's `events=div` is a chart-endpoint side
//! effect by comparison, and Robinhood serves none at all.
//!
//! # The free tier is IEX, and that is a backtest problem
//!
//! Alpaca's free plan serves only IEX for equities. IEX is a low-single-digit
//! share of consolidated volume, so free-tier bars are a thin sample of the
//! tape rather than the tape: the prices are real, the volumes are not, and
//! anything conditioned on size is reading a different market. `vwap_reversion`
//! computed from IEX prints is not VWAP.
//!
//! That is invisible to a cross-check, because the prices agree — which is
//! exactly why [`arvo_data::source::Source::basis`] exists and why
//! [`Alpaca::iex`] declares [`arvo_data::source::Feed::SingleVenue`]. A comparison against the broker will say so rather
//! than reporting `Aligned` and leaving it there.
//!
//! SIP costs $99 a month. Against a small account that is a hurdle a strategy
//! has to clear before it earns anything, so the two feeds are separate
//! constructors filing under separate venues: `AAPL.AIEX` and `AAPL.ASIP` are
//! two datasets with two content hashes, and letting them share a name would
//! let a study silently run on whichever was fetched last.
//!
//! # Paper and live are two venues, not one with a flag
//!
//! Alpaca runs a paper endpoint that is the real trading API against simulated
//! money: real order lifecycle, real market hours, real rejections, real
//! partial fills. It is the only place any of those get exercised before real
//! capital does, so [`AlpacaExecutor::paper`] is the constructor to reach for
//! and [`AlpacaExecutor::live`] is the one that should feel deliberate.
//!
//! This does **not** retire `arvo_execution::paper::PaperExecutor`, and the two
//! answer different questions. Alpaca paper answers *does the integration
//! work* — auth, lifecycle, rejections, PDT. `PaperExecutor` answers *was the
//! cost assumption right*, against prices that actually arrived. Running
//! `Divergence` against Alpaca paper fills would measure Alpaca's fill
//! simulator rather than the market.
//!
//! # What this does not implement
//!
//! [`arvo_data::source::Source::search`] and
//! [`arvo_data::source::Source::quotes`] stay at their trait defaults, which
//! report that this vendor does not offer them rather than returning an empty
//! list. Alpaca has both, and nothing here needs them yet — the broker already
//! answers the search box.

mod auth;
pub mod chain;
mod execution;
pub mod options;
pub mod holdings;
mod parse;
mod source;

pub use auth::{forget, has, store, Env, Keys, LEGACY_CREDENTIAL_ID};
pub use holdings::{holdings, AccountHoldings, Position};
pub use execution::{AlpacaExecutor, DEFAULT_MAX_ORDER_AGE_SECS};
pub use source::{
    Alpaca, IEX_SOURCE_ID, IEX_TOTAL_RETURN_SOURCE_ID, IEX_TOTAL_RETURN_VENUE, IEX_VENUE,
    SIP_SOURCE_ID, SIP_TOTAL_RETURN_SOURCE_ID, SIP_TOTAL_RETURN_VENUE, SIP_VENUE,
};
