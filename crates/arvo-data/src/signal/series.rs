//! A signal's history in the library, and the flag that decides whether a
//! study may trade on it (#164).
//!
//! A signal is a live reading ([`super::Signal`]); a *series* is its history,
//! stored the way bars are — one file, content-hashed, with a version a
//! staleness check can compare against. That is what makes a signal research
//! material rather than a display: a rule that reads `regime.trend` can only
//! be studied if `regime.trend` has a past.
//!
//! # The causal flag is the whole point
//!
//! A series declares whether each value was computed **from data available at
//! that bar**. A study may filter or gate on a series only if it is causal,
//! and [`SignalSeries::as_filter`] is the only way to obtain one for that use.
//!
//! Arvo's own [`regime`](../../../arvo_research/regime/index.html) labels are
//! the second implementation, and they are the reason the rule exists: they
//! are computed after the fact, over a completed run's benchmark curve, and
//! the module that produces them says in its own doc comment that a backtest
//! filtered by them would be "look-ahead of the most flattering kind". They
//! are stored, they are read for the breakdown they already produce, and they
//! are refused as a filter with that reason attached.
//!
//! # What a series says about its own past
//!
//! A producer that can replay its history writes it as one: every value is
//! what it would have been at that bar. A producer that cannot starts
//! recording the day it was installed, and [`History::RecordedSince`] says
//! so — so a study over a window that begins before that date is reading a
//! series that has no opinion about it, which is [`super::Signal`]'s absent
//! value and not a gap to be filled in.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use super::{Signal, SignalName};
use crate::{csv::safe_name, BarInterval, DataError};

/// Where series live under the library root.
pub const SIGNALS_SUBDIR: &str = "signals";

/// The folder a series about no particular instrument goes in: a market-wide
/// regime, a macro reading. Not a valid instrument id, on purpose.
const MARKET_WIDE: &str = "_market";

/// One value at one instant. `None` is "not known here", as everywhere else
/// a signal is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SignalPoint {
    pub at: NaiveDateTime,
    pub value: Option<f64>,
}

/// Whether each value was computed from data available at its own bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "causality", rename_all = "snake_case")]
pub enum Causality {
    /// Every value used only what existed at that bar. A study may gate on
    /// it.
    Causal,
    /// Computed with knowledge the bar did not have — after the fact, over
    /// the whole window, or with a threshold chosen once the answer was
    /// visible. Describing a result with it is honest; selecting on it is
    /// not.
    NotCausal {
        /// Said in full, because this is what a refusal shows a person.
        because: String,
    },
}

/// How much of a past the series has, and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "history", rename_all = "snake_case")]
pub enum History {
    /// The producer replayed its own history: the series covers whatever its
    /// points cover, and each is what it would have said then.
    Replayed,
    /// The producer could not replay, so it began recording on this date.
    /// Nothing before it exists, and nothing should invent it.
    RecordedSince { from: NaiveDate },
}

/// A signal's history: what it said, when, and whether a study may act on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalSeries {
    pub name: SignalName,
    /// The instrument this describes, or empty for a market-wide series.
    pub instrument: String,
    pub interval: BarInterval,
    /// What computed it, by name and version — `arvo.regime 1`,
    /// `price-behavior-v2.1-frozen`. A series from a classifier that was
    /// retuned is a different series, and this is what says so.
    pub engine: String,
    pub causality: Causality,
    pub history: History,
    /// Oldest first.
    pub points: Vec<SignalPoint>,
}

/// A series a study asked to filter on that was not computed causally.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{name} cannot filter or gate a study: {because}")]
pub struct NotCausal {
    pub name: SignalName,
    pub because: String,
}

