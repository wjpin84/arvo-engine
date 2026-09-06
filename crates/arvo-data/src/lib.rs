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

pub mod interval;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

pub use crate::interval::{BarInterval, IntervalUnit};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// Bars held in memory.
///
/// The deterministic source: a fixture that cannot fail to read, cannot
/// change between runs, and needs no disk. Used for exercising the research
/// loop end to end and for regression fixtures pinned into evidence.
#[derive(Debug, Default, Clone)]
pub struct InMemoryBars {
    /// Keyed by instrument *and* resolution: the same instrument at five
    /// minutes and at one day is two different series, and returning one when
    /// the other was asked for would be a silent resolution mismatch.
    bars: BTreeMap<(String, String), Vec<Bar>>,
    instruments: std::collections::BTreeSet<String>,
}

impl InMemoryBars {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an instrument's daily history.
    #[must_use]
    pub fn with_instrument(self, instrument: &str, bars: Vec<Bar>) -> Self {
        self.with_interval(instrument, BarInterval::DAILY, bars)
    }

    /// Adds an instrument's history at one resolution, sorted on the way in so
    /// callers need not care about the order they supply.
    #[must_use]
    pub fn with_interval(
        mut self,
        instrument: &str,
        interval: BarInterval,
        mut bars: Vec<Bar>,
    ) -> Self {
        bars.sort_by_key(|bar| bar.at);
        self.instruments.insert(instrument.to_owned());
        self.bars
            .insert((instrument.to_owned(), interval.to_string()), bars);
        self
    }
}

impl BarProvider for InMemoryBars {
    fn source(&self) -> &str {
        "in-memory"
    }

    fn bars(
        &self,
        instrument: &str,
        interval: BarInterval,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError> {
        if !self.instruments.contains(instrument) {
            return Err(DataError::UnknownInstrument(instrument.to_owned()));
        }
        // Known instrument, nothing at this resolution: empty rather than
        // unknown, matching the distinction the trait already draws.
        let Some(bars) = self
            .bars
            .get(&(instrument.to_owned(), interval.to_string()))
        else {
            return Ok(Vec::new());
        };

        Ok(bars
            .iter()
            .filter(|bar| bar.at.date() >= from && bar.at.date() <= to)
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

    /// Writes bars into the library, replacing whatever was there.
    ///
    /// The counterpart to reading, so a fetched series lands in exactly the
    /// shape [`CsvBars`] already reads — same directory rule, same header,
    /// same timestamp spelling. A fetcher that invented its own format would
    /// be a second parser to keep in step with this one.
    ///
    /// Replacing rather than appending is deliberate. Merging two overlapping
    /// pulls means deciding which revision of a bar wins, and a file that is
    /// partly one fetch and partly another is not a dataset anyone can name:
    /// the content hash that identifies it would describe a state no single
    /// request ever returned.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if the directory cannot be created or the
    /// file cannot be written.
    pub fn write(
        &self,
        instrument: &str,
        interval: BarInterval,
        bars: &[Bar],
    ) -> Result<PathBuf, DataError> {
        let path = self.path_for(instrument, interval)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| DataError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut out = String::with_capacity(bars.len() * 64 + 32);
        out.push_str("date,open,high,low,close,volume\n");
        for bar in bars {
            // A daily bar keeps its bare date so an exported file still looks
            // like the free daily exports this format came from; anything
            // finer needs the time or the resolution is lost.
            let at = if interval == BarInterval::DAILY {
                bar.at.date().to_string()
            } else {
                bar.at.format("%Y-%m-%dT%H:%M:%S").to_string()
            };
            out.push_str(&format!(
                "{at},{},{},{},{},{}\n",
                bar.open, bar.high, bar.low, bar.close, bar.volume
            ));
        }

        std::fs::write(&path, out).map_err(|source| DataError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Where a resolution's files live.
    ///
    /// Daily bars sit in the root, so an existing library keeps working and
    /// [`Self::instruments`] still lists instruments rather than filenames.
    /// Anything finer goes in a subdirectory named for the interval —
    /// `5minute/AAPL.NASDAQ.csv` — because putting the interval in the
    /// filename would make `AAPL.NASDAQ.5minute` look like an instrument.
    fn directory(&self, interval: BarInterval) -> PathBuf {
        if interval == BarInterval::DAILY {
            self.root.clone()
        } else {
            self.root.join(interval.to_string())
        }
    }

    /// Resolves an instrument to its file, refusing anything that could
    /// escape the root.
    ///
    /// Instrument names arrive from config files and UI fields, so this is a
    /// trust boundary: `../../etc/passwd` is rejected here rather than handed
    /// to the filesystem.
    fn path_for(&self, instrument: &str, interval: BarInterval) -> Result<PathBuf, DataError> {
        let safe = !instrument.is_empty()
            && instrument
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && !instrument.contains("..");
        if !safe {
            return Err(DataError::UnsafeInstrument(instrument.to_owned()));
        }
        Ok(self.directory(interval).join(format!("{instrument}.csv")))
    }
}

impl CsvBars {
    /// Every instrument this directory holds, from the file names.
    ///
    /// Sorted, so a UI listing them does not reshuffle between launches.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if the directory cannot be read. A missing
    /// directory is not an error — it is an empty library, which is the
    /// ordinary state before anyone has added data.
    pub fn instruments(&self) -> Result<Vec<String>, DataError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(DataError::Io {
                    path: self.root.clone(),
                    source,
                })
            }
        };

        let mut instruments: Vec<String> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
            })
            .filter_map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(ToOwned::to_owned)
            })
            .collect();
        instruments.sort();
        Ok(instruments)
    }
}

