//! The library on disk: a directory of CSV files, and how bars, dividends and
//! option chains are filed in it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

use crate::{option, Bar, BarInterval, BarProvider, DataError, Dividend};

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

/// Where distribution files live, relative to the library root.
const DIVIDEND_SUBDIR: &str = "dividends";

/// Where option contracts' bars live, relative to the library root:
/// `options/<UNDERLYING>/<interval>/<EXPIRATION>.<VENUE>.csv`.
const OPTION_SUBDIR: &str = "options";

/// An instrument name that cannot escape the library root.
///
/// Instrument names arrive from config files and UI fields, so this is a trust
/// boundary: `../../etc/passwd` is rejected here rather than handed to the
/// filesystem. One definition because two paths are built from these now, and
/// a guard applied to one of them is not a guard.
///
/// # Errors
///
/// Returns [`DataError::UnsafeInstrument`] for an empty name, one containing
/// anything but alphanumerics, `.`, `-` and `_`, or one containing `..`.
pub(crate) fn safe_name(instrument: &str) -> Result<&str, DataError> {
    let safe = !instrument.is_empty()
        && instrument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !instrument.contains("..");
    if safe {
        Ok(instrument)
    } else {
        Err(DataError::UnsafeInstrument(instrument.to_owned()))
    }
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
        if option::OptionContract::parse(instrument).is_some() {
            let one = BTreeMap::from([(instrument.to_owned(), bars.to_vec())]);
            let mut paths = self.write_contracts(interval, &one)?;
            return Ok(paths.remove(0));
        }
        let path = self.path_for(instrument, interval)?;
        create_parent(&path)?;

        let mut out = String::with_capacity(bars.len() * 64 + 32);
        out.push_str("date,open,high,low,close,volume\n");
        for bar in bars {
            out.push_str(&row_text(bar, interval));
            out.push('\n');
        }
        write_file(&path, &out)?;
        Ok(path)
    }

    /// Writes option contracts' bars, replacing each named contract's rows and
    /// keeping every other contract in the same files.
    ///
    /// # One file per expiration, not per contract
    ///
    /// A year of daily SPY expirations near the money is tens of thousands of
    /// contracts. A file each is a directory nobody can open and a fingerprint
    /// check that stats every one. They are read an expiration at a time — a
    /// strategy picks from the contracts expiring on a date — so that is the
    /// file: `options/SPY/5minute/2025-09-12.AOPT.csv`, rows keyed by symbol.
    ///
    /// Each file is rewritten once however many of its contracts are given,
    /// which is why a fetch hands over a whole expiration rather than calling
    /// [`Self::write`] per contract.
    ///
    /// # Errors
    ///
    /// [`DataError::UnsafeInstrument`] for a name that is not an option
    /// contract, before anything is written; [`DataError::Io`] and
    /// [`DataError::Malformed`] from the files being merged into.
    pub fn write_contracts(
        &self,
        interval: BarInterval,
        series: &BTreeMap<String, Vec<Bar>>,
    ) -> Result<Vec<PathBuf>, DataError> {
        let mut by_file: BTreeMap<PathBuf, Vec<(String, &[Bar])>> = BTreeMap::new();
        for (instrument, bars) in series {
            let contract = option::OptionContract::parse(instrument)
                .ok_or_else(|| DataError::UnsafeInstrument(instrument.clone()))?;
            by_file
                .entry(self.path_for(instrument, interval)?)
                .or_default()
                .push((contract.symbol(), bars));
        }

        for (path, contracts) in &by_file {
            let replaced: std::collections::BTreeSet<&str> =
                contracts.iter().map(|(symbol, _)| symbol.as_str()).collect();
            let mut rows: Vec<String> = match std::fs::read_to_string(path) {
                Ok(text) => text
                    .lines()
                    .skip(1)
                    .filter(|line| {
                        let symbol = line.split(',').next().unwrap_or_default();
                        !line.trim().is_empty() && !replaced.contains(symbol)
                    })
                    .map(ToOwned::to_owned)
                    .collect(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(source) => {
                    return Err(DataError::Io {
                        path: path.clone(),
                        source,
                    })
                }
            };
            for (symbol, bars) in contracts {
                rows.extend(
                    bars.iter()
                        .map(|bar| format!("{symbol},{}", row_text(bar, interval))),
                );
            }
            // Symbol then time, because every row's time is fixed-width: the
            // file diffs cleanly between fetches.
            rows.sort();

            create_parent(path)?;
            let mut out = String::with_capacity(rows.len() * 72 + 48);
            out.push_str(OPTION_HEADER);
            out.push('\n');
            for row in rows {
                out.push_str(&row);
                out.push('\n');
            }
            write_file(path, &out)?;
        }
        Ok(by_file.into_keys().collect())
    }

    /// Writes an instrument's distributions, replacing whatever was there.
    ///
    /// Replacing rather than merging, for the same reason [`Self::write`]
    /// replaces: a file that is partly one fetch and partly another is not a
    /// dataset anyone can name.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if the directory cannot be created or the file
    /// cannot be written, and [`DataError::UnsafeInstrument`] for a name that
    /// could escape the root.
    pub fn write_dividends(
        &self,
        instrument: &str,
        dividends: &[Dividend],
    ) -> Result<PathBuf, DataError> {
        let path = self.dividend_path(instrument)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| DataError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut out = String::with_capacity(dividends.len() * 24 + 16);
        out.push_str("ex_date,amount
");
        for dividend in dividends {
            out.push_str(&format!("{},{}
", dividend.ex_date, dividend.amount));
        }

        std::fs::write(&path, out).map_err(|source| DataError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Where an instrument's distributions live: `dividends/SYMBOL.VENUE.csv`.
    ///
    /// # Why a subdirectory rather than `SYMBOL.VENUE.dividends.csv`
    ///
    /// The same reason a five-minute file does not sit in the root:
    /// [`Self::instruments`] lists the root's file *stems*, so a sibling named
    /// `MSFT.RH.dividends.csv` would be listed as an instrument called
    /// `MSFT.RH.dividends`. It would then appear in the data library, in the
    /// watchlist, and in anything that iterates the library — an instrument
    /// with no bars that nothing put there on purpose.
    fn dividend_path(&self, instrument: &str) -> Result<PathBuf, DataError> {
        Ok(self
            .root
            .join(DIVIDEND_SUBDIR)
            .join(format!("{}.csv", safe_name(instrument)?)))
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
        let file = format!("{}.csv", safe_name(instrument)?);
        // An option contract files with the rest of its expiration, under its
        // underlying — see `write_contracts`. Beside the stocks, a chain would
        // bury the library listing.
        if let Some(contract) = option::OptionContract::parse(instrument) {
            // The venue stays in the name, so two vendors' copies of one
            // expiration are two files, as two copies of a stock are.
            let stem = match instrument.split_once('.') {
                Some((_, venue)) => format!("{}.{venue}", contract.expiration),
                None => contract.expiration.to_string(),
            };
            return Ok(self
                .root
                .join(OPTION_SUBDIR)
                .join(&contract.underlying)
                .join(interval.to_string())
                .join(format!("{stem}.csv")));
        }
        Ok(self.directory(interval).join(file))
    }
}

/// The header of an expiration file.
const OPTION_HEADER: &str = "symbol,date,open,high,low,close,volume";

/// One bar as a CSV row, without a symbol.
///
/// A daily bar keeps its bare date so an exported file still looks like the
/// free daily exports this format came from; anything finer needs the time or
/// the resolution is lost.
fn row_text(bar: &Bar, interval: BarInterval) -> String {
    let at = if interval == BarInterval::DAILY {
        bar.at.date().to_string()
    } else {
        bar.at.format("%Y-%m-%dT%H:%M:%S").to_string()
    };
    format!(
        "{at},{},{},{},{},{}",
        bar.open, bar.high, bar.low, bar.close, bar.volume
    )
}

fn create_parent(path: &Path) -> Result<(), DataError> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent).map_err(|source| DataError::Io {
            path: parent.to_path_buf(),
            source,
        }),
        None => Ok(()),
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), DataError> {
    std::fs::write(path, text).map_err(|source| DataError::Io {
        path: path.to_path_buf(),
        source,
    })
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

        // In an expiration file, only this contract's rows.
        let symbol = option::OptionContract::parse(instrument).map(|contract| contract.symbol());

        let mut bars = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            // Skip the header and any blank separator lines.
            if line.is_empty() || index == 0 && (line.starts_with("date") || line.starts_with("symbol")) {
                continue;
            }
            let line = match &symbol {
                Some(symbol) => match line.split_once(',') {
                    Some((row_symbol, rest)) if row_symbol == symbol => rest,
                    Some(_) => continue,
                    None => {
                        return Err(DataError::Malformed {
                            path,
                            line: index + 1,
                            reason: "expected symbol,date,open,high,low,close,volume".to_owned(),
                        })
                    }
                },
                None => line,
            };
            let bar = parse_row(&path, index + 1, line)?;
            if bar.at.date() >= from && bar.at.date() <= to {
                bars.push(bar);
            }
        }

        // A contract absent from its expiration's file is unknown, exactly as a
        // stock with no file is. Checked over the whole file, not the window,
        // so a known contract with no bars in the window stays an empty series.
        if let Some(symbol) = &symbol {
            let prefix = format!("{symbol},");
            if bars.is_empty() && !text.lines().any(|line| line.trim().starts_with(&prefix)) {
                return Err(DataError::UnknownInstrument(instrument.to_owned()));
            }
        }

        // Sources are not reliably ordered, and every consumer assumes they
        // are. Sorting once here is cheaper than every caller remembering.
        bars.sort_by_key(|bar| bar.at);
        Ok(bars)
    }

    /// Reads `dividends/SYMBOL.VENUE.csv`, if a fetch ever wrote one.
    ///
    /// A missing file is `None`, not an error and not an empty list: no source
    /// has ever supplied a distribution series for this instrument, which is
    /// different from one supplying a series that is empty over the window.
    fn dividends(
        &self,
        instrument: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Option<Vec<Dividend>>, DataError> {
        let path = self.dividend_path(instrument)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(DataError::Io { path, source }),
        };

        let mut paid = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || index == 0 && line.starts_with("ex_date") {
                continue;
            }
            let malformed = |reason: &str| DataError::Malformed {
                path: path.clone(),
                line: index + 1,
                reason: reason.to_owned(),
            };
            let (date, amount) = line
                .split_once(',')
                .ok_or_else(|| malformed("expected ex_date,amount"))?;
            // Parsed strictly. A distribution silently read as zero is a cash
            // credit that never happens, which is precisely the error the whole
            // series exists to remove.
            let ex_date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
                .map_err(|err| malformed(&format!("ex_date {date:?}: {err}")))?;
            let amount: f64 = amount
                .trim()
                .parse()
                .map_err(|_| malformed(&format!("amount {amount:?} is not a number")))?;
            if ex_date >= from && ex_date <= to {
                paid.push(Dividend { ex_date, amount });
            }
        }

        paid.sort_by_key(|dividend| dividend.ex_date);
        Ok(Some(paid))
    }

    /// Hashes each expiration file's parsed rows, files in name order, rows as
    /// they sort — one pass, where fingerprinting every contract separately
    /// reparses its whole file once per contract. Parsed rather than raw bytes,
    /// for the reason [`BarProvider::fingerprint`] gives.
    fn option_chain_fingerprint(
        &self,
        underlying: &str,
        interval: BarInterval,
    ) -> Result<Option<String>, DataError> {
        let folder = self
            .root
            .join(OPTION_SUBDIR)
            .join(safe_name(underlying)?)
            .join(interval.to_string());
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&folder) {
            Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(DataError::Io { path: folder, source }),
        };
        if files.is_empty() {
            return Ok(None);
        }
        files.sort();
        let mut hasher = blake3::Hasher::new();
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|source| DataError::Io {
                path: path.clone(),
                source,
            })?;
            hasher.update(path.file_name().and_then(|n| n.to_str()).unwrap_or_default().as_bytes());
            let mut rows: Vec<(&str, Bar)> = Vec::new();
            for (index, line) in text.lines().enumerate().skip(1) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let (symbol, rest) = line.split_once(',').ok_or_else(|| DataError::Malformed {
                    path: path.clone(),
                    line: index + 1,
                    reason: "expected symbol,date,open,high,low,close,volume".to_owned(),
                })?;
                rows.push((symbol, parse_row(&path, index + 1, rest)?));
            }
            rows.sort_by(|a, b| a.0.cmp(b.0).then(a.1.at.cmp(&b.1.at)));
            for (symbol, bar) in rows {
                hasher.update(symbol.as_bytes());
                hasher.update(&bar.at.and_utc().timestamp().to_le_bytes());
                for value in [bar.open, bar.high, bar.low, bar.close, bar.volume] {
                    hasher.update(&value.to_bits().to_le_bytes());
                }
            }
        }
        Ok(Some(hasher.finalize().to_hex().to_string()))
    }

    /// Reads the symbol column of every expiration file under
    /// `options/<UNDERLYING>/<interval>/`, naming each contract with the venue
    /// its file is filed under.
    fn option_contracts(
        &self,
        underlying: &str,
        interval: BarInterval,
    ) -> Result<Vec<String>, DataError> {
        let folder = self
            .root
            .join(OPTION_SUBDIR)
            .join(safe_name(underlying)?)
            .join(interval.to_string());
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(DataError::Io { path: folder, source }),
        };
        let mut names = std::collections::BTreeSet::new();
        for path in entries.flatten().map(|entry| entry.path()) {
            // `2025-09-12.AOPT.csv`: the venue is the stem's second part.
            let Some(venue) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.split_once('.'))
                .map(|(_, venue)| venue.to_owned())
            else {
                continue;
            };
            let text = std::fs::read_to_string(&path).map_err(|source| DataError::Io {
                path: path.clone(),
                source,
            })?;
            for line in text.lines().skip(1) {
                if let Some(symbol) = line.split(',').next().filter(|s| !s.is_empty()) {
                    names.insert(format!("{symbol}.{venue}"));
                }
            }
        }
        Ok(names.into_iter().collect())
    }
}

#[cfg(test)]
mod tests;
