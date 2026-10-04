//! A finding's series, kept apart from its record (ADR-0037, ADR-0039).
//!
//! A finding is two things. The record is what was asked, what was concluded
//! and what it rested on. The artifact is the series the run produced: its
//! equity curves. On one project the curves were 88 percent of the store, and
//! one of the two is buy-and-hold over bars the library already holds.
//!
//! So a record is written without its curves. They go into one Parquet file
//! under `artifacts/`, named by a hash of what they say, and the record keeps
//! a reference: the hash, and for each curve where it belongs, how many points
//! it has and where it ended. Reading a finding puts them back, and everything
//! above the store sees the finding it always saw.
//!
//! # Nothing is taken out that cannot be put back exactly
//!
//! Before a curve leaves the record, every time in it is turned into the
//! number the file will hold and back into text, and has to come back the
//! same. The file is then read and compared with what was meant to be in it.
//! A curve that fails either check stays in the record, as every curve did
//! before this. The split saves space; it is never allowed to cost a point.
//!
//! ponytail: the ledgers stay in the record. They are under one percent of
//! the bytes and are rows of mixed fields, not two columns. Move them when a
//! ledger is what fills a store.

use std::path::Path;

use arvo_data::series::Series;
use chrono::{DateTime, NaiveDateTime};
use serde_json::{json, Value};

/// Under the evidence store: one Parquet file per artifact, named by its hash.
pub const ARTIFACTS_SUBDIR: &str = "artifacts";

/// The key, at the top of a stored finding, that says where its curves are.
const KEY: &str = "artifact";

/// The fields that hold an equity curve, wherever in a record they appear.
const CURVES: [&str; 3] = ["strategy_curve", "benchmark_curve", "combined_curve"];

/// One point, if it is exactly `{"at": <time>, "equity": <number>}` and its
/// time survives the trip to milliseconds and back unchanged.
fn point(value: &Value) -> Option<(i64, f64)> {
    let object = value.as_object()?;
    if object.len() != 2 {
        return None;
    }
    let text = object.get("at")?;
    let equity = object.get("equity")?.as_f64()?;
    let at: NaiveDateTime = serde_json::from_value(text.clone()).ok()?;
    let millis = at.and_utc().timestamp_millis();
    (&stamp(millis)? == text).then_some((millis, equity))
}

/// A time as a record spells it.
fn stamp(millis: i64) -> Option<Value> {
    serde_json::to_value(DateTime::from_timestamp_millis(millis)?.naive_utc()).ok()
}

/// Finds every curve under `value`, by JSON pointer, without changing it.
fn find(value: &Value, pointer: &mut String, found: &mut Vec<Series>) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                let length = pointer.len();
                pointer.push('/');
                pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                let curve = CURVES
                    .contains(&key.as_str())
                    .then(|| child.as_array())
                    .flatten()
                    .filter(|points| !points.is_empty())
                    .and_then(|points| points.iter().map(point).collect::<Option<Vec<_>>>());
                match curve {
                    Some(points) => found.push(Series { name: pointer.clone(), points }),
                    None => find(child, pointer, found),
                }
                pointer.truncate(length);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let length = pointer.len();
                pointer.push('/');
                pointer.push_str(&index.to_string());
                find(child, pointer, found);
                pointer.truncate(length);
            }
        }
        _ => {}
    }
}

/// Moves the curves of a finding about to be written into an artifact under
/// `root`, leaving a reference in their place.
///
/// Returns whether anything moved. `value` is changed only after the artifact
/// is on disk and has been read back, so on an error it is as it was and can
/// be written whole.
///
/// # Errors
///
/// The artifact could not be written, or did not read back as written.
pub(super) fn split(root: &Path, value: &mut Value) -> Result<bool, String> {
    let mut curves = Vec::new();
    find(value, &mut String::new(), &mut curves);
    if curves.is_empty() {
        return Ok(false);
    }
    let hash = arvo_data::series::identity(&curves);
    let folder = root.join(ARTIFACTS_SUBDIR);
    let path = folder.join(format!("{hash}.parquet"));
    if !path.is_file() {
        std::fs::create_dir_all(&folder).map_err(|err| format!("{}: {err}", folder.display()))?;
        let partial = folder.join(format!("{hash}.parquet.partial"));
        arvo_data::series::write(&partial, &curves).map_err(|err| err.to_string())?;
        std::fs::rename(&partial, &path).map_err(|err| format!("{}: {err}", path.display()))?;
    }
    // Read back whether it was just written or was already there: the name
    // says what the file should hold, and this is the one moment to find out
    // that it does not, while the curves are still in hand.
    let held = arvo_data::series::read(&path).map_err(|err| err.to_string())?;
    if held != curves {
        return Err(format!("{} does not hold the curves it is named for", path.display()));
    }

    let series: Vec<Value> = curves
        .iter()
        .map(|curve| {
            json!({
                "path": curve.name,
                "points": curve.points.len(),
                "last": curve.points.last().map(|(_, equity)| *equity),
            })
        })
        .collect();
    for curve in &curves {
        if let Some(slot) = value.pointer_mut(&curve.name) {
            *slot = Value::Array(Vec::new());
        }
    }
    if let Some(fields) = value.as_object_mut() {
        fields.insert(KEY.to_owned(), json!({ "hash": hash, "series": series }));
    }
    Ok(true)
}

