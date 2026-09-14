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

use crate::{
    agent_search::AgentSearch,
    family::{FamilyEvidence, Selection},
    panel::PanelEvidence,
    walk_forward::WalkForwardEvidence,
    HypothesisId, Verdict,
};

/// One finding, of whichever kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Study(Box<FamilyEvidence>),
    Panel(Box<PanelEvidence>),
    WalkForward(Box<WalkForwardEvidence>),
}

impl Record {
    /// Which sort of finding this is, as the stored id says it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Study(_) => "study",
            Self::Panel(_) => "panel",
            Self::WalkForward(_) => "walk-forward",
        }
    }

    /// What the finding is about, for a history listing.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::Study(evidence) => evidence.selected.instrument.clone(),
            Self::Panel(evidence) => {
                format!("Panel of {} instruments", evidence.pooled.instruments)
            }
            Self::WalkForward(evidence) => {
                format!("{} walk-forward", evidence.template.instrument)
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
            Self::WalkForward(evidence) => &evidence.template.dataset.version,
        }
    }

    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match self {
            Self::Study(evidence) => evidence.verdict,
            Self::Panel(evidence) => evidence.verdict,
            Self::WalkForward(evidence) => evidence.verdict,
        }
    }

    #[must_use]
    pub fn hypothesis(&self) -> &HypothesisId {
        match self {
            Self::Study(evidence) => &evidence.hypothesis,
            Self::Panel(evidence) => &evidence.hypothesis,
            Self::WalkForward(evidence) => &evidence.hypothesis,
        }
    }

    /// Every search over configurations that went into this finding: one for
    /// a study or a panel, one per fold for a walk-forward.
    #[must_use]
    pub fn selections(&self) -> Vec<&Selection> {
        match self {
            Self::Study(evidence) => vec![&evidence.selection],
            Self::Panel(evidence) => vec![&evidence.selection],
            Self::WalkForward(evidence) => {
                evidence.folds.iter().map(|fold| &fold.selection).collect()
            }
        }
    }

    /// Marks the finding `NotSupported`, saying why. Never upgrades.
    pub(crate) fn refuse(&mut self, reason: String) {
        let (verdict, reasons) = match self {
            Self::Study(evidence) => (&mut evidence.verdict, &mut evidence.reasons),
            Self::Panel(evidence) => (&mut evidence.verdict, &mut evidence.reasons),
            Self::WalkForward(evidence) => (&mut evidence.verdict, &mut evidence.reasons),
        };
        *verdict = Verdict::NotSupported;
        reasons.push(reason);
    }
}

/// Who ran a finding.
///
/// Recorded on every finding from the first one an agent could write, because
/// it cannot be recovered afterwards: deflating an agent's finding means
/// counting everything *that agent* ran, and a store that did not say who ran
/// what makes every stored agent finding uninterpretable (#25).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Author {
    /// Someone at the workbench. `default` because every finding recorded
    /// before this existed was one.
    #[default]
    Person,
    /// An agent, with the bar its whole search held this finding to. The bar
    /// is kept rather than recomputed: the history it was measured over
    /// changes as findings are added and deleted.
    Agent { id: String, search: AgentSearch },
}

impl Author {
    /// The agent's id, or `None` for a person.
    #[must_use]
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::Person => None,
            Self::Agent { id, .. } => Some(id),
        }
    }
}

/// The shape this build writes.
///
/// Bumped whenever a stored finding stops being readable by the code that
/// wrote the previous one. It is not a migration system — it is the thing that
/// lets a failure say *which* version wrote the file, instead of
/// `missing field \`at\` at line 16903`, which is what four real findings said
/// after `EquityPoint.date` was renamed and nothing recorded that a rename had
/// happened.
pub const SCHEMA: u32 = 1;