/// A date or a timestamp, as the instant a bar opens.
fn parse_stamp(text: &str) -> Option<NaiveDateTime> {
    let cleaned = text.trim().trim_end_matches('Z').replace(' ', "T");
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(at) = NaiveDateTime::parse_from_str(&cleaned, format) {
            return Some(at);
        }
    }
    // A bare date is midnight: a daily bar opens at the start of its day.
    NaiveDate::parse_from_str(&cleaned, "%Y-%m-%d")
        .ok()
        .map(|date| date.and_time(NaiveTime::MIN))
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

    let stamp = next("date")?;
    // A date or a timestamp. Daily exports write `2024-01-02`; an intraday
    // one writes `2024-01-02T13:30:00` or the same with a space, and some
    // carry a trailing Z. All mean the instant the bar opens.
    let at = parse_stamp(&stamp)
        .ok_or_else(|| malformed(format!("date {stamp:?} is not a date or timestamp")))?;

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
        at,
        open: number("open")?,
        high: number("high")?,
        low: number("low")?,
        close: number("close")?,
        volume: number("volume")?,
    };

    // Full OHLC consistency, not just high >= low. These are exactly the
    // predicates Nautilus checks when a bar reaches the engine, asserted here
    // instead so corrupt data fails at the trust boundary with a line number
    // rather than deep inside a backtest.
    for (name, ok) in [
        ("high is below low", bar.high >= bar.low),
        ("high is below open", bar.high >= bar.open),
        ("high is below close", bar.high >= bar.close),
        ("low is above open", bar.low <= bar.open),
        ("low is above close", bar.low <= bar.close),
        ("volume is negative", bar.volume >= 0.0),
    ] {
        if !ok {
            return Err(malformed(format!(
                "{name} (o={} h={} l={} c={} v={})",
                bar.open, bar.high, bar.low, bar.close, bar.volume
            )));
        }
    }

    Ok(bar)
}

impl BarProvider for CsvBars {
    fn source(&self) -> &str {
        "csv"
    }