/// Puts a stored finding's curves back where they were, from its artifact
/// under `root`. A finding with no reference is left alone: it carries its
/// curves, as every finding written before this did.
///
/// # Errors
///
/// The reference names no artifact, the artifact is not on disk or cannot be
/// read, or it holds a curve the record has no place for.
pub(super) fn join(root: &Path, value: &mut Value) -> Result<(), String> {
    let Some(reference) = value.as_object_mut().and_then(|fields| fields.remove(KEY)) else {
        return Ok(());
    };
    let hash = reference
        .get("hash")
        .and_then(Value::as_str)
        .filter(|hash| hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| "its artifact reference names no artifact".to_owned())?;
    let path = root.join(ARTIFACTS_SUBDIR).join(format!("{hash}.parquet"));
    if !path.is_file() {
        return Err(format!("its curves are kept in {} and that file is not there", path.display()));
    }
    for curve in arvo_data::series::read(&path).map_err(|err| format!("its curves could not be read: {err}"))? {
        let points = curve
            .points
            .iter()
            .map(|(at, equity)| Some(json!({ "at": stamp(*at)?, "equity": equity })))
            .collect::<Option<Vec<Value>>>()
            .ok_or_else(|| format!("{} holds a time that is not one", path.display()))?;
        let slot = value
            .pointer_mut(&curve.name)
            .ok_or_else(|| format!("{} holds a curve for {}, which the record has no place for", path.display(), curve.name))?;
        *slot = Value::Array(points);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding() -> Value {
        json!({
            "id": "20260914T170439317-TQQQ.RH",
            "schema": 2,
            "record": {
                "kind": "study",
                "verdict": "NotSupported",
                "out_of_sample_evidence": { "evaluation": {
                    "strategy_curve": [
                        { "at": "2023-09-06T00:00:00", "equity": 100000.0 },
                        { "at": "2023-09-07T00:00:00", "equity": 100012.34 }
                    ],
                    "benchmark_curve": [
                        { "at": "2023-09-06T00:00:00", "equity": 100000.0 },
                        { "at": "2023-09-07T00:00:00", "equity": 99871.5 }
                    ],
                    "strategy_ledger": [ { "instrument": "TQQQ.RH", "pnl": 2193.34 } ]
                } },
                "per_instrument": [
                    { "kept": { "strategy_curve": [ { "at": "2026-06-01T13:30:00", "equity": 100000.0 } ] } }
                ]
            }
        })
    }

    #[test]
    fn curves_leave_the_record_and_come_back_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let whole = finding();
        let mut stored = whole.clone();
        assert!(split(dir.path(), &mut stored).expect("split"));

        // The record no longer carries a point, and says where they went.
        let evaluation = &stored["record"]["out_of_sample_evidence"]["evaluation"];
        assert_eq!(evaluation["strategy_curve"], json!([]));
        assert_eq!(evaluation["benchmark_curve"], json!([]));
        assert_eq!(evaluation["strategy_ledger"], whole["record"]["out_of_sample_evidence"]["evaluation"]["strategy_ledger"], "ledgers stay");
        let reference = &stored["artifact"];
        let hash = reference["hash"].as_str().expect("a hash");
        assert!(dir.path().join(ARTIFACTS_SUBDIR).join(format!("{hash}.parquet")).is_file());
        assert_eq!(reference["series"].as_array().expect("series").len(), 3, "a curve nested in a list is found too");
        let strategy = reference["series"]
            .as_array()
            .expect("series")
            .iter()
            .find(|one| one["path"] == "/record/out_of_sample_evidence/evaluation/strategy_curve")
            .expect("the strategy curve");
        assert_eq!((strategy["points"].as_u64(), strategy["last"].as_f64()), (Some(2), Some(100_012.34)));

        // And reading puts every point back where it was.
        let mut read = stored.clone();
        join(dir.path(), &mut read).expect("join");
        assert_eq!(read, whole);

        // The same curves again are the same artifact: nothing new is written.
        let mut again = whole.clone();
        assert!(split(dir.path(), &mut again).expect("split"));
        assert_eq!(again["artifact"]["hash"], stored["artifact"]["hash"]);
        assert_eq!(std::fs::read_dir(dir.path().join(ARTIFACTS_SUBDIR)).expect("folder").count(), 1);
    }

    #[test]
    fn a_curve_that_would_not_come_back_the_same_stays_in_the_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A date with no time is how the first findings were written. It does
        // not survive the trip to milliseconds and back as the same text.
        let old = json!({ "record": { "strategy_curve": [ { "date": "2019-06-01", "equity": 100000.0 } ] } });
        let mut stored = old.clone();
        assert!(!split(dir.path(), &mut stored).expect("nothing to move"));
        assert_eq!(stored, old);

        // Finer than a millisecond, which the file does not keep.
        let fine = json!({ "record": { "strategy_curve": [ { "at": "2026-06-01T13:30:00.000000001", "equity": 1.0 } ] } });
        let mut stored = fine.clone();
        assert!(!split(dir.path(), &mut stored).expect("nothing to move"));
        assert_eq!(stored, fine);

        // A finding with no reference reads as it is.
        let mut plain = finding();
        join(dir.path(), &mut plain).expect("nothing to join");
        assert_eq!(plain, finding());
    }

    #[test]
    fn a_missing_artifact_is_said_and_not_drawn_as_an_empty_chart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut stored = finding();
        split(dir.path(), &mut stored).expect("split");
        std::fs::remove_dir_all(dir.path().join(ARTIFACTS_SUBDIR)).expect("remove");
        let reason = join(dir.path(), &mut stored).expect_err("the curves are gone");
        assert!(reason.contains("that file is not there"), "{reason}");
    }
}
