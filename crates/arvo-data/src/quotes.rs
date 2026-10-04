//! Recorded option quotes, as they sit on disk (ADR-0039).
//!
//! A day of one underlying's chains is one file. While the day is being
//! recorded it is a CSV, because the recorder appends a snapshot every fifteen
//! minutes and a Parquet file cannot be appended to. Once the day is over it is
//! compacted to Parquet: the same rows, a seventeenth of the bytes, and
//! readable by anything that reads Parquet.
//!
//! The recording had no reader at all before this. It exists so the option
//! cost model can be measured rather than assumed, and a folder of CSVs nobody
//! opens measures nothing.
//!
//! # What compaction may drop
//!
//! Only a snapshot that is the one before it over again: the same contracts,
//! the same quotes at the same times, beside the same underlying price. A
//! closed market answers every request that way, and the recorder ran through
//! two weekends before it knew what a weekend was (arvo-engine#28). Nothing
//! that says anything is dropped, and the count of what was is returned.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use parquet::basic::{Compression, ZstdLevel};
use parquet::data_type::{ByteArray, ByteArrayType, DoubleType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::writer::SerializedFileWriter;
use parquet::record::{Field, RowAccessor};
use parquet::schema::parser::parse_message_type;

use crate::DataError;

/// The CSV's first line, and the order of the Parquet file's columns.
pub const HEADER: &str = "recorded_at,symbol,expiration,right,strike,quote_at,bid,ask,bid_size,ask_size,underlying_bid,underlying_ask";

/// The same columns as a Parquet schema. Times are UTC, to the millisecond.
const SCHEMA: &str = "message option_quote {
    required int64 recorded_at (TIMESTAMP(MILLIS,true));
    required binary symbol (STRING);
    required int32 expiration (DATE);
    required binary right (STRING);
    required double strike;
    required int64 quote_at (TIMESTAMP(MILLIS,true));
    required double bid;
    required double ask;
    required double bid_size;
    required double ask_size;
    required double underlying_bid;
    required double underlying_ask;
}";

/// One contract's quote in one snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    /// When the snapshot was taken, in milliseconds since the epoch, UTC.
    pub recorded_at: i64,
    /// The OCC symbol.
    pub symbol: String,
    pub expiration: NaiveDate,
    /// `C` or `P`.
    pub right: char,
    pub strike: f64,
    /// When the venue stamped the quote, in milliseconds since the epoch, UTC.
    pub quote_at: i64,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    pub underlying_bid: f64,
    pub underlying_ask: f64,
}

impl Quote {
    /// Half the distance between the bid and the ask, in dollars per share.
    #[must_use]
    pub fn half_spread(&self) -> f64 {
        (self.ask - self.bid) / 2.0
    }

    /// The price halfway between the bid and the ask.
    #[must_use]
    pub fn mid(&self) -> f64 {
        (self.ask + self.bid) / 2.0
    }

    /// What this row says apart from when its snapshot was taken.
    fn says(&self) -> (&str, i64, [u64; 6]) {
        (
            &self.symbol,
            self.quote_at,
            [self.bid, self.ask, self.bid_size, self.ask_size, self.underlying_bid, self.underlying_ask]
                .map(f64::to_bits),
        )
    }
}

/// Reads a day's quotes from either form, by the file's extension.
///
/// # Errors
///
/// [`DataError`] if the file cannot be read or does not hold quotes.
pub fn read(path: &Path) -> Result<Vec<Quote>, DataError> {
    if path.extension().is_some_and(|ext| ext == "parquet") {
        read_parquet(path)
    } else {
        read_csv(path)
    }
}