    fn bars(
        &self,
        instrument: &str,
        interval: BarInterval,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, DataError> {
        let path = self.path_for(instrument, interval)?;
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
            if bar.at.date() >= from && bar.at.date() <= to {
                bars.push(bar);
            }
        }

        // Sources are not reliably ordered, and every consumer assumes they
        // are. Sorting once here is cheaper than every caller remembering.
        bars.sort_by_key(|bar| bar.at);
        Ok(bars)
    }
}

#[cfg(test)]
mod tests {
    /// What a fetcher writes must be what the library reads. This is the seam
    /// where a format mismatch costs nothing at write time and shows up later
    /// as an instrument that exists on disk and holds no bars.
    mod round_trip {
        use super::super::*;

        fn bars(interval: BarInterval) -> Vec<Bar> {
            (0..4)
                .map(|index| {
                    let at = NaiveDate::from_ymd_opt(2026, 9, 3)
                        .expect("valid")
                        .and_hms_opt(13, 30, 0)
                        .expect("valid")
                        + interval.duration() * index;
                    let close = 500.0 + f64::from(index) * 0.25;
                    Bar {
                        at,
                        open: close - 0.1,
                        high: close + 0.5,
                        low: close - 0.5,
                        close,
                        volume: 1_000.0 + f64::from(index),
                    }
                })
                .collect()
        }

        #[test]
        fn intraday_bars_survive_a_write_and_a_read() {
            let dir = tempfile::tempdir().expect("tempdir");
            let library = CsvBars::new(dir.path());
            let interval = BarInterval::new(5, IntervalUnit::Minute);
            let written = bars(interval);

            library
                .write("MSFT.NASDAQ", interval, &written)
                .expect("writes");
            let read = library
                .bars(
                    "MSFT.NASDAQ",
                    interval,
                    NaiveDate::MIN,
                    NaiveDate::MAX,
                )
                .expect("reads");

            assert_eq!(read.len(), written.len());
            for (read, written) in read.iter().zip(&written) {
                assert_eq!(read.at, written.at, "the timestamp is the whole instant");
                assert!((read.close - written.close).abs() < 1e-9);
                assert!((read.volume - written.volume).abs() < 1e-9);
            }
        }

        #[test]
        fn a_daily_file_keeps_the_bare_date_the_free_exports_use() {
            let dir = tempfile::tempdir().expect("tempdir");
            let library = CsvBars::new(dir.path());
            let daily: Vec<Bar> = bars(BarInterval::DAILY)
                .into_iter()
                .map(|bar| Bar {
                    at: bar.at.date().and_time(NaiveTime::MIN),
                    ..bar
                })
                .collect();

            let path = library
                .write("MSFT.NASDAQ", BarInterval::DAILY, &daily)
                .expect("writes");
            let text = std::fs::read_to_string(&path).expect("readable");
            assert!(
                text.contains("\n2026-09-03,"),
                "a daily row is a bare date: {text}"
            );

            let read = library
                .daily_bars("MSFT.NASDAQ", NaiveDate::MIN, NaiveDate::MAX)
                .expect("reads");
            assert_eq!(read, daily);
        }

        #[test]
        fn an_intraday_write_does_not_appear_as_a_new_instrument() {
            // Intraday files live in a subdirectory precisely so a five-minute
            // pull does not make `MSFT.NASDAQ.5minute` look like something you
            // could trade.
            let dir = tempfile::tempdir().expect("tempdir");
            let library = CsvBars::new(dir.path());
            let interval = BarInterval::new(5, IntervalUnit::Minute);

            library
                .write("MSFT.NASDAQ", interval, &bars(interval))
                .expect("writes");
            assert_eq!(
                library.instruments().expect("lists"),
                Vec::<String>::new(),
                "only daily files name instruments"
            );
        }

