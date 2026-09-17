//! Where research data comes from.
//!
//! One trait, [`BarProvider`], and two implementations that both do real work
//! today: [`CsvBars`] reads daily bars off disk, [`InMemoryBars`] holds a
//! fixture so the research loop can be exercised deterministically without
//! touching the filesystem.
//!
//! # Scope
//!
//! Bars at any resolution, from seconds to weeks — see [`interval`]. Quotes,
//! order books and live subscriptions are still not modelled, because nothing
//! consumes them yet and guessing their shape would mean guessing wrong.
//!
//! A bar carries a *timestamp*, not a date. That was a date while everything
//! was daily, and the assumption had spread into three crates by the time it
//! had to come out.
//!
//! This is *research* data — it is not Nautilus's `DataClient` and does not
//! mirror it. Venue adapters, execution feeds and live streaming remain
//! Nautilus's, reached through `arvo-nautilus`.

pub mod source;
pub mod agreement;
pub mod interval;
pub mod option;
pub mod quality;
pub mod session;
pub mod signal;

mod csv;
mod in_memory;

pub use crate::csv::CsvBars;
pub use crate::in_memory::InMemoryBars;
pub use crate::interval::{BarInterval, IntervalUnit};
pub use crate::signal::{Signal, SignalName, Signals};

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One bar of trading for one instrument.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    /// When the bar *opens*. A daily bar opens at midnight of its date.
    ///
    /// The instant it closes is this plus the interval's duration, and that
    /// is what the engine timestamps it with — a close is not knowable until
    /// the period ends, and pretending otherwise is look-ahead bias.
    pub at: NaiveDateTime,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// One cash distribution, on the day it went ex.
///
/// # Why the library holds these at all
///
/// Because the absence of them is a live overstatement, not a missing feature.
/// Every source is asked for split-adjusted prices, which is right — raw prices
/// make a split look like a crash, and a breakout rule would trade it. But
/// split-adjusted is not *total-return* adjusted: dividends are absent from the
/// price series, so nothing receives them. A benchmark holds through every
/// ex-date and a rule in the market some of the time holds through only some,
/// which overstates excess return in the strategy's favour on every
/// dividend-paying instrument.
///
/// Held as fetched files beside the bars, and for the same reason as the bars:
/// a distribution read live at backtest time would break reproducibility
/// exactly as a live bar would. See `arvo_research::dividend` for the
/// measurement they make possible.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Dividend {
    /// The first day the shares trade without entitlement to this payment.
    ///
    /// Entitlement is settled at the close *before* this date, so a holder who
    /// sells on the ex-date still receives it and a buyer on the ex-date does
    /// not. That asymmetry is the whole of the arithmetic in
    /// `arvo_research::dividend`.
    pub ex_date: NaiveDate,
    /// Per share, in the instrument's currency, on the same split basis as the
    /// prices it sits beside.
    pub amount: f64,
}