/// Reads a day as the recorder wrote it.
///
/// A last line that does not parse is left out: the recorder writes a snapshot
/// in one call, so a crash can tear at most the final line, and that is not a
/// reason to lose the day. A bad line anywhere else is an error.
///
/// # Errors
///
/// [`DataError::Io`] if the file cannot be read, [`DataError::Malformed`] for
/// a wrong header or a bad line that is not the last.
pub fn read_csv(path: &Path) -> Result<Vec<Quote>, DataError> {
    let text = std::fs::read_to_string(path).map_err(|source| DataError::Io { path: path.to_path_buf(), source })?;
    let mut lines = text.lines().enumerate().peekable();
    match lines.next() {
        Some((_, header)) if header.trim() == HEADER => {}
        Some((_, header)) => {
            return Err(DataError::Malformed {
                path: path.to_path_buf(),
                line: 1,
                reason: format!("expected the header {HEADER:?}, found {header:?}"),
            })
        }
        None => return Ok(Vec::new()),
    }
    let mut quotes = Vec::new();
    while let Some((index, line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_row(line) {
            Ok(quote) => quotes.push(quote),
            Err(_) if lines.peek().is_none() => break,
            Err(reason) => return Err(DataError::Malformed { path: path.to_path_buf(), line: index + 1, reason }),
        }
    }
    Ok(quotes)
}

fn parse_row(line: &str) -> Result<Quote, String> {
    let fields: Vec<&str> = line.trim().split(',').collect();
    let expected = HEADER.split(',').count();
    if fields.len() != expected {
        return Err(format!("{} fields where {expected} were expected", fields.len()));
    }
    let instant = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .map(|at| at.with_timezone(&Utc).timestamp_millis())
            .map_err(|err| format!("{text:?} is not a time: {err}"))
    };
    let number = |text: &str| text.parse::<f64>().map_err(|err| format!("{text:?} is not a number: {err}"));
    let right = match fields[3] {
        "C" => 'C',
        "P" => 'P',
        other => return Err(format!("{other:?} is neither C nor P")),
    };
    Ok(Quote {
        recorded_at: instant(fields[0])?,
        symbol: fields[1].to_owned(),
        expiration: NaiveDate::parse_from_str(fields[2], "%Y-%m-%d")
            .map_err(|err| format!("{:?} is not a date: {err}", fields[2]))?,
        right,
        strike: number(fields[4])?,
        quote_at: instant(fields[5])?,
        bid: number(fields[6])?,
        ask: number(fields[7])?,
        bid_size: number(fields[8])?,
        ask_size: number(fields[9])?,
        underlying_bid: number(fields[10])?,
        underlying_ask: number(fields[11])?,
    })
}

/// Days from the epoch, which is how Parquet keeps a date.
fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid")
}

/// Writes `quotes` as one Parquet file, compressed with zstd.
///
/// # Errors
///
/// [`DataError`] if the file cannot be created or written.
pub fn write_parquet(path: &Path, quotes: &[Quote]) -> Result<(), DataError> {
    let failed = |reason: String| DataError::Parquet { path: path.to_path_buf(), reason };
    let schema = Arc::new(parse_message_type(SCHEMA).map_err(|err| failed(err.to_string()))?);
    let level = ZstdLevel::try_new(3).map_err(|err| failed(err.to_string()))?;
    let properties = Arc::new(WriterProperties::builder().set_compression(Compression::ZSTD(level)).build());
    let file = File::create(path).map_err(|source| DataError::Io { path: path.to_path_buf(), source })?;
    let mut writer = SerializedFileWriter::new(file, schema, properties).map_err(|err| failed(err.to_string()))?;

    let longs = |pick: fn(&Quote) -> i64| quotes.iter().map(pick).collect::<Vec<i64>>();
    let doubles = |pick: fn(&Quote) -> f64| quotes.iter().map(pick).collect::<Vec<f64>>();
    let texts = |pick: fn(&Quote) -> String| {
        quotes.iter().map(|quote| ByteArray::from(pick(quote).into_bytes())).collect::<Vec<ByteArray>>()
    };

    let mut group = writer.next_row_group().map_err(|err| failed(err.to_string()))?;
    let mut index = 0;
    while let Some(mut column) = group.next_column().map_err(|err| failed(err.to_string()))? {
        let written = match index {
            0 => column.typed::<Int64Type>().write_batch(&longs(|q| q.recorded_at), None, None),
            1 => column.typed::<ByteArrayType>().write_batch(&texts(|q| q.symbol.clone()), None, None),
            2 => {
                let days: Vec<i32> = quotes
                    .iter()
                    .map(|quote| i32::try_from((quote.expiration - epoch()).num_days()).unwrap_or(i32::MAX))
                    .collect();
                column.typed::<Int32Type>().write_batch(&days, None, None)
            }
            3 => column.typed::<ByteArrayType>().write_batch(&texts(|q| q.right.to_string()), None, None),
            4 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.strike), None, None),
            5 => column.typed::<Int64Type>().write_batch(&longs(|q| q.quote_at), None, None),
            6 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.bid), None, None),
            7 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.ask), None, None),
            8 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.bid_size), None, None),
            9 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.ask_size), None, None),
            10 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.underlying_bid), None, None),
            11 => column.typed::<DoubleType>().write_batch(&doubles(|q| q.underlying_ask), None, None),
            other => return Err(failed(format!("the schema has a column {other} this writer does not know"))),
        };
        written.map_err(|err| failed(err.to_string()))?;
        column.close().map_err(|err| failed(err.to_string()))?;
        index += 1;
    }
    group.close().map_err(|err| failed(err.to_string()))?;
    writer.close().map_err(|err| failed(err.to_string()))?;
    Ok(())
}