        #[test]
        fn writing_replaces_rather_than_appends() {
            // A file that is partly one fetch and partly another is not a
            // dataset anyone can name: its content hash would describe a state
            // no single request ever returned.
            let dir = tempfile::tempdir().expect("tempdir");
            let library = CsvBars::new(dir.path());
            let interval = BarInterval::new(5, IntervalUnit::Minute);

            library
                .write("MSFT.NASDAQ", interval, &bars(interval))
                .expect("writes");
            library
                .write("MSFT.NASDAQ", interval, &bars(interval)[..2])
                .expect("writes again");

            let read = library
                .bars("MSFT.NASDAQ", interval, NaiveDate::MIN, NaiveDate::MAX)
                .expect("reads");
            assert_eq!(read.len(), 2);
        }
    }

    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("test date is valid")
    }

    fn bar(day: u32, close: f64) -> Bar {
        Bar {
            at: date(2024, 1, day).and_time(chrono::NaiveTime::MIN),
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
        assert_eq!(bars[0].at.date(), date(2024, 1, 3));
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
    fn coverage_reports_the_real_extent_of_the_data() {
        let source = InMemoryBars::new()
            .with_instrument("AAPL.NASDAQ", vec![bar(3, 3.0), bar(1, 1.0), bar(2, 2.0)]);

        let (first, last) = source
            .coverage("AAPL.NASDAQ", BarInterval::DAILY)
            .expect("known instrument")
            .expect("it holds bars");
        assert_eq!(first, date(2024, 1, 1));
        assert_eq!(last, date(2024, 1, 3));

        let empty = InMemoryBars::new().with_instrument("EMPTY.X", vec![]);
        assert_eq!(
            empty
                .coverage("EMPTY.X", BarInterval::DAILY)
                .expect("known"),
            None
        );
    }

    #[test]
    fn a_fingerprint_changes_when_a_price_changes_and_not_otherwise() {
        let original = InMemoryBars::new()
            .with_instrument("AAPL.NASDAQ", vec![bar(1, 1.0), bar(2, 2.0), bar(3, 3.0)]);
        let reordered = InMemoryBars::new()
            .with_instrument("AAPL.NASDAQ", vec![bar(3, 3.0), bar(1, 1.0), bar(2, 2.0)]);
        let edited = InMemoryBars::new()
            .with_instrument("AAPL.NASDAQ", vec![bar(1, 1.0), bar(2, 2.5), bar(3, 3.0)]);

        let of = |source: &InMemoryBars| {
            source
                .fingerprint("AAPL.NASDAQ", BarInterval::DAILY)
                .expect("known instrument")
                .expect("holds bars")
        };

        assert_eq!(
            of(&original),
            of(&reordered),
            "the same bars in a different input order are the same dataset"
        );
        assert_ne!(
            of(&original),
            of(&edited),
            "one changed price must invalidate every result that used it"
        );
    }

    #[test]
    fn an_empty_instrument_has_no_fingerprint_to_report() {
        let empty = InMemoryBars::new().with_instrument("EMPTY.X", vec![]);
        assert_eq!(
            empty
                .fingerprint("EMPTY.X", BarInterval::DAILY)
                .expect("known"),
            None
        );

        let err = empty
            .fingerprint("NEVER.HEARD", BarInterval::DAILY)
            .expect_err("unknown instrument");
        assert!(matches!(err, DataError::UnknownInstrument(_)), "{err}");
    }

    #[test]
    fn an_absent_data_directory_is_an_empty_library_not_an_error() {
        let missing = CsvBars::new("/no/such/directory/anywhere");
        assert_eq!(
            missing.instruments().expect("absence is not failure"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn instruments_come_from_the_csv_file_names_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_csv(
            dir.path(),
            "MSFT.NASDAQ.csv",
            "date,open,high,low,close,volume
",
        );
        write_csv(
            dir.path(),
            "AAPL.NASDAQ.csv",
            "date,open,high,low,close,volume
",
        );
        write_csv(dir.path(), "notes.txt", "ignore me");

        assert_eq!(
            CsvBars::new(dir.path()).instruments().expect("readable"),
            vec!["AAPL.NASDAQ".to_owned(), "MSFT.NASDAQ".to_owned()]
        );
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
