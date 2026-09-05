//! Evidence that outlives the session that produced it.
//!
//! A study costs eleven backtests and a panel costs dozens. Holding the
//! results only in memory means every question has to be re-answered from
//! scratch, and — more importantly — it means the platform cannot accumulate
//! anything. The loop this crate exists to serve ends:
//!
//! ```text
//! … → Evidence → Research Memory → AI Research Agent
//! ```
//!
//! This is the Research Memory step, and it is the last one before an agent
//! has anything to reason over.
//!
//! # What is stored
//!
//! The **domain record**, not the display projection. A [`FamilyEvidence`] or
//! [`PanelEvidence`] carries the whole `Experiment` — window, dataset hash,
//! parameters, cost model, seed — so a stored result stays reproducible
//! without the code that rendered it. Storing a flattened view would save a
//! picture of a finding and lose the finding.
//!
//! # What is deliberately not here
//!
//! No `StorageProvider` trait. There is one backend, local files, and the
//! abstraction is earned by a second one or by artifacts big enough to need
//! content addressing — neither of which exists. `std::fs` is the whole
//! implementation.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{family::FamilyEvidence, panel::PanelEvidence, HypothesisId, Verdict};

/// One finding, of whichever kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Study(Box<FamilyEvidence>),
    Panel(Box<PanelEvidence>),
}

impl Record {
    /// What the finding is about, for a history listing.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::Study(evidence) => evidence.selected.instrument.clone(),
            Self::Panel(evidence) => {
                format!("Panel of {} instruments", evidence.pooled.instruments)
            }
        }
    }

    /// The dataset this finding was produced from. Compared against the data
    /// on disk to decide whether a stored result still describes reality.
    #[must_use]
    pub fn dataset_version(&self) -> &str {
        match self {
            Self::Study(evidence) => &evidence.selected.dataset.version,
            Self::Panel(evidence) => &evidence.dataset.version,
        }
    }

    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match self {
            Self::Study(evidence) => evidence.verdict,
            Self::Panel(evidence) => evidence.verdict,
        }
    }

    #[must_use]
    pub fn hypothesis(&self) -> &HypothesisId {
        match self {
            Self::Study(evidence) => &evidence.hypothesis,
            Self::Panel(evidence) => &evidence.hypothesis,
        }
    }
}

/// A record plus when it was taken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub id: String,
    pub recorded_at: DateTime<Utc>,
    pub record: Record,
}

impl StoredRecord {
    /// Builds a record with an id derived from the time and subject.
    ///
    /// The timestamp leads so a directory listing sorts chronologically
    /// without reading a single file.
    ///
    /// `recorded_at` is a parameter rather than `Utc::now()` so this stays
    /// testable — a constructor that reaches for the clock cannot be asserted
    /// against.
    #[must_use]
    pub fn new(record: Record, recorded_at: DateTime<Utc>) -> Self {
        let id = format!(
            "{}-{}",
            recorded_at.format("%Y%m%dT%H%M%S%3f"),
            slug(&record.subject())
        );
        Self {
            id,
            recorded_at,
            record,
        }
    }
}

/// Reduces a subject to something safe to put in a file name.
///
/// Instrument names reach this from config and data files, so it is also a
/// trust boundary: anything outside the allowed set becomes `-`, which cannot
/// traverse a directory or name a device.
fn slug(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(60)
        .collect();
    // Dots are allowed because instrument names are SYMBOL.VENUE, which means
    // ".." survives the character filter intact. Anything that is only
    // punctuation gets a real name instead.
    if cleaned.trim_matches(|c| c == '-' || c == '.').is_empty() {
        "record".to_owned()
    } else {
        cleaned
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
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
    #[error("encoding a record")]
    Encode(#[source] serde_json::Error),
}

/// Everything a load found, including what it could not read.
///
/// Unreadable records are reported rather than skipped. A store that quietly
/// drops what it cannot parse looks identical to an empty one, and this
/// codebase has already been bitten by a silence that looked like absence.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Newest first.
    pub records: Vec<StoredRecord>,
    pub problems: Vec<String>,
}

/// Findings on disk, one JSON file each.
///
/// A file per record rather than one growing document: writes never rewrite
/// existing findings, a corrupt file costs one result instead of all of them,
/// and the directory is greppable by a human with no tooling.
#[derive(Debug, Clone)]
pub struct EvidenceStore {
    root: PathBuf,
}

impl EvidenceStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Writes one record, returning where it landed.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the directory cannot be created, the record
    /// cannot be encoded, or the file cannot be written.
    pub fn save(&self, record: &StoredRecord) -> Result<PathBuf, MemoryError> {
        std::fs::create_dir_all(&self.root).map_err(|source| MemoryError::Write {
            path: self.root.clone(),
            source,
        })?;

        let path = self.root.join(format!("{}.json", slug(&record.id)));
        let encoded = serde_json::to_vec_pretty(record).map_err(MemoryError::Encode)?;
        std::fs::write(&path, encoded).map_err(|source| MemoryError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Reads every record, newest first.
    ///
    /// A missing directory is an empty store, not an error — that is the
    /// ordinary state before anything has been run.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Read`] only if the directory itself cannot be
    /// listed. Individual unreadable files land in [`Loaded::problems`].
    pub fn load(&self) -> Result<Loaded, MemoryError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::default()),
            Err(source) => {
                return Err(MemoryError::Read {
                    path: self.root.clone(),
                    source,
                })
            }
        };

        let mut loaded = Loaded::default();
        for path in entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        {
            match read_record(&path) {
                Ok(record) => loaded.records.push(record),
                Err(problem) => loaded.problems.push(problem),
            }
        }

        // Newest first: `Reverse` rather than a flipped comparator, which is
        // what clippy wants and is the clearer statement anyway.
        loaded
            .records
            .sort_by_key(|record| std::cmp::Reverse(record.recorded_at));
        Ok(loaded)
    }
}