/// Reads a compacted day.
///
/// # Errors
///
/// [`DataError`] if the file cannot be opened or is not this schema.
pub fn read_parquet(path: &Path) -> Result<Vec<Quote>, DataError> {
    let failed = |reason: String| DataError::Parquet { path: path.to_path_buf(), reason };
    let file = File::open(path).map_err(|source| DataError::Io { path: path.to_path_buf(), source })?;
    let reader = SerializedFileReader::new(file).map_err(|err| failed(err.to_string()))?;
    let rows = reader.get_row_iter(None).map_err(|err| failed(err.to_string()))?;
    let mut quotes = Vec::new();
    for row in rows {
        let row = row.map_err(|err| failed(err.to_string()))?;
        let quote = (|| -> parquet::errors::Result<Quote> {
            Ok(Quote {
                recorded_at: row.get_timestamp_millis(0)?,
                symbol: row.get_string(1)?.clone(),
                // The record API has an accessor for every type but a date.
                expiration: match row.get_column_iter().nth(2) {
                    Some((_, Field::Date(days))) => epoch() + chrono::Duration::days(i64::from(*days)),
                    other => {
                        return Err(parquet::errors::ParquetError::General(format!(
                            "the expiration column holds {:?}, not a date",
                            other.map(|(_, field)| field)
                        )))
                    }
                },
                right: row.get_string(3)?.chars().next().unwrap_or('?'),
                strike: row.get_double(4)?,
                quote_at: row.get_timestamp_millis(5)?,
                bid: row.get_double(6)?,
                ask: row.get_double(7)?,
                bid_size: row.get_double(8)?,
                ask_size: row.get_double(9)?,
                underlying_bid: row.get_double(10)?,
                underlying_ask: row.get_double(11)?,
            })
        })()
        .map_err(|err| failed(err.to_string()))?;
        quotes.push(quote);
    }
    Ok(quotes)
}

/// Leaves out every snapshot that is the one kept before it over again.
///
/// Returns what is kept and how many snapshots were dropped. A snapshot is the
/// run of rows sharing a `recorded_at`.
#[must_use]
pub fn without_repeats(quotes: Vec<Quote>) -> (Vec<Quote>, usize) {
    let mut kept: Vec<Quote> = Vec::with_capacity(quotes.len());
    let mut last: Option<std::ops::Range<usize>> = None;
    let mut dropped = 0;
    let mut start = 0;
    while start < quotes.len() {
        let end = quotes[start..]
            .iter()
            .position(|quote| quote.recorded_at != quotes[start].recorded_at)
            .map_or(quotes.len(), |offset| start + offset);
        let snapshot = &quotes[start..end];
        let repeats = last.as_ref().is_some_and(|range| {
            let before = &kept[range.clone()];
            before.len() == snapshot.len() && before.iter().zip(snapshot).all(|(was, now)| was.says() == now.says())
        });
        if repeats {
            dropped += 1;
        } else {
            let from = kept.len();
            kept.extend_from_slice(snapshot);
            last = Some(from..kept.len());
        }
        start = end;
    }
    (kept, dropped)
}

