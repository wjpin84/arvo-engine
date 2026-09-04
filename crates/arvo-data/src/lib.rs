//! Where research data comes from.
//!
//! One trait, [`BarProvider`], and two implementations that both do real work
//! today: [`CsvBars`] reads daily bars off disk, [`InMemoryBars`] holds a
//! fixture so the research loop can be exercised deterministically without
//! touching the filesystem.
//!
//! # Scope
//!
//! Daily bars only, and deliberately. The first vertical slice runs on daily
//! bars, which sidesteps the intraday tier and the paid-feed question
//! entirely. Intraday, quotes, order books and live subscriptions are not
//! modelled here because nothing consumes them yet, and guessing their shape
//! now would mean guessing wrong.
//!
//! This is *research* data — it is not Nautilus's `DataClient` and does not
//! mirror it. Venue adapters, execution feeds and live streaming remain
//! Nautilus's, reached through `arvo-nautilus`.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One day of trading for one instrument.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
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
/// Synchronous, matching [`arvo_research::SimulationProvider`]: a backtest
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
    fn daily_bars(
        &self,
        instrument: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError>;
}

/// Bars held in memory.
///
/// The deterministic source: a fixture that cannot fail to read, cannot
/// change between runs, and needs no disk. Used for exercising the research
/// loop end to end and for regression fixtures pinned into evidence.
#[derive(Debug, Default, Clone)]
pub struct InMemoryBars {
    bars: BTreeMap<String, Vec<Bar>>,
}

impl InMemoryBars {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an instrument's history, sorted on the way in so callers need not
    /// care about the order they supply.
    #[must_use]
    pub fn with_instrument(mut self, instrument: &str, mut bars: Vec<Bar>) -> Self {
        bars.sort_by_key(|bar| bar.date);
        self.bars.insert(instrument.to_owned(), bars);
        self
    }
}

impl BarProvider for InMemoryBars {
    fn source(&self) -> &str {
        "in-memory"
    }

    fn daily_bars(
        &self,
        instrument: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError> {
        let bars = self
            .bars
            .get(instrument)
            .ok_or_else(|| DataError::UnknownInstrument(instrument.to_owned()))?;

        Ok(bars
            .iter()
            .filter(|bar| bar.date >= from && bar.date <= to)
            .copied()
            .collect())
    }
}

/// Daily bars from a directory of CSV files, one per instrument.
///
/// Each file is `<INSTRUMENT>.csv` — so `AAPL.NASDAQ.csv` — with the header
/// `date,open,high,low,close,volume` and ISO `YYYY-MM-DD` dates. That is what
/// the free daily-bar exports (Stooq, Yahoo) already look like, so the slice
/// can run on real prices without a paid feed or an API key.
#[derive(Debug, Clone)]
pub struct CsvBars {
    root: PathBuf,
}

impl CsvBars {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Into::into(root),
        }
    }

    /// Resolves an instrument to its file, refusing anything that could
    /// escape the root.
    ///
    /// Instrument names arrive from config files and UI fields, so this is a
    /// trust boundary: `../../etc/passwd` is rejected here rather than handed
    /// to the filesystem.
    fn path_for(&self, instrument: &str) -> Result<PathBuf, DataError> {
        let safe = !instrument.is_empty()
            && instrument
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && !instrument.contains("..");
        if !safe {
            return Err(DataError::UnsafeInstrument(instrument.to_owned()));
        }
        Ok(self.root.join(format!("{instrument}.csv")))
    }
}

/// Parses one data row.
///
/// ponytail: split on commas, no quoting or escapes. OHLCV exports are bare
/// numbers and ISO dates, so there is nothing to quote. Swap in the `csv`
/// crate if a source ever ships quoted fields or embedded commas.
fn parse_row(path: &Path, line_no: usize, line: &str) -> Result<Bar, DataError> {
    let malformed = |reason: String| DataError::Malformed {
        path: path.to_path_buf(),
        line: line_no,
        reason,
    };

    let mut fields = line.split(',').map(str::trim);
    let mut next = |name: &str| -> Result<String, DataError> {
        fields
            .next()
            .map(ToOwned::to_owned)
            .ok_or_else(|| malformed(format!("missing {name}")))
    };

    let date = next("date")?;
    let date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
        .map_err(|err| malformed(format!("date {date:?}: {err}")))?;

    let mut number = |name: &str| -> Result<f64, DataError> {
        let raw = next(name)?;
        let value: f64 = raw
            .parse()
            .map_err(|err| malformed(format!("{name} {raw:?}: {err}")))?;
        if !value.is_finite() {
            return Err(malformed(format!("{name} {raw:?} is not finite")));
        }
        Ok(value)
    };

    let bar = Bar {
        date,
        open: number("open")?,
        high: number("high")?,
        low: number("low")?,
        close: number("close")?,
        volume: number("volume")?,
    };

    // A high below the low is corrupt data, not an unusual day. Catching it
    // here keeps it out of an equity curve that would otherwise look fine.
    if bar.high < bar.low {
        return Err(malformed(format!(
            "high {} is below low {}",
            bar.high, bar.low
        )));
    }

    Ok(bar)
}