/// Why data could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("no data held for instrument {0:?}")]
    UnknownInstrument(String),
    #[error("instrument {0:?} is not a usable file name")]
    UnsafeInstrument(String),
    #[error("reading {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} line {line}: {reason}")]
    Malformed {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

/// Supplies historical daily bars.
///
/// Synchronous, matching `arvo_research::SimulationProvider`: a backtest
/// loads its whole window up front and then runs CPU-bound, so there is no
/// reactor to keep free. A remote source can still live behind this trait —
/// it blocks inside `spawn_blocking` like any other batch fetch.
///
/// Turn this async when a provider needs to interleave fetches with a live
/// run, which is a live-trading concern and explicitly out of scope for now.
pub trait BarProvider: Send + Sync {
    /// Names the source, for the reproducibility record.
    fn source(&self) -> &str;

    /// Bars for `instrument` between `from` and `to`, both inclusive,
    /// ordered oldest first.
    ///
    /// An empty result means the instrument is known but has no bars in the
    /// window — distinct from [`DataError::UnknownInstrument`], which means
    /// the source has never heard of it. Evaluation treats those differently.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the instrument is unknown or its data cannot
    /// be read or parsed.
    fn bars(
        &self,
        instrument: &str,
        interval: BarInterval,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError>;

    /// Daily bars, which is what most callers still want.
    ///
    /// # Errors
    ///
    /// As [`Self::bars`].
    fn daily_bars(
        &self,
        instrument: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError> {
        self.bars(instrument, BarInterval::DAILY, from, to)
    }

    /// The first and last day this source holds for `instrument`.
    ///
    /// Callers need this to state a *real* window in an experiment. Reaching
    /// for an obviously-too-wide range instead would put a date in the
    /// reproducibility record that no data ever covered.
    ///
    /// `None` for a known instrument with no bars at all.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] on the same conditions as [`Self::daily_bars`].
    fn coverage(
        &self,
        instrument: &str,
        interval: BarInterval,
    ) -> Result<Option<(NaiveDate, NaiveDate)>, DataError> {
        let bars = self.bars(instrument, interval, NaiveDate::MIN, NaiveDate::MAX)?;
        Ok(match (bars.first(), bars.last()) {
            (Some(first), Some(last)) => Some((first.at.date(), last.at.date())),
            _ => None,
        })
    }

    /// Cash distributions for `instrument` between `from` and `to`, both
    /// inclusive, oldest first.
    ///
    /// `None` means this source holds no distribution series at all — it has
    /// never been told about them. `Some(vec![])` means it has one and the
    /// instrument paid nothing in the window. Those are different facts and
    /// collapsing them would report *no dividends* for an instrument that pays
    /// them, which is the direction the excess-return bias already leans.
    ///
    /// Defaulted to `None` rather than made required: an in-memory fixture and
    /// every test double have no such series, and forcing each to say so would
    /// be ceremony. The one provider that reads a real library overrides it.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] if the series exists and cannot be read or parsed.
    fn dividends(
        &self,
        _instrument: &str,
        _from: NaiveDate,
        _to: NaiveDate,
    ) -> Result<Option<Vec<Dividend>>, DataError> {
        Ok(None)
    }

    /// Every option contract on `underlying` this source holds at `interval`,
    /// as instrument names (`SPY250912P00640000.AOPT`), in no promised order.
    ///
    /// A chain is what a rule that picks its own contracts reads from (#86).
    /// Defaulted to none: only a library that files contracts holds any.
    ///
    /// # Errors
    ///
    /// [`DataError`] if the contracts exist and cannot be listed.
    fn option_contracts(
        &self,
        _underlying: &str,
        _interval: BarInterval,
    ) -> Result<Vec<String>, DataError> {
        Ok(Vec::new())
    }

    /// A content hash of every option bar this source holds on `underlying` at
    /// `interval`: the chain as one dataset, so a finding a chain produced can
    /// be found stale when any contract in it changes.
    ///
    /// Defaulted to fingerprinting each listed contract, which is right and
    /// slow; a library that files contracts together hashes them in one pass.
    /// `None` when there is no chain.
    ///
    /// # Errors
    ///
    /// As [`Self::fingerprint`].
    fn option_chain_fingerprint(
        &self,
        underlying: &str,
        interval: BarInterval,
    ) -> Result<Option<String>, DataError> {
        let mut contracts = self.option_contracts(underlying, interval)?;
        if contracts.is_empty() {
            return Ok(None);
        }
        contracts.sort();
        let mut hasher = blake3::Hasher::new();
        for contract in contracts {
            hasher.update(contract.as_bytes());
            if let Some(fingerprint) = self.fingerprint(&contract, interval)? {
                hasher.update(fingerprint.as_bytes());
            }
        }
        Ok(Some(hasher.finalize().to_hex().to_string()))
    }

    /// A content hash of every bar this source holds for `instrument`.
    ///
    /// This is what makes a result reproducible rather than merely repeatable.
    /// An experiment records the *identity* of the data it ran against, so a
    /// stored result can later be checked against the data still on disk and
    /// found stale instead of being quietly trusted.
    ///
    /// Hashes the parsed bars, not the file bytes, and that distinction is
    /// deliberate: reformatting a CSV, changing its line endings or resaving it
    /// does not invalidate a result, because none of that changes what the
    /// experiment saw. Changing a single price does.
    ///
    /// Covers the instrument's whole history at that resolution, rather than
    /// any one window. A dataset is the data; which slice of it an experiment
    /// used is recorded separately, and conflating the two would make every
    /// window look like a different dataset.
    ///
    /// Per-resolution, because they *are* different datasets: the same
    /// instrument at five minutes and at one day is two different series, and
    /// one hash covering both would say a result was stale when the other
    /// changed.
    ///
    /// `None` for a known instrument holding no bars.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] on the same conditions as [`Self::daily_bars`].
    fn fingerprint(
        &self,
        instrument: &str,
        interval: BarInterval,
    ) -> Result<Option<String>, DataError> {
        let bars = self.bars(instrument, interval, NaiveDate::MIN, NaiveDate::MAX)?;
        if bars.is_empty() {
            return Ok(None);
        }

        let mut hasher = blake3::Hasher::new();
        for bar in &bars {
            // Raw bit patterns and a day count, not formatted text: exact,
            // and identical on every platform and toolchain. `DefaultHasher`
            // would have been easier and is explicitly not stable across Rust
            // releases, which would make a fingerprint meaningless the moment
            // the compiler moved.
            hasher.update(&bar.at.and_utc().timestamp().to_le_bytes());
            for value in [bar.open, bar.high, bar.low, bar.close, bar.volume] {
                hasher.update(&value.to_bits().to_le_bytes());
            }
        }
        Ok(Some(hasher.finalize().to_hex().to_string()))
    }
}

#[cfg(test)]
mod tests;