/// What compacting one day did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// The Parquet file written.
    pub path: PathBuf,
    /// Rows the CSV held.
    pub rows: usize,
    /// Rows kept, which is `rows` less the repeated snapshots.
    pub kept: usize,
    /// Snapshots left out because they repeated the one before.
    pub repeats: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Rewrites a finished day's CSV as Parquet beside it, and removes the CSV
/// only once the Parquet file has been read back and holds exactly what was
/// meant to be kept.
///
/// The Parquet file is written under a temporary name and renamed, so a crash
/// leaves the CSV and at worst a stray partial file, never half a day.
///
/// # Errors
///
/// [`DataError`] if the CSV cannot be read, the Parquet file cannot be written,
/// or what was read back differs. In every case the CSV is still there.
pub fn compact(csv: &Path) -> Result<Compacted, DataError> {
    let bytes_before = std::fs::metadata(csv).map_err(|source| DataError::Io { path: csv.to_path_buf(), source })?.len();
    let recorded = read_csv(csv)?;
    let rows = recorded.len();
    let (kept, repeats) = without_repeats(recorded);

    let path = csv.with_extension("parquet");
    let partial = csv.with_extension("parquet.partial");
    write_parquet(&partial, &kept)?;
    let back = read_parquet(&partial)?;
    if back != kept {
        let _ = std::fs::remove_file(&partial);
        return Err(DataError::Parquet {
            path,
            reason: format!(
                "read back {} rows that are not the {} written; the CSV is kept",
                back.len(),
                kept.len()
            ),
        });
    }
    std::fs::rename(&partial, &path).map_err(|source| DataError::Io { path: path.clone(), source })?;
    let bytes_after = std::fs::metadata(&path).map_err(|source| DataError::Io { path: path.clone(), source })?.len();
    std::fs::remove_file(csv).map_err(|source| DataError::Io { path: csv.to_path_buf(), source })?;
    Ok(Compacted { path, rows, kept: kept.len(), repeats, bytes_before, bytes_after })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(recorded_at: &str, symbol: &str, bid: f64) -> Quote {
        let instant = |text: &str| DateTime::parse_from_rfc3339(text).expect("valid").timestamp_millis();
        Quote {
            recorded_at: instant(recorded_at),
            symbol: symbol.to_owned(),
            expiration: NaiveDate::from_ymd_opt(2026, 10, 5).expect("valid"),
            right: if symbol.contains('C') { 'C' } else { 'P' },
            strike: 710.0,
            quote_at: instant("2026-10-02T19:59:59.420Z"),
            bid,
            ask: bid + 0.93,
            bid_size: 50.0,
            ask_size: 3.0,
            underlying_bid: 769.64,
            underlying_ask: 769.78,
        }
    }

    const DAY: &str = "\
recorded_at,symbol,expiration,right,strike,quote_at,bid,ask,bid_size,ask_size,underlying_bid,underlying_ask
2026-10-02T14:38:26Z,SPY261005C00710000,2026-10-05,C,710,2026-10-02T14:38:20.420Z,59.28,60.21,50,3,769.64,769.78
2026-10-02T14:38:26Z,SPY261005P00710000,2026-10-05,P,710,2026-10-02T14:38:21.537Z,0.01,0.02,2,2,769.64,769.78
2026-10-02T14:53:26Z,SPY261005C00710000,2026-10-05,C,710,2026-10-02T14:53:20.420Z,59.3,60.25,50,3,769.7,769.8
2026-10-02T14:53:26Z,SPY261005P00710000,2026-10-05,P,710,2026-10-02T14:38:21.537Z,0.01,0.02,2,2,769.7,769.8
";

    #[test]
    fn a_day_survives_parquet_to_the_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let csv = dir.path().join("2026-10-02.csv");
        std::fs::write(&csv, DAY).expect("write");
        let recorded = read_csv(&csv).expect("the recorder's own format");
        assert_eq!(recorded.len(), 4);
        assert_eq!(recorded[0].right, 'C');
        assert!((recorded[0].half_spread() - 0.465).abs() < 1e-9);

        let parquet = dir.path().join("2026-10-02.parquet");
        write_parquet(&parquet, &recorded).expect("written");
        let back = read(&parquet).expect("read back");
        assert_eq!(back, recorded);
        for (was, now) in recorded.iter().zip(&back) {
            assert_eq!(was.bid.to_bits(), now.bid.to_bits(), "the same number, not a near one");
        }
    }

    #[test]
    fn compaction_drops_only_a_snapshot_that_repeats_the_one_before() {
        let first = "2026-10-03T13:34:00Z";
        let quotes = vec![
            quote(first, "SPY261005C00710000", 59.28),
            quote(first, "SPY261005P00710000", 0.01),
            // A closed market: fifteen minutes later, the same chain.
            quote("2026-10-03T13:49:00Z", "SPY261005C00710000", 59.28),
            quote("2026-10-03T13:49:00Z", "SPY261005P00710000", 0.01),
            // One quote moves, so this one says something.
            quote("2026-10-03T14:04:00Z", "SPY261005C00710000", 59.30),
            quote("2026-10-03T14:04:00Z", "SPY261005P00710000", 0.01),
            // And then the market is still again.
            quote("2026-10-03T14:19:00Z", "SPY261005C00710000", 59.30),
            quote("2026-10-03T14:19:00Z", "SPY261005P00710000", 0.01),
            // A contract leaving the chain is a change too.
            quote("2026-10-03T14:34:00Z", "SPY261005C00710000", 59.30),
        ];
        let (kept, dropped) = without_repeats(quotes);
        assert_eq!(dropped, 2);
        let snapshots: Vec<i64> = {
            let mut times: Vec<i64> = kept.iter().map(|quote| quote.recorded_at).collect();
            times.dedup();
            times
        };
        assert_eq!(snapshots.len(), 3, "the first, the one that moved, and the one that lost a contract");
        assert_eq!(kept.len(), 5);
    }

    #[test]
    fn a_compacted_day_replaces_its_csv_and_a_torn_last_line_is_not_the_day() {
        let dir = tempfile::tempdir().expect("tempdir");
        let csv = dir.path().join("2026-10-02.csv");
        // The recorder crashed mid-write once: the last line is half a row.
        std::fs::write(&csv, format!("{DAY}2026-10-02T15:08:26Z,SPY261005C0071")).expect("write");

        let done = compact(&csv).expect("compacted");
        assert_eq!((done.rows, done.kept, done.repeats), (4, 4, 0));
        assert!(!csv.exists(), "the CSV goes once the Parquet file is proven");
        assert!(done.path.ends_with("2026-10-02.parquet"));
        assert!(!dir.path().join("2026-10-02.parquet.partial").exists());
        assert_eq!(read(&done.path).expect("read").len(), 4);
    }

    #[test]
    fn a_bad_line_in_the_middle_keeps_the_csv() {
        let dir = tempfile::tempdir().expect("tempdir");
        let csv = dir.path().join("2026-10-02.csv");
        let broken = DAY.replacen("59.28", "not-a-price", 1);
        std::fs::write(&csv, &broken).expect("write");
        let refused = compact(&csv).expect_err("a day that cannot be read is not compacted");
        assert!(matches!(refused, DataError::Malformed { line: 2, .. }), "{refused}");
        assert!(csv.exists(), "and nothing is removed");
        assert!(!dir.path().join("2026-10-02.parquet").exists());
    }
}