/// A record plus when it was taken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub id: String,
    pub recorded_at: DateTime<Utc>,
    /// Which build's format this is. `0` for anything written before the
    /// version existed — which is exactly the set of findings that cannot be
    /// read any more, so it is a useful thing to be able to say.
    #[serde(default)]
    pub schema: u32,
    /// Who ran it. `default`, so findings written before this read as a
    /// person's — which they all were.
    #[serde(default)]
    pub author: Author,
    pub record: Record,
}

/// What a finding is, without reading the finding.
///
/// Everything the history list and the staleness check need, and nothing
/// else. It exists because the alternative measured badly: a stored finding is
/// around 400 KB — curves, ledgers, per-fold evidence and the search surface —
/// and listing them all parsed every byte of every one to render a column of
/// names. Five findings cost 1.7 MB of parsing; five hundred would cost
/// seconds, on every render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub recorded_at: DateTime<Utc>,
    pub kind: String,
    pub subject: String,
    pub verdict: Verdict,
    pub hypothesis: HypothesisId,
    pub dataset_version: String,
    /// The instrument and resolution the finding was produced at, so
    /// staleness can be checked without loading it. `None` for a panel, whose
    /// dataset identity is every member's hash combined and has to be
    /// recomputed the same way it was produced.
    pub instrument: Option<String>,
    pub interval: Option<arvo_data::BarInterval>,
    /// Instruments a book held alongside [`Self::instrument`]; empty for
    /// anything else.
    ///
    /// A book's dataset version is every member's hash combined, so checking
    /// it against the head instrument alone called every book stale forever —
    /// and refused to replay any of them.
    ///
    /// Required rather than `default`, on purpose. Summaries are only ever
    /// persisted in the index, which is a cache: an index written before this
    /// field fails to parse, is treated as empty, and is rebuilt from the
    /// findings. Defaulting would have kept every cached book wrong.
    pub alongside: Vec<String>,
    /// The agent that ran it, or `None` for a person.
    ///
    /// On the summary so History can mark an agent's finding without opening
    /// it: a number an agent produced must not read, in a list, like one a
    /// person produced (#33). Required, like `alongside`, so an index written
    /// before it is rebuilt rather than read as "all by people".
    pub agent: Option<String>,
}

