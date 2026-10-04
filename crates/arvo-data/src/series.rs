//! Named series of dated numbers, as one Parquet file (ADR-0039).
//!
//! An equity curve is two columns, a time and a number. Stored as JSON it
//! spells both field names out for every point, and across one project's
//! findings that was 88 MB of text for 1.6 million points. As Parquet the same
//! points are under 5 MB.
//!
//! # Identity is the values, not the bytes
//!
//! Two Parquet writers, or two versions of one, produce different bytes for
//! the same series. So [`identity`] hashes what the series *say* — each name,
//! each time, the raw bits of each number, in order — the way
//! [`BarProvider::fingerprint`](crate::BarProvider::fingerprint) hashes bars.
//! A file can be rewritten with another compression and still be the same
//! artifact.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use parquet::basic::{Compression, ZstdLevel};
use parquet::data_type::{ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RowAccessor;
use parquet::schema::parser::parse_message_type;

use crate::DataError;

const SCHEMA: &str = "message series {
    required binary series (STRING);
    required int64 at (TIMESTAMP(MILLIS,true));
    required double value;
}";

/// One named series: its points in order, each a time in milliseconds since
/// the epoch (UTC) and a value.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub points: Vec<(i64, f64)>,
}

/// A content hash of what `series` say, stable across writers, platforms and
/// toolchains.
#[must_use]
pub fn identity(series: &[Series]) -> String {
    let mut hasher = blake3::Hasher::new();
    for one in series {
        hasher.update(&(one.name.len() as u64).to_le_bytes());
        hasher.update(one.name.as_bytes());
        hasher.update(&(one.points.len() as u64).to_le_bytes());
        for (at, value) in &one.points {
            hasher.update(&at.to_le_bytes());
            hasher.update(&value.to_bits().to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Writes `series` as one Parquet file, compressed with zstd.
///
/// # Errors
///
/// [`DataError`] if the file cannot be created or written.
pub fn write(path: &Path, series: &[Series]) -> Result<(), DataError> {
    let failed = |reason: String| DataError::Parquet { path: path.to_path_buf(), reason };
    let schema = Arc::new(parse_message_type(SCHEMA).map_err(|err| failed(err.to_string()))?);
    let level = ZstdLevel::try_new(3).map_err(|err| failed(err.to_string()))?;
    let properties = Arc::new(WriterProperties::builder().set_compression(Compression::ZSTD(level)).build());
    let file = File::create(path).map_err(|source| DataError::Io { path: path.to_path_buf(), source })?;
    let mut writer = SerializedFileWriter::new(file, schema, properties).map_err(|err| failed(err.to_string()))?;

    let rows: usize = series.iter().map(|one| one.points.len()).sum();
    let mut names = Vec::with_capacity(rows);
    let mut times = Vec::with_capacity(rows);
    let mut values = Vec::with_capacity(rows);
    for one in series {
        // One name per point: dictionary encoding stores it once.
        let name = ByteArray::from(one.name.clone().into_bytes());
        for (at, value) in &one.points {
            names.push(name.clone());
            times.push(*at);
            values.push(*value);
        }
    }

    let mut group = writer.next_row_group().map_err(|err| failed(err.to_string()))?;
    let mut index = 0;
    while let Some(mut column) = group.next_column().map_err(|err| failed(err.to_string()))? {
        let written = match index {
            0 => column.typed::<ByteArrayType>().write_batch(&names, None, None),
            1 => column.typed::<Int64Type>().write_batch(&times, None, None),
            2 => column.typed::<DoubleType>().write_batch(&values, None, None),
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

/// Reads the series back, in the order they were written.
///
/// # Errors
///
/// [`DataError`] if the file cannot be opened or is not this schema.
pub fn read(path: &Path) -> Result<Vec<Series>, DataError> {
    let failed = |reason: String| DataError::Parquet { path: path.to_path_buf(), reason };
    let file = File::open(path).map_err(|source| DataError::Io { path: path.to_path_buf(), source })?;
    let reader = SerializedFileReader::new(file).map_err(|err| failed(err.to_string()))?;
    let rows = reader.get_row_iter(None).map_err(|err| failed(err.to_string()))?;
    let mut series: Vec<Series> = Vec::new();
    for row in rows {
        let row = row.map_err(|err| failed(err.to_string()))?;
        let name = row.get_string(0).map_err(|err| failed(err.to_string()))?;
        let at = row.get_timestamp_millis(1).map_err(|err| failed(err.to_string()))?;
        let value = row.get_double(2).map_err(|err| failed(err.to_string()))?;
        match series.last_mut() {
            Some(current) if &current.name == name => current.points.push((at, value)),
            _ => series.push(Series { name: name.clone(), points: vec![(at, value)] }),
        }
    }
    Ok(series)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curves() -> Vec<Series> {
        vec![
            Series { name: "/record/strategy_curve".to_owned(), points: vec![(1_693_958_400_000, 100_000.0), (1_694_044_800_000, 100_012.34)] },
            Series { name: "/record/benchmark_curve".to_owned(), points: vec![(1_693_958_400_000, 100_000.0), (1_694_044_800_000, 99_871.5)] },
        ]
    }

    #[test]
    fn series_survive_the_file_to_the_bit_and_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("curves.parquet");
        write(&path, &curves()).expect("written");
        let back = read(&path).expect("read back");
        assert_eq!(back, curves());
        assert_eq!(back[0].points[1].1.to_bits(), 100_012.34f64.to_bits());
    }

    #[test]
    fn identity_is_what_the_series_say_and_nothing_about_the_file() {
        let same = identity(&curves());
        assert_eq!(same, identity(&curves()));
        assert_eq!(same.len(), 64);

        // One value moves by the smallest amount a float can.
        let mut nudged = curves();
        nudged[0].points[1].1 = f64::from_bits(nudged[0].points[1].1.to_bits() + 1);
        assert_ne!(identity(&nudged), same);

        // The same points under another name are another artifact, and so are
        // the same series in another order.
        let mut renamed = curves();
        renamed[0].name.push('x');
        assert_ne!(identity(&renamed), same);
        let mut swapped = curves();
        swapped.swap(0, 1);
        assert_ne!(identity(&swapped), same);

        // Where a boundary between two series falls is part of what they say.
        let joined = vec![Series { name: "ab".to_owned(), points: Vec::new() }];
        let split = vec![Series { name: "a".to_owned(), points: Vec::new() }, Series { name: "b".to_owned(), points: Vec::new() }];
        assert_ne!(identity(&joined), identity(&split));
    }
}