impl BarProvider for CsvBars {
    fn source(&self) -> &str {
        "csv"
    }

    fn daily_bars(
        &self,
        instrument: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError> {
        let path = self.path_for(instrument)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(DataError::UnknownInstrument(instrument.to_owned()))
            }
            Err(source) => return Err(DataError::Io { path, source }),
        };

        let mut bars = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            // Skip the header and any blank separator lines.
            if line.is_empty() || index == 0 && line.starts_with("date") {
                continue;
            }
            let bar = parse_row(&path, index + 1, line)?;
            if bar.date >= from && bar.date <= to {
                bars.push(bar);
            }
        }

        // Sources are not reliably ordered, and every consumer assumes they
        // are. Sorting once here is cheaper than every caller remembering.
        bars.sort_by_key(|bar| bar.date);
        Ok(bars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("test date is valid")
    }

    fn bar(day: u32, close: f64) -> Bar {
        Bar {
            date: date(2024, 1, day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1_000.0,
        }
    }

    #[test]
    fn in_memory_bars_come_back_sorted_and_windowed() {
        let source = InMemoryBars::new()
            .with_instrument("AAPL.NASDAQ", vec![bar(3, 3.0), bar(1, 1.0), bar(2, 2.0)]);

        let bars = source
            .daily_bars("AAPL.NASDAQ", date(2024, 1, 1), date(2024, 1, 2))
            .expect("instrument is known");

        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        assert_eq!(closes, vec![1.0, 2.0], "window is inclusive at both ends");
    }

    #[test]
    fn an_unknown_instrument_is_distinct_from_an_empty_window() {
        let source = InMemoryBars::new().with_instrument("AAPL.NASDAQ", vec![bar(1, 1.0)]);

        let empty = source
            .daily_bars("AAPL.NASDAQ", date(2025, 1, 1), date(2025, 1, 2))
            .expect("known instrument, no bars in window");
        assert!(empty.is_empty());

        let err = source
            .daily_bars("MSFT.NASDAQ", date(2024, 1, 1), date(2024, 1, 2))
            .expect_err("never heard of it");
        assert!(matches!(err, DataError::UnknownInstrument(_)), "{err}");
    }

    fn write_csv(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("fixture should write");
    }

    #[test]
    fn csv_bars_parse_and_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_csv(
            dir.path(),
            "AAPL.NASDAQ.csv",
            "date,open,high,low,close,volume\n\
             2024-01-02,185.0,186.0,184.0,185.5,1000\n\
             2024-01-03,185.5,187.0,185.0,186.5,1100\n\
             2024-01-04,186.5,188.0,186.0,187.5,1200\n",
        );

        let bars = CsvBars::new(dir.path())
            .daily_bars("AAPL.NASDAQ", date(2024, 1, 3), date(2024, 1, 4))
            .expect("fixture parses");

        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].date, date(2024, 1, 3));
        assert!((bars[1].close - 187.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_corrupt_row_is_an_error_naming_the_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_csv(
            dir.path(),
            "BAD.X.csv",
            "date,open,high,low,close,volume\n\
             2024-01-02,185.0,186.0,184.0,185.5,1000\n\
             2024-01-03,185.5,180.0,185.0,186.5,1100\n",
        );

        let err = CsvBars::new(dir.path())
            .daily_bars("BAD.X", date(2024, 1, 1), date(2024, 12, 31))
            .expect_err("high below low is corrupt");

        match err {
            DataError::Malformed {
                line, ref reason, ..
            } => {
                assert_eq!(line, 3, "should name the offending line");
                assert!(reason.contains("below low"), "{reason}");
            }
            other => panic!("expected a malformed-row error, got {other}"),
        }
    }

    #[test]
    fn a_traversing_instrument_name_never_reaches_the_filesystem() {
        let source = CsvBars::new("/does/not/matter");
        for attempt in ["../../secrets", "..", "a/b", ""] {
            let err = source
                .daily_bars(attempt, date(2024, 1, 1), date(2024, 1, 2))
                .expect_err("should refuse to build a path");
            assert!(
                matches!(err, DataError::UnsafeInstrument(_)),
                "{attempt:?}: {err}"
            );
        }
    }
}