/// A finding that could not be read, and why.
///
/// Kept as a value rather than a log line. Four findings were lost to a field
/// rename and the only trace was a warning nobody had reason to look at; a
/// research store that quietly forgets things is worse than one that says it
/// has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unreadable {
    pub id: String,
    pub reason: String,
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
            schema: SCHEMA,
            author: Author::Person,
            record,
        }
    }

    /// Builds an agent's record, judged against everything that agent has
    /// already recorded.
    ///
    /// The only way to attribute a finding to an agent, so there is no path
    /// that stores one without deflating it. `history` is the store as it
    /// stands — [`EvidenceStore::load`] — and findings by anyone else in it
    /// are ignored.
    // ponytail: history is every full finding (~400 KB each); carry trial
    // counts and scores in `Summary` once an agent's store reaches hundreds.
    #[must_use]
    pub fn by_agent(
        mut record: Record,
        agent: &str,
        history: &[Self],
        recorded_at: DateTime<Utc>,
    ) -> Self {
        let search = crate::agent_search::deflate_for_agent(&mut record, agent, history);
        Self {
            author: Author::Agent {
                id: agent.to_owned(),
                search,
            },
            ..Self::new(record, recorded_at)
        }
    }

    /// What the history list needs, without the finding itself.
    #[must_use]
    pub fn summary(&self) -> Summary {
        let (instrument, interval, alongside) = match &self.record {
            Record::Study(evidence) => (
                Some(evidence.selected.instrument.clone()),
                Some(evidence.selected.interval),
                evidence.selected.alongside.clone(),
            ),
            Record::WalkForward(evidence) => (
                Some(evidence.template.instrument.clone()),
                Some(evidence.template.interval),
                evidence.template.alongside.clone(),
            ),
            Record::Panel(_) => (None, None, Vec::new()),
        };
        Summary {
            id: self.id.clone(),
            recorded_at: self.recorded_at,
            kind: self.record.kind().to_owned(),
            subject: self.record.subject(),
            verdict: self.record.verdict(),
            hypothesis: self.record.hypothesis().clone(),
            dataset_version: self.record.dataset_version().to_owned(),
            instrument,
            interval,
            alongside,
            agent: self.author.agent().map(ToOwned::to_owned),
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
            // The summary cache lives beside the findings and is not one.
            .filter(|path| path.file_name().is_some_and(|name| name != INDEX))
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

/// Where the summaries are cached, inside the store.
const INDEX: &str = "index.json";

impl EvidenceStore {
    /// Every finding's headline, newest first, with the ones that could not be
    /// read.
    ///
    /// # The index is a cache, and is treated as one
    ///
    /// The directory is the truth. This lists it — which reads no file
    /// contents — takes summaries from the cache for ids it already knows,
    /// parses only the ids it does not, and rewrites the cache if anything
    /// changed. An id in the cache that is no longer on disk is dropped.
    ///
    /// That shape has no staleness class to reason about: delete the index,
    /// hand-edit it, copy findings in from another machine, and the next call
    /// is correct. The alternative — an index maintained on write and trusted
    /// on read — is one missed write away from a finding that exists and
    /// cannot be seen.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Read`] only if the directory cannot be listed.
    /// A finding that cannot be parsed is reported, not fatal.
    pub fn summaries(&self) -> Result<(Vec<Summary>, Vec<Unreadable>), MemoryError> {
        let paths = self.finding_paths()?;

        let cached: std::collections::HashMap<String, Summary> = self
            .read_index()
            .into_iter()
            .map(|summary| (summary.id.clone(), summary))
            .collect();

        let mut summaries = Vec::with_capacity(paths.len());
        let mut unreadable = Vec::new();
        let mut rebuilt = paths.len() != cached.len();

        for path in paths {
            let id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();

            if let Some(summary) = cached.get(&id) {
                summaries.push(summary.clone());
                continue;
            }
            rebuilt = true;
            match read_record(&path) {
                Ok(record) => summaries.push(record.summary()),
                Err(reason) => unreadable.push(Unreadable { id, reason }),
            }
        }

        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.recorded_at));
        if rebuilt {
            self.write_index(&summaries);
        }
        Ok((summaries, unreadable))
    }

    /// Reads one finding by id.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Read`] if the file is missing or unreadable.
    pub fn open(&self, id: &str) -> Result<StoredRecord, MemoryError> {
        let path = self.root.join(format!("{}.json", slug(id)));
        read_record(&path).map_err(|reason| MemoryError::Read {
            path: path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, reason),
        })
    }

    fn finding_paths(&self) -> Result<Vec<PathBuf>, MemoryError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(MemoryError::Read {
                    path: self.root.clone(),
                    source,
                })
            }
        };
        Ok(entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            // The cache lives in the same directory and is not a finding.
            .filter(|path| path.file_name().is_some_and(|name| name != INDEX))
            .collect())
    }

    fn read_index(&self) -> Vec<Summary> {
        std::fs::read_to_string(self.root.join(INDEX))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Best effort. A cache that cannot be written costs the next listing some
    /// parsing and nothing else, and failing a read because a *cache* could
    /// not be updated would be the wrong trade entirely.
    fn write_index(&self, summaries: &[Summary]) {
        if let Ok(text) = serde_json::to_string(summaries) {
            let _ = std::fs::write(self.root.join(INDEX), text);
        }
    }
}

