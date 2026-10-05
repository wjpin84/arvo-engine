//! What each fetch did, written down (ADR-0036).
//!
//! A fetch already worked out the thing that matters about a re-fetch: whether
//! the series it brought back differs from the one held by a rescaling, which
//! is what a corporate action does to every price at once, or by a revision,
//! which is a source changing its mind. It handed that to its caller, the
//! window showed it once, the scheduled refresh dropped it, and nothing was
//! kept. So when a finding went stale nobody could say why, and nothing on
//! disk recorded when a series was fetched, from where, or for what window.
//!
//! This is the record: one line per fetch, appended to `fetches.jsonl` in the
//! library. It is a log and not a store. Nothing reads it to decide what a
//! bar is; the staleness check reads it to say what happened.

use std::io::Write as _;
use std::path::Path;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agreement::Agreement;

/// The log's name, in the library's root.
pub const FILE: &str = "fetches.jsonl";

/// How what a fetch brought back compares with what was held.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Change {
    /// Nothing was held for the instrument and window: a first fetch.
    First,
    /// What was held covers a different period, so nothing could be compared.
    NoOverlap,
    /// Every bar held in the window came back the same.
    Aligned { compared: usize },
    /// Every bar moved by one factor: a re-adjustment after a corporate
    /// action. Nothing that happened has been contradicted.
    Rescaled { factor: f64, compared: usize },
    /// Individual bars came back different: the source revised history.
    Diverged { disagreeing: usize, compared: usize, worst: f64, at: NaiveDateTime },
}

impl From<Option<&Agreement>> for Change {
    fn from(revision: Option<&Agreement>) -> Self {
        match revision {
            None => Self::First,
            Some(Agreement::NoOverlap) => Self::NoOverlap,
            Some(Agreement::Aligned { compared }) => Self::Aligned { compared: *compared },
            Some(Agreement::Rescaled { factor, compared }) => Self::Rescaled { factor: *factor, compared: *compared },
            Some(Agreement::Diverged { disagreeing, compared, worst, at }) => {
                Self::Diverged { disagreeing: *disagreeing, compared: *compared, worst: *worst, at: *at }
            }
        }
    }
}

/// One fetch into the library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fetch {
    pub at: DateTime<Utc>,
    /// The source's id, e.g. `yahoo`.
    pub source: String,
    /// `SYMBOL.VENUE`, as the library names the series.
    pub instrument: String,
    /// As the library spells it, e.g. `1day`.
    pub interval: String,
    /// The window that was asked for.
    pub asked_from: NaiveDate,
    pub asked_to: NaiveDate,
    /// What came back.
    pub bars: usize,
    pub first: Option<NaiveDateTime>,
    pub last: Option<NaiveDateTime>,
    /// Bars the vendor synthesised to fill a gap, which were dropped.
    pub interpolated: usize,
    /// The whole series' fingerprint before the write, when one was held, and
    /// after it.
    pub before: Option<String>,
    pub after: Option<String>,
    pub change: Change,
    /// What the quality check said of what arrived.
    pub faults: usize,
    pub suspects: usize,
}

/// Appends one line to the log under `root`, the library.
///
/// # Errors
///
/// The file could not be opened or written. The fetch itself has already
/// succeeded by then, so a caller logs this and carries on.
pub fn append(root: &Path, fetch: &Fetch) -> std::io::Result<()> {
    let line = serde_json::to_string(fetch).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(root.join(FILE))?;
    writeln!(file, "{line}")
}

/// Every fetch on record, oldest first. A line that cannot be read is left
/// out: one bad line is not a reason to forget the rest.
#[must_use]
pub fn read(root: &Path) -> Vec<Fetch> {
    std::fs::read_to_string(root.join(FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(instrument: &str, change: Change) -> Fetch {
        Fetch {
            at: DateTime::parse_from_rfc3339("2026-10-05T13:00:00Z").expect("valid").with_timezone(&Utc),
            source: "yahoo".to_owned(),
            instrument: instrument.to_owned(),
            interval: "1day".to_owned(),
            asked_from: NaiveDate::from_ymd_opt(2016, 10, 5).expect("valid"),
            asked_to: NaiveDate::from_ymd_opt(2026, 10, 5).expect("valid"),
            bars: 2513,
            first: None,
            last: None,
            interpolated: 0,
            before: Some("a".repeat(64)),
            after: Some("b".repeat(64)),
            change,
            faults: 0,
            suspects: 1,
        }
    }

    #[test]
    fn the_log_is_appended_to_and_read_back_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read(dir.path()).is_empty(), "no log is no fetches");

        let first = fetch("AAPL.YF", Change::First);
        let split = fetch("AAPL.YF", Change::Rescaled { factor: 0.25, compared: 2512 });
        append(dir.path(), &first).expect("appended");
        append(dir.path(), &split).expect("appended");
        assert_eq!(read(dir.path()), vec![first, split]);

        // One line a person can read, with the class named.
        let text = std::fs::read_to_string(dir.path().join(FILE)).expect("read");
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().nth(1).expect("a line").contains(r#""change":{"class":"rescaled","factor":0.25,"compared":2512}"#));

        // A torn line does not cost the others.
        std::fs::write(dir.path().join(FILE), format!("{text}{{\"at\":\"2026")).expect("write");
        assert_eq!(read(dir.path()).len(), 2);
    }
}