impl SignalSeries {
    /// The series as something a study may filter or gate on.
    ///
    /// The only way to get one. A study that wants to trade only when a
    /// signal says so goes through here, so that a series computed with
    /// knowledge the bar did not have cannot reach a backtest by any route
    /// that did not have to say no.
    ///
    /// # Errors
    ///
    /// [`NotCausal`], carrying the producer's own reason, for a series
    /// flagged [`Causality::NotCausal`].
    pub fn as_filter(&self) -> Result<&Self, NotCausal> {
        match &self.causality {
            Causality::Causal => Ok(self),
            Causality::NotCausal { because } => Err(NotCausal {
                name: self.name.clone(),
                because: because.clone(),
            }),
        }
    }

    /// What the series says at `at`, as a signal.
    ///
    /// An instant the series does not hold is absent — including one before
    /// [`History::RecordedSince`], which is the case this type exists to keep
    /// honest. It is never the last known value.
    #[must_use]
    pub fn at(&self, at: NaiveDateTime) -> Signal {
        let value = self
            .points
            .iter()
            .find(|point| point.at == at)
            .and_then(|point| point.value);
        Signal::new(self.name.clone(), at, value)
    }

    /// A content hash of everything that makes this series what it is.
    ///
    /// The same thing a bar fingerprint is (ADR-0008), for the same reason: a
    /// finding produced from a series can say which series, and a check can
    /// tell whether the one on disk is still it. Raw bit patterns rather than
    /// formatted text, so it is identical on every platform — and an absent
    /// value is tagged rather than skipped, because a series that lost a
    /// value must not hash like one that never had the point at all.
    #[must_use]
    pub fn version(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for field in [
            self.name.as_str(),
            self.instrument.as_str(),
            &self.interval.to_string(),
            self.engine.as_str(),
        ] {
            hasher.update(field.as_bytes());
            hasher.update(&[0]);
        }
        // The flags are part of the identity: the same numbers with the
        // causal flag flipped are a different series, and the difference is
        // the one that decides whether they may be traded on.
        match &self.causality {
            Causality::Causal => hasher.update(b"causal"),
            Causality::NotCausal { because } => {
                hasher.update(b"not-causal");
                hasher.update(because.as_bytes())
            }
        };
        match self.history {
            History::Replayed => hasher.update(b"replayed"),
            History::RecordedSince { from } => {
                hasher.update(b"recorded-since");
                hasher.update(from.to_string().as_bytes())
            }
        };
        for point in &self.points {
            hasher.update(&point.at.and_utc().timestamp().to_le_bytes());
            match point.value {
                Some(value) => {
                    hasher.update(&[1]);
                    hasher.update(&value.to_bits().to_le_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Signal histories on disk, beside the bars.
///
/// One file per series — `signals/SPY.RH/1day/regime.trend.json` — because a
/// series is read whole: a study wants every value over its window, and a
/// rule wants the one at its bar out of a series already loaded.
#[derive(Debug, Clone)]
pub struct SignalStore {
    root: PathBuf,
}

impl SignalStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Into::into(root),
        }
    }

    /// Writes the series, replacing whatever was held under the same name,
    /// instrument and interval. Returns the file and its version.
    ///
    /// # Errors
    ///
    /// [`DataError::UnsafeInstrument`] for an instrument that is not a usable
    /// file name, and [`DataError::Io`] for anything that could not be
    /// written.
    pub fn write(&self, series: &SignalSeries) -> Result<(PathBuf, String), DataError> {
        let path = self.path_for(&series.name, &series.instrument, series.interval)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| DataError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = serde_json::to_string_pretty(series).map_err(|err| DataError::Malformed {
            path: path.clone(),
            line: 0,
            reason: err.to_string(),
        })?;
        std::fs::write(&path, text).map_err(|source| DataError::Io {
            path: path.clone(),
            source,
        })?;
        Ok((path, series.version()))
    }

    /// The series held under this name, or `None` if there is none.
    ///
    /// # Errors
    ///
    /// [`DataError::UnsafeInstrument`], [`DataError::Io`] for a file that
    /// could not be read, and [`DataError::Malformed`] for one that is not a
    /// series this build understands.
    pub fn read(
        &self,
        name: &SignalName,
        instrument: &str,
        interval: BarInterval,
    ) -> Result<Option<SignalSeries>, DataError> {
        let path = self.path_for(name, instrument, interval)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(DataError::Io { path, source }),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| DataError::Malformed {
                path,
                line: err.line(),
                reason: err.to_string(),
            })
    }

    /// The version of the series held under this name, for a staleness check
    /// that does not want the values. `None` when there is none.
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn version(
        &self,
        name: &SignalName,
        instrument: &str,
        interval: BarInterval,
    ) -> Result<Option<String>, DataError> {
        Ok(self.read(name, instrument, interval)?.map(|series| series.version()))
    }

    /// Where a series is kept. Public so a caller can pin or show the file.
    ///
    /// # Errors
    ///
    /// [`DataError::UnsafeInstrument`] for an instrument that is not a usable
    /// file name. The name needs no such check: [`SignalName`] is validated
    /// when it is built.
    pub fn path_for(
        &self,
        name: &SignalName,
        instrument: &str,
        interval: BarInterval,
    ) -> Result<PathBuf, DataError> {
        let folder = if instrument.is_empty() {
            MARKET_WIDE
        } else {
            safe_name(instrument)?
        };
        Ok(self
            .root
            .join(SIGNALS_SUBDIR)
            .join(folder)
            .join(interval.to_string())
            .join(format!("{name}.json")))
    }

    /// Every series held, as the files say: name, instrument and interval.
    ///
    /// # Errors
    ///
    /// [`DataError::Io`] if a directory under the signals folder cannot be
    /// listed. A file that is not a readable series is skipped rather than
    /// failing the listing — one unreadable series should not hide the rest.
    pub fn held(&self) -> Result<Vec<(SignalName, String, BarInterval)>, DataError> {
        let root = self.root.join(SIGNALS_SUBDIR);
        let mut held = Vec::new();
        for instrument in read_dir(&root)? {
            let folder = instrument.file_name().to_string_lossy().into_owned();
            let instrument = if folder == MARKET_WIDE { String::new() } else { folder };
            for interval in read_dir(&instrument_path(&instrument, &root))? {
                let Ok(parsed) = interval.file_name().to_string_lossy().parse::<BarInterval>()
                else {
                    continue;
                };
                for file in read_dir(&interval.path())? {
                    let path = file.path();
                    if path.extension().is_none_or(|ext| ext != "json") {
                        continue;
                    }
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                    if let Ok(name) = SignalName::new(stem.into_owned()) {
                        held.push((name, instrument.clone(), parsed));
                    }
                }
            }
        }
        // `BarInterval` is not ordered — a listing is read as text, so sort as
        // one: name, then instrument, then how the interval spells itself.
        held.sort_by(|left, right| {
            (&left.0, &left.1, left.2.to_string()).cmp(&(&right.0, &right.1, right.2.to_string()))
        });
        Ok(held)
    }
}

/// The folder a listed instrument's series live in, given the signals root.
fn instrument_path(instrument: &str, root: &Path) -> PathBuf {
    root.join(if instrument.is_empty() { MARKET_WIDE } else { instrument })
}

/// A directory's entries, or none at all when it does not exist — a library
/// that has never stored a signal is empty, not broken.
fn read_dir(path: &Path) -> Result<Vec<std::fs::DirEntry>, DataError> {
    match std::fs::read_dir(path) {
        Ok(entries) => Ok(entries.flatten().collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(DataError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> SignalName {
        SignalName::new(name).expect("a name")
    }

    fn at(day: u32) -> NaiveDateTime {
        format!("2024-01-{day:02}T00:00:00").parse().expect("a timestamp")
    }

    fn series(causality: Causality) -> SignalSeries {
        SignalSeries {
            name: named("regime.trend"),
            instrument: "SPY.RH".to_owned(),
            interval: BarInterval::DAILY,
            engine: "arvo.regime 1".to_owned(),
            causality,
            history: History::Replayed,
            points: vec![
                SignalPoint { at: at(2), value: None },
                SignalPoint { at: at(3), value: Some(1.0) },
            ],
        }
    }

    #[test]
    fn a_series_computed_after_the_fact_cannot_filter_a_study() {
        let descriptive = series(Causality::NotCausal {
            because: "computed over the whole window after the run".to_owned(),
        });
        let refused = descriptive.as_filter().expect_err("a study may not gate on it");
        assert_eq!(refused.name, named("regime.trend"));
        assert!(
            refused.to_string().contains("after the run"),
            "the refusal carries the producer's own reason: {refused}"
        );

        // The same numbers, computed from what each bar knew, are research
        // material — the flag is the whole difference.
        assert!(series(Causality::Causal).as_filter().is_ok());
    }

    #[test]
    fn an_instant_the_series_does_not_hold_is_absent_rather_than_the_last_value() {
        let held = series(Causality::Causal);
        assert_eq!(held.at(at(3)).value, Some(1.0), "a value it holds");
        assert_eq!(held.at(at(2)).value, None, "a point it holds with no value");

        // The case that matters: a study window reaching back before the
        // series begins. Carrying 1.0 backwards would be the series
        // answering a question it was never asked.
        let before = held.at(at(1));
        assert_eq!(before.value, None);
        assert!(!before.at_least(0.0), "and it satisfies nothing");
    }

    #[test]
    fn the_version_changes_with_anything_that_makes_it_a_different_series() {
        let causal = series(Causality::Causal);
        let descriptive = series(Causality::NotCausal { because: "after the fact".to_owned() });
        assert_ne!(causal.version(), descriptive.version(), "the flag is part of the identity");

        let mut retuned = causal.clone();
        retuned.engine = "arvo.regime 2".to_owned();
        assert_ne!(causal.version(), retuned.version(), "so is what computed it");

        // An absent value is tagged, not skipped: a series that lost a value
        // must not hash like one that never had the point.
        let mut lost = causal.clone();
        lost.points[1].value = None;
        let mut dropped = causal.clone();
        dropped.points.remove(1);
        assert_ne!(lost.version(), dropped.version());
        assert_eq!(causal.version(), series(Causality::Causal).version(), "and it is stable");
    }

    #[test]
    fn a_series_survives_the_round_trip_and_is_listed() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = SignalStore::new(root.path());
        let held = series(Causality::NotCausal { because: "descriptive".to_owned() });

        assert_eq!(store.read(&held.name, &held.instrument, held.interval).expect("reads"), None, "nothing yet");
        assert!(store.held().expect("a listing").is_empty(), "and nothing listed");

        let (path, version) = store.write(&held).expect("writes");
        assert!(path.is_file());
        let read = store
            .read(&held.name, &held.instrument, held.interval)
            .expect("reads")
            .expect("it is there");
        assert_eq!(read, held, "everything about it, including why it may not be traded on");
        assert_eq!(read.version(), version);
        assert_eq!(
            store.version(&held.name, &held.instrument, held.interval).expect("a version"),
            Some(version)
        );
        assert_eq!(
            store.held().expect("a listing"),
            vec![(named("regime.trend"), "SPY.RH".to_owned(), BarInterval::DAILY)]
        );
    }

    #[test]
    fn a_market_wide_series_needs_no_instrument() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = SignalStore::new(root.path());
        let mut wide = series(Causality::Causal);
        wide.instrument = String::new();
        wide.name = named("macro.yield-curve");

        store.write(&wide).expect("writes");
        assert_eq!(
            store.held().expect("a listing"),
            vec![(named("macro.yield-curve"), String::new(), BarInterval::DAILY)]
        );
        assert_eq!(store.read(&wide.name, "", wide.interval).expect("reads"), Some(wide));
    }
}