fn read_record(path: &Path) -> Result<StoredRecord, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;

    // Read the version before the record. A finding written by a newer build
    // fails on whichever field changed first, and "missing field `at`" is a
    // description of a symptom rather than of the problem.
    let schema = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("schema").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    // The path stays in every message. A reason without one is a reason
    // nobody can act on when the store holds hundreds of files.
    let where_ = path.display();
    if schema > u64::from(SCHEMA) {
        return Err(format!(
            "{where_}: written by a newer version of Arvo (format {schema}, this build reads {SCHEMA})"
        ));
    }

    serde_json::from_str(&text).map_err(|err| {
        if schema < u64::from(SCHEMA) {
            format!(
                "{where_}: written by an older version of Arvo (format {schema}) and cannot be read: {err}"
            )
        } else {
            format!("{where_}: {err}")
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The index is a cache over the directory, and every test here is about
    /// that being true rather than nearly true.
    mod index {
        use super::*;

        fn store() -> (tempfile::TempDir, EvidenceStore) {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = EvidenceStore::new(dir.path());
            (dir, store)
        }

        fn record(subject: &str, at: i64) -> StoredRecord {
            let mut stored = StoredRecord::new(
                study(subject, "v1"),
                DateTime::from_timestamp(at, 0).expect("valid"),
            );
            stored.id = format!("{at}-{subject}");
            stored
        }

        #[test]
        fn a_summary_says_what_the_list_needs_without_the_finding() {
            let (_dir, store) = store();
            store.save(&record("AAPL.NASDAQ", 1_700_000_000)).expect("saves");

            let (summaries, unreadable) = store.summaries().expect("lists");
            assert!(unreadable.is_empty());
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].kind, "study");
            assert_eq!(summaries[0].subject, "AAPL.NASDAQ");
            assert_eq!(summaries[0].instrument.as_deref(), Some("AAPL.NASDAQ"));
        }

        #[test]
        fn a_finding_added_behind_the_index_still_appears() {
            // Copied in from another machine, restored from a backup, written
            // by a build that did not know about the cache. The directory is
            // the truth and the cache catches up.
            let (_dir, store) = store();
            store.save(&record("AAPL.NASDAQ", 1_700_000_000)).expect("saves");
            store.summaries().expect("builds the index");

            store.save(&record("MSFT.NASDAQ", 1_700_000_100)).expect("saves");
            let (summaries, _) = store.summaries().expect("lists");
            assert_eq!(summaries.len(), 2);
        }

        #[test]
        fn a_finding_deleted_behind_the_index_stops_appearing() {
            let (dir, store) = store();
            let stored = record("AAPL.NASDAQ", 1_700_000_000);
            let path = store.save(&stored).expect("saves");
            store.summaries().expect("builds the index");

            std::fs::remove_file(&path).expect("removes");
            let (summaries, _) = store.summaries().expect("lists");
            assert!(summaries.is_empty(), "{:?}", dir.path());
        }

        #[test]
        fn a_corrupt_index_costs_parsing_and_nothing_else() {
            // It is a cache. Losing it must never lose a finding.
            let (dir, store) = store();
            store.save(&record("AAPL.NASDAQ", 1_700_000_000)).expect("saves");
            std::fs::write(dir.path().join(INDEX), "not json").expect("write");

            let (summaries, unreadable) = store.summaries().expect("lists");
            assert_eq!(summaries.len(), 1);
            assert!(unreadable.is_empty());
        }

        #[test]
        fn an_index_from_before_books_were_summarised_is_rebuilt_not_trusted() {
            // Cached summaries without `alongside` would keep every book
            // checked against its head instrument alone.
            let (dir, store) = store();
            let mut stored = record("AAPL.NASDAQ", 1_700_000_000);
            if let Record::Study(found) = &mut stored.record {
                found.selected.alongside = vec!["MSFT.NASDAQ".to_owned()];
            }
            store.save(&stored).expect("saves");
            store.summaries().expect("builds the index");

            let path = dir.path().join(INDEX);
            let mut old: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("index")).expect("json");
            for summary in old.as_array_mut().expect("a list") {
                summary.as_object_mut().expect("object").remove("alongside");
            }
            std::fs::write(&path, old.to_string()).expect("write");

            let (summaries, _) = store.summaries().expect("lists");
            assert_eq!(summaries[0].alongside, vec!["MSFT.NASDAQ".to_owned()]);
        }

        #[test]
        fn the_index_is_not_listed_as_a_finding() {
            let (_dir, store) = store();
            store.save(&record("AAPL.NASDAQ", 1_700_000_000)).expect("saves");
            store.summaries().expect("writes the index");
            let (summaries, unreadable) = store.summaries().expect("lists again");
            assert_eq!(summaries.len(), 1);
            assert!(unreadable.is_empty(), "{unreadable:?}");
        }

        #[test]
        fn an_unreadable_finding_is_reported_rather_than_skipped() {
            // Four real findings were lost to a field rename and the only
            // trace was a log line. A store that quietly forgets is worse than
            // one that says it has.
            let (dir, store) = store();
            store.save(&record("AAPL.NASDAQ", 1_700_000_000)).expect("saves");
            std::fs::write(dir.path().join("broken.json"), r#"{"id":"broken"}"#).expect("write");

            let (summaries, unreadable) = store.summaries().expect("lists");
            assert_eq!(summaries.len(), 1, "the good one still lists");
            assert_eq!(unreadable.len(), 1);
            assert_eq!(unreadable[0].id, "broken");
        }

        #[test]
        fn a_finding_from_a_newer_build_says_so_rather_than_naming_a_field() {
            // "missing field `at` at line 16903" is a description of a symptom.
            let (dir, store) = store();
            std::fs::create_dir_all(dir.path()).expect("dir");
            std::fs::write(
                dir.path().join("future.json"),
                format!(r#"{{"id":"future","schema":{},"recorded_at":"2026-01-01T00:00:00Z"}}"#, SCHEMA + 9),
            )
            .expect("write");

            let (_, unreadable) = store.summaries().expect("lists");
            assert_eq!(unreadable.len(), 1);
            assert!(
                unreadable[0].reason.contains("newer version"),
                "{}",
                unreadable[0].reason
            );
        }

        #[test]
        fn a_finding_written_before_the_version_existed_says_which_format_it_is() {
            let (dir, store) = store();
            std::fs::create_dir_all(dir.path()).expect("dir");
            // Schema 0: no version field, and a shape this build cannot read.
            std::fs::write(dir.path().join("ancient.json"), r#"{"id":"ancient"}"#).expect("write");

            let (_, unreadable) = store.summaries().expect("lists");
            assert!(
                unreadable[0].reason.contains("older version"),
                "{}",
                unreadable[0].reason
            );
        }

        #[test]
        fn a_saved_record_carries_the_format_that_wrote_it() {
            assert_eq!(
                StoredRecord::new(study("AAPL.NASDAQ", "v1"), Utc::now()).schema,
                SCHEMA
            );
        }

        #[test]
        fn one_finding_can_be_opened_without_reading_the_rest() {
            let (_dir, store) = store();
            let stored = record("AAPL.NASDAQ", 1_700_000_000);
            store.save(&stored).expect("saves");
            assert_eq!(store.open(&stored.id).expect("opens").id, stored.id);
        }
    }
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
            psr: None,
            trades: 40,
        }
    }

    fn experiment(instrument: &str, dataset_version: &str) -> Experiment {
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
        Experiment {
            id: ExperimentId::from("e-1"),
            hypothesis: HypothesisId::from("h-1"),
            instrument: instrument.to_owned(),
            alongside: Vec::new(),
            window: DateRange::new(day(1), day(31)).expect("ordered"),
            interval: arvo_data::BarInterval::DAILY,
            dataset: DatasetRef {
                id: instrument.to_owned(),
                version: dataset_version.to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
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

    pub(crate) fn study(instrument: &str, dataset_version: &str) -> Record {
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
                prior_trials: 0,
                scored: Vec::new(),
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
