//! What a portfolio was worth, on the days it was looked at.
//!
//! A holdings file says what you hold *now*. Almost everything interesting
//! about a portfolio is a *change*: what it gained this month, how the
//! allocation drifted, whether a position grew because you bought more or
//! because it went up. None of that is answerable from a single snapshot, and
//! a broker export gives you exactly one.
//!
//! So each time a portfolio is valued, the result is written down.
//!
//! # One per day, last write wins
//!
//! Keyed by date, so opening the app five times in an afternoon leaves one
//! record rather than five, and the series stays a daily one — which is what
//! a chart of it wants. Growth is bounded at 365 files per portfolio per year.
//!
//! # The valued form, not the raw holdings
//!
//! Prices are not recoverable after the fact — the whole point is what it was
//! worth *then*. Storing the holdings and re-pricing them later with today's
//! prices would produce a flat line that says nothing.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::ValuedPortfolio;

/// One day's valuation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub taken_on: NaiveDate,
    pub portfolio: ValuedPortfolio,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("writing {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("reading {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("encoding a snapshot")]
    Encode(#[source] serde_json::Error),
}

/// Everything a load found, including what it could not read.
#[derive(Debug, Default)]
pub struct LoadedHistory {
    /// Oldest first — the order a chart wants to draw them in.
    pub snapshots: Vec<Snapshot>,
    pub problems: Vec<String>,
}

/// Snapshots on disk, one JSON file per portfolio per day.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

/// Reduces a portfolio name to something safe in a file name.
///
/// Names come from file names and broker account labels, so this is a trust
/// boundary. Anything that is only punctuation gets a real name, so `..`
/// cannot survive as a path component.
fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(48)
        .collect();
    if cleaned.trim_matches('-').is_empty() {
        "portfolio".to_owned()
    } else {
        cleaned
    }
}

impl SnapshotStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Records a valuation for `taken_on`, replacing any earlier one that day.
    ///
    /// The date is in the file name, so the replacement is the filesystem's
    /// job rather than a read-modify-write. Two snapshots on one day is not a
    /// meaningful distinction for a daily series, and the later one is the
    /// better record of that day.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] if the directory cannot be created, the
    /// snapshot cannot be encoded, or the file cannot be written.
    pub fn record(
        &self,
        portfolio: &ValuedPortfolio,
        taken_on: NaiveDate,
    ) -> Result<PathBuf, HistoryError> {
        std::fs::create_dir_all(&self.root).map_err(|source| HistoryError::Write {
            path: self.root.clone(),
            source,
        })?;

        let path = self
            .root
            .join(format!("{}-{taken_on}.json", slug(&portfolio.name)));
        let snapshot = Snapshot {
            taken_on,
            portfolio: portfolio.clone(),
        };
        let encoded = serde_json::to_vec_pretty(&snapshot).map_err(HistoryError::Encode)?;
        std::fs::write(&path, encoded).map_err(|source| HistoryError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Every snapshot for one portfolio, oldest first.
    ///
    /// A missing directory is an empty history, not an error — that is the
    /// state before anything has ever been valued.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::Read`] only if the directory cannot be listed.
    /// Individual unreadable files land in [`LoadedHistory::problems`], because
    /// a store that silently drops what it cannot parse is indistinguishable
    /// from an empty one.
    pub fn history(&self, name: &str) -> Result<LoadedHistory, HistoryError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedHistory::default())
            }
            Err(source) => {
                return Err(HistoryError::Read {
                    path: self.root.clone(),
                    source,
                })
            }
        };

        // Matched on the stored name rather than the file name, so a slug
        // collision between two portfolios cannot merge their histories.
        let mut loaded = LoadedHistory::default();
        for path in entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        {
            match read_snapshot(&path) {
                Ok(snapshot) if snapshot.portfolio.name == name => loaded.snapshots.push(snapshot),
                Ok(_) => {}
                Err(problem) => loaded.problems.push(problem),
            }
        }

        loaded.snapshots.sort_by_key(|snapshot| snapshot.taken_on);
        Ok(loaded)
    }
}

fn read_snapshot(path: &Path) -> Result<Snapshot, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

/// The change between the two most recent snapshots.
///
/// `None` until there are two — a single observation has nothing to change
/// from, and reporting `+0.00` would claim a flat day that was never measured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub absolute: f64,
    /// `None` when the earlier value was zero.
    pub percent: Option<f64>,
}