fn read_record(path: &Path) -> Result<StoredRecord, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::{Evaluation, EvaluationCriteria, Evidence};
    use crate::family::Selection;
    use crate::{
        CostModel, DatasetRef, DateRange, Experiment, ExperimentId, Metrics, StrategySpec,
    };
    use chrono::TimeZone;
    use std::collections::BTreeMap;

    fn metrics() -> Metrics {
        Metrics {
            total_return: 0.1,
            cagr: 0.05,
            max_drawdown: 0.02,
            volatility: 0.1,
            sharpe: Some(1.0),
            sortino: Some(1.2),
            calmar: Some(0.9),
            trades: 40,
        }
    }

    fn experiment(instrument: &str, dataset_version: &str) -> Experiment {
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
        Experiment {
            id: ExperimentId::from("e-1"),
            hypothesis: HypothesisId::from("h-1"),
            instrument: instrument.to_owned(),
            window: DateRange::new(day(1), day(31)).expect("ordered"),
            interval: arvo_data::BarInterval::DAILY,
            dataset: DatasetRef {
                id: instrument.to_owned(),
                version: dataset_version.to_owned(),
            },
            strategy: StrategySpec {
                name: "sma_cross".to_owned(),
                params: BTreeMap::new(),
            },
            costs: CostModel::proportional(1.0, 0.0),
            risk: crate::RiskModel::default(),
            starting_cash: 100_000.0,
            seed: 1,
        }
    }

    fn study(instrument: &str, dataset_version: &str) -> Record {
        let selected = experiment(instrument, dataset_version);
        let window = selected.window;
        Record::Study(Box::new(FamilyEvidence {
            hypothesis: HypothesisId::from("h-1"),
            in_sample: window,
            out_of_sample: window,
            selection: Selection {
                trials: 9,
                best_sharpe: 1.0,
                expected_best_under_null: Some(0.5),
                survived_deflation: true,
            },
            out_of_sample_evidence: Evidence {
                hypothesis: HypothesisId::from("h-1"),
                experiment: selected.clone(),
                benchmark: ExperimentId::from("e-1-benchmark"),
                engine: "test 0".to_owned(),
                criteria: EvaluationCriteria::default(),
                evaluation: Evaluation::new(
                    metrics(),
                    metrics(),
                    Vec::new(),
                    Vec::new(),
                    &EvaluationCriteria::default(),
                ),
            },
            selected,
            failures: Vec::new(),
            verdict: Verdict::NotSupported,
            reasons: vec!["because".to_owned()],
        }))
    }

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 4, 12, 0, second)
            .single()
            .expect("valid instant")
    }

    #[test]
    fn a_saved_record_survives_a_round_trip_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(dir.path());
        let original = StoredRecord::new(study("AAPL.NASDAQ", "hash-a"), at(1));

        store.save(&original).expect("should write");
        let loaded = store.load().expect("should read");

        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(
            loaded.records[0], original,
            "the whole experiment must survive, not a rendering of it"
        );
        assert_eq!(loaded.records[0].record.dataset_version(), "hash-a");
    }

    #[test]
    fn records_come_back_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(dir.path());
        for (second, instrument) in [(1, "A.SIM"), (3, "C.SIM"), (2, "B.SIM")] {
            store
                .save(&StoredRecord::new(study(instrument, "hash"), at(second)))
                .expect("should write");
        }

        let subjects: Vec<String> = store
            .load()
            .expect("should read")
            .records
            .iter()
            .map(|record| record.record.subject())
            .collect();
        assert_eq!(subjects, vec!["C.SIM", "B.SIM", "A.SIM"]);
    }

    #[test]
    fn an_unreadable_record_is_reported_rather_than_silently_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(dir.path());
        store
            .save(&StoredRecord::new(study("A.SIM", "hash"), at(1)))
            .expect("should write");
        std::fs::write(dir.path().join("broken.json"), b"{ not json")
            .expect("fixture should write");

        let loaded = store.load().expect("should read");
        assert_eq!(loaded.records.len(), 1, "the good record still loads");
        assert_eq!(loaded.problems.len(), 1, "and the bad one is named");
        assert!(
            loaded.problems[0].contains("broken.json"),
            "{:?}",
            loaded.problems
        );
    }

    #[test]
    fn a_store_that_has_never_been_written_is_empty_not_broken() {
        let store = EvidenceStore::new("/no/such/directory/anywhere");
        let loaded = store.load().expect("absence is not failure");
        assert!(loaded.records.is_empty());
        assert!(loaded.problems.is_empty());
    }

    #[test]
    fn a_subject_can_never_escape_the_store_directory() {
        for hostile in ["../../etc/passwd", "a/b", "..", ""] {
            let cleaned = slug(hostile);
            assert!(
                !cleaned.contains('/') && !cleaned.contains('\\') && cleaned != "..",
                "{hostile:?} produced {cleaned:?}"
            );
        }
    }
}
