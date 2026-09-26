//! Tests for [`super`].
//!
//! Split out of `memory.rs` on 2026-09-26 — it was 402 lines of
//! tests against 817 of code, which is the shape
//! `advice/` and `replay/` already moved out for.

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

    /// A finding recorded before builds were stamped reads as unknown,
    /// and a stamped one reads back what stamped it, on the summary too.
    #[test]
    fn provenance_survives_the_store_and_an_unstamped_finding_reads_as_unknown() {
        let (_dir, store) = store();
        let stamped = record("AAPL.NASDAQ", 1_700_000_000).with_provenance(Provenance {
            code_commit: "abc123def456".to_owned(),
            ruleset: Some(RulesetRef { name: "my_cross".to_owned(), hash: "h1".to_owned() }),
        });
        store.save(&stamped).expect("saves");
        let opened = store.open(&stamped.id).expect("opens");
        assert_eq!(opened.provenance, stamped.provenance);
        let (summaries, _) = store.summaries().expect("lists");
        assert_eq!(summaries[0].code_commit, "abc123def456");
        assert_eq!(summaries[0].ruleset_hash.as_deref(), Some("h1"));
        assert_eq!(summaries[0].strategy, "my_cross", "the name the person chose, not the rule under it");

        let mut old: serde_json::Value =
            serde_json::to_value(record("MSFT.NASDAQ", 1_700_000_001)).expect("encodes");
        old.as_object_mut().expect("object").remove("provenance");
        let path = store.root.join("1700000001-MSFT.NASDAQ.json");
        std::fs::write(&path, serde_json::to_vec(&old).expect("json")).expect("writes");
        let loaded = read_record(&path).expect("reads without the field");
        assert_eq!(loaded.provenance, Provenance::default());
        assert_eq!(loaded.summary().code_commit, "");
        assert_eq!(loaded.summary().strategy, "sma_cross", "unstamped: the rule is all that is known");
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
        underlying: None,
        window: DateRange::new(day(1), day(31)).expect("ordered"),
        interval: arvo_data::BarInterval::DAILY,
        dataset: DatasetRef {
            id: instrument.to_owned(),
            version: dataset_version.to_owned(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: StrategySpec {
            rule: None,
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
    
        under_conservative_costs: None,
    
        conservative: None,
    }))
}

fn at(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 4, 12, 0, second)
        .single()
        .expect("valid instant")
}

/// A file kept with a finding (#157): stored once by content, listed on
/// the record, back after a reopen, and never reachable by a hash that
/// is not one.
#[test]
fn an_attachment_is_stored_once_by_content_and_listed_on_the_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = EvidenceStore::new(dir.path());
    let stored = StoredRecord::new(study("AAPL.NASDAQ", "hash-a"), at(1));
    store.save(&stored).expect("saved");

    let kept = store.attach(&stored.id, "report.md", "text/markdown", b"# one").expect("kept");
    assert_eq!(kept.len(), 1);
    let hash = kept[0].hash.clone();
    assert_eq!(hash.len(), 64);
    assert!(store.attachment_path(&hash).expect("a hash").is_file(), "the bytes, under their hash");

    let same = store.attach(&stored.id, "report.md", "text/markdown", b"# one").expect("kept");
    assert_eq!(same.len(), 1, "same name, same bytes: nothing new");
    let renamed = store.attach(&stored.id, "copy.md", "text/markdown", b"# one").expect("kept");
    assert_eq!(renamed.len(), 2, "a second name for the same bytes is a second entry");
    assert_eq!(renamed[1].hash, hash, "and the same one file");

    let reopened = store.open(&stored.id).expect("opens");
    assert_eq!(reopened.attachments.len(), 2);
    assert_eq!(reopened.summary().attachments, 2);
    assert!(store.attachment_path("../engine.json").is_none(), "a path is not a hash");
    assert!(store.attach("nope", "x", "text/plain", b"x").is_err(), "no such finding");
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