#[must_use]
pub fn latest_change(snapshots: &[Snapshot]) -> Option<Change> {
    let [.., previous, latest] = snapshots else {
        return None;
    };
    let (before, after) = (previous.portfolio.total_value, latest.portfolio.total_value);
    Some(Change {
        from: previous.taken_on,
        to: latest.taken_on,
        absolute: after - before,
        percent: (before != 0.0).then(|| (after - before) / before),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Holding, Portfolio};
    use std::collections::BTreeMap;

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, day).expect("valid")
    }

    fn valued(name: &str, price: f64) -> ValuedPortfolio {
        Portfolio {
            name: name.to_owned(),
            as_of: date(1),
            holdings: vec![Holding {
                instrument: "A.X".to_owned(),
                quantity: Some(10.0),
                cost_basis: Some(500.0),
                price: Some(price),
                value: None,
            }],
        }
        .value(&BTreeMap::new())
    }

    #[test]
    fn snapshots_come_back_oldest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path());
        for (day, price) in [(3, 70.0), (1, 50.0), (2, 60.0)] {
            store
                .record(&valued("main", price), date(day))
                .expect("writes");
        }

        let history = store.history("main").expect("reads");
        assert!(history.problems.is_empty(), "{:?}", history.problems);
        let values: Vec<f64> = history
            .snapshots
            .iter()
            .map(|s| s.portfolio.total_value)
            .collect();
        assert_eq!(values, vec![500.0, 600.0, 700.0], "a chart draws in order");
    }

    #[test]
    fn a_second_look_on_the_same_day_replaces_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path());
        store
            .record(&valued("main", 50.0), date(1))
            .expect("writes");
        store
            .record(&valued("main", 55.0), date(1))
            .expect("writes");

        let history = store.history("main").expect("reads");
        assert_eq!(history.snapshots.len(), 1, "one record per day");
        assert!(
            (history.snapshots[0].portfolio.total_value - 550.0).abs() < 1e-9,
            "and it is the later look"
        );
    }

    #[test]
    fn one_portfolios_history_never_includes_anothers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path());
        store
            .record(&valued("main", 50.0), date(1))
            .expect("writes");
        store
            .record(&valued("retirement", 90.0), date(1))
            .expect("writes");

        assert_eq!(store.history("main").expect("reads").snapshots.len(), 1);
        assert_eq!(
            store.history("retirement").expect("reads").snapshots.len(),
            1
        );
        assert_eq!(store.history("nothing").expect("reads").snapshots.len(), 0);
    }

    #[test]
    fn a_single_snapshot_has_no_change_to_report() {
        let one = vec![Snapshot {
            taken_on: date(1),
            portfolio: valued("main", 50.0),
        }];
        assert_eq!(
            latest_change(&one),
            None,
            "one observation cannot have changed from anything"
        );
        assert_eq!(latest_change(&[]), None);
    }

    #[test]
    fn change_is_measured_between_the_two_most_recent() {
        let snapshots = vec![
            Snapshot {
                taken_on: date(1),
                portfolio: valued("main", 50.0),
            },
            Snapshot {
                taken_on: date(2),
                portfolio: valued("main", 40.0),
            },
            Snapshot {
                taken_on: date(3),
                portfolio: valued("main", 60.0),
            },
        ];
        let change = latest_change(&snapshots).expect("three snapshots");

        assert_eq!((change.from, change.to), (date(2), date(3)));
        assert!(
            (change.absolute - 200.0).abs() < 1e-9,
            "600 - 400, not 600 - 500"
        );
        assert!((change.percent.expect("nonzero base") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn an_unreadable_snapshot_is_reported_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path());
        store
            .record(&valued("main", 50.0), date(1))
            .expect("writes");
        std::fs::write(dir.path().join("broken.json"), b"{ not json").expect("writes");

        let history = store.history("main").expect("reads");
        assert_eq!(history.snapshots.len(), 1);
        assert_eq!(history.problems.len(), 1);
    }

    #[test]
    fn a_hostile_portfolio_name_cannot_escape_the_directory() {
        for hostile in ["../../etc/passwd", "..", "", "a/b"] {
            let cleaned = slug(hostile);
            assert!(
                !cleaned.contains('/') && !cleaned.contains('\\') && cleaned != "..",
                "{hostile:?} produced {cleaned:?}"
            );
        }
    }
}
