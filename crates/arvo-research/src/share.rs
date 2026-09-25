//! An experiment as a file someone else can run: a question, and the search
//! that produced it ([ADR-0014]).
//!
//! # Three rules, in the order they are expensive to retrofit
//!
//! 1. **Configuration, not logic.** A file names a strategy; it does not
//!    define one. [`import`] against a name this build does not have fails,
//!    naming the ones it does.
//! 2. **A shared finding carries its search.** Someone who ran fifty
//!    configurations and shared the survivor has handed over the maximum of
//!    fifty draws. [`Search::trials`] travels, and
//!    [`SharedExperiment::family`] puts it into
//!    [`ExperimentFamily::prior_trials`], so the recipient's bar counts it.
//! 3. **No result travels.** There is no field here for a dataset hash, a
//!    curve, a metric or a verdict. The recipient's bars will not hash the
//!    same as the exporter's, so a result could only ever be believed, never
//!    replayed — and the type makes that impossible rather than discouraged.
//!
//! # Known limit
//!
//! Rule 2 has no enforcement. A file claiming one trial when fifty ran is
//! indistinguishable from an honest one. This is a format for people who want
//! the answer to be right, not a proof system.
//!
//! [ADR-0014]: https://github.com/wjpin84/arvo-desktop/blob/master/https://github.com/wjpin84/arvo-adrs/blob/main/0014-a-shared-experiment-carries-its-search.md

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::evaluation::EvaluationCriteria;
use crate::family::{ExperimentFamily, ParameterGrid};
use crate::memory::Record;
use crate::{
    CostModel, DatasetRef, DateRange, Experiment, ExperimentId, HypothesisId, RiskModel,
    StrategySpec,
};

/// What the `format` field of every file says, so a JSON document that happens
/// to parse is not mistaken for one of these.
pub const FORMAT: &str = "arvo.experiment";

/// The shape this build writes and reads. Bumped when a file written by the
/// previous build would deserialise into something it does not mean.
pub const VERSION: u32 = 1;

/// A configuration against a named rule, and how much searching produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedExperiment {
    pub format: String,
    pub version: u32,
    pub hypothesis: HypothesisId,
    /// The rule, by name, with the parameters every configuration shares.
    /// What the search varies is [`Self::grid`].
    pub strategy: StrategySpec,
    pub grid: ParameterGrid,
    pub in_sample_fraction: f64,
    /// How long the exporter's window was, in calendar days. A shape, not a
    /// period: the recipient's data decides the dates.
    pub window_days: i64,
    pub interval: arvo_data::BarInterval,
    pub costs: CostModel,
    pub risk: RiskModel,
    pub starting_cash: f64,
    pub seed: u64,
    pub criteria: EvaluationCriteria,
    pub search: Search,
}

/// The search behind a shared experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    /// Every configuration tried to arrive here — including any the exporter
    /// had itself imported, so a file passed along twice does not reset.
    pub trials: usize,
    /// Whether the exporter's winner cleared its own bar. Said, not relied on:
    /// the recipient re-runs and re-deflates regardless.
    pub survived_deflation: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShareError {
    #[error("not an Arvo experiment file: {0}")]
    NotAnExperiment(String),
    #[error("written by a newer version of Arvo (format {found}, this build reads {VERSION})")]
    Newer { found: u64 },
    #[error("written in format {found}, which this build does not read (it reads {VERSION})")]
    Older { found: u64 },
    #[error("names strategy {name:?}, which this build does not have; it has {known}")]
    UnknownStrategy { name: String, known: String },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unshareable(String),
}

/// A stored study, as a file.
///
/// # Errors
///
/// [`ShareError::Unshareable`] for a panel or walk-forward — not yet, rather
/// than never — and for a study recorded before its search surface was kept,
/// whose grid cannot be recovered.
pub fn export(record: &Record) -> Result<SharedExperiment, ShareError> {
    let Record::Study(found) = record else {
        return Err(ShareError::Unshareable(format!(
            "a {} cannot be shared yet; only a study can",
            record.kind()
        )));
    };
    if !found.selected.alongside.is_empty() {
        // A book is recorded as a study, and a file with no field for the
        // other members would quietly turn a question about capital shared
        // across several instruments into one about a single instrument.
        return Err(ShareError::Unshareable(
            "a book cannot be shared yet; the file has no way to name the instruments that \
             share its account"
                .to_owned(),
        ));
    }
    let selection = &found.selection;
    if selection.scored.is_empty() {
        return Err(ShareError::Unshareable(
            "this study was recorded before its search surface was kept, so the grid it \
             searched cannot be recovered; run it again to share it"
                .to_owned(),
        ));
    }

    // The grid, recovered from what ran. A configuration that failed is not in
    // the surface, so a value only it used is missing here — and is not in
    // `trials` either, so the count and the grid agree about what ran.
    let mut axes: BTreeMap<&str, BTreeSet<u64>> = BTreeMap::new();
    for trial in &selection.scored {
        for (name, value) in &trial.params {
            axes.entry(name).or_default().insert(value.to_bits());
        }
    }
    let grid = axes
        .iter()
        .fold(ParameterGrid::new(), |grid, (name, values)| {
            let mut values: Vec<f64> = values.iter().map(|bits| f64::from_bits(*bits)).collect();
            values.sort_by(f64::total_cmp);
            grid.axis(name, values)
        });

    let template = &found.selected;
    let fixed = template
        .strategy
        .params
        .iter()
        .filter(|(name, _)| !axes.contains_key(name.as_str()))
        .map(|(name, value)| (name.clone(), *value))
        .collect();

    let (in_days, out_days) = (found.in_sample.days(), found.out_of_sample.days());
    #[expect(clippy::cast_precision_loss, reason = "day counts are small")]
    let in_sample_fraction = in_days as f64 / (in_days + out_days) as f64;

    Ok(SharedExperiment {
        format: FORMAT.to_owned(),
        version: VERSION,
        hypothesis: found.hypothesis.clone(),
        strategy: StrategySpec {
            // A rule written as data travels with every trial (#225).
            rule: template.strategy.rule.clone(),
            name: template.strategy.name.clone(),
            params: fixed,
        },
        grid,
        in_sample_fraction,
        window_days: in_days + out_days,
        interval: template.interval,
        costs: template.costs,
        risk: template.risk.clone(),
        starting_cash: template.starting_cash,
        seed: template.seed,
        criteria: found.out_of_sample_evidence.criteria,
        search: Search {
            trials: selection.trials + selection.prior_trials,
            survived_deflation: selection.survived_deflation,
        },
    })
}

/// Reads a file, refusing anything this build cannot honour exactly.
///
/// `known` is the rule library this build runs — `arvo_nautilus::STRATEGIES`,
/// passed in because this crate cannot name the engine.
///
/// # Errors
///
/// [`ShareError`] naming what is wrong: not one of these files, a format this
/// build does not read, a strategy it does not have, or a value no run could
/// use. Never a partial import.
pub fn import(text: &str, known: &[&str]) -> Result<SharedExperiment, ShareError> {
    // The envelope before the body. A file from a newer build fails on
    // whichever field changed first, and "unknown field" describes a symptom.
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| ShareError::NotAnExperiment(err.to_string()))?;
    if value.get("format").and_then(serde_json::Value::as_str) != Some(FORMAT) {
        return Err(ShareError::NotAnExperiment(format!(
            "its format field is not {FORMAT:?}"
        )));
    }
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(found) if found > u64::from(VERSION) => return Err(ShareError::Newer { found }),
        Some(found) if found < u64::from(VERSION) => return Err(ShareError::Older { found }),
        Some(_) => {}
        None => return Err(ShareError::NotAnExperiment("it has no version".to_owned())),
    }

    let shared: SharedExperiment =
        serde_json::from_value(value).map_err(|err| ShareError::Invalid(err.to_string()))?;

    // A rule written as data (#225) travels inside the experiment and needs
    // no name in the library.
    if shared.strategy.rule.is_none() && !known.contains(&shared.strategy.name.as_str()) {
        return Err(ShareError::UnknownStrategy {
            name: shared.strategy.name,
            known: known.join(", "),
        });
    }
    let invalid = |why: &str| Err(ShareError::Invalid(why.to_owned()));
    if shared.grid.size() == 0 {
        return invalid("its grid is empty, so it searches nothing");
    }
    if !(shared.in_sample_fraction > 0.0 && shared.in_sample_fraction < 1.0) {
        return invalid("in_sample_fraction must be between 0 and 1, leaving both halves a period");
    }
    if shared.window_days <= 0 {
        return invalid("window_days must be positive");
    }
    if !(shared.starting_cash.is_finite() && shared.starting_cash > 0.0) {
        return invalid("starting_cash must be a positive amount");
    }
    if shared.search.trials == 0 {
        // The one lie this can catch: whatever produced a configuration tried
        // at least that configuration.
        return invalid("search.trials is zero, and anything shared was tried at least once");
    }
    Ok(shared)
}

impl SharedExperiment {
    /// The file as JSON, pretty-printed so a person can read and edit it.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("plain data always serialises")
    }

    /// A family ready to run on this machine's data, with the exporter's search
    /// counted into its bar.
    #[must_use]
    pub fn family(
        &self,
        instrument: &str,
        window: DateRange,
        dataset: DatasetRef,
    ) -> ExperimentFamily {
        let template = Experiment {
            id: ExperimentId(format!("shared-{}-{instrument}", self.strategy.name)),
            hypothesis: self.hypothesis.clone(),
            instrument: instrument.to_owned(),
            alongside: Vec::new(),
            underlying: None,
            window,
            interval: self.interval,
            dataset,
            strategy: self.strategy.clone(),
            costs: self.costs,
            risk: self.risk.clone(),
            starting_cash: self.starting_cash,
            seed: self.seed,
        };
        ExperimentFamily {
            in_sample_fraction: self.in_sample_fraction,
            prior_trials: self.search.trials,
            ..ExperimentFamily::new(template, self.grid.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::{expected_best_of, ScoredTrial};
    use crate::memory::tests::study;

    const KNOWN: &[&str] = &["sma_cross", "opening_range"];

    /// A stored study whose grid was fast in {10, 20} by slow in {50, 100}.
    fn searched() -> Record {
        let mut record = study("MSFT.NASDAQ", "v1");
        let Record::Study(found) = &mut record else {
            unreachable!("study() builds a study")
        };
        found.selection.scored = [(10.0, 50.0), (10.0, 100.0), (20.0, 50.0), (20.0, 100.0)]
            .iter()
            .map(|(fast, slow)| ScoredTrial {
                params: BTreeMap::from([("fast".to_owned(), *fast), ("slow".to_owned(), *slow)]),
                sharpe: fast / 100.0,
            })
            .collect();
        found.selection.trials = 4;
        found.selection.prior_trials = 3;
        found.selected.strategy.params = BTreeMap::from([
            ("fast".to_owned(), 20.0),
            ("slow".to_owned(), 50.0),
            ("trade_size".to_owned(), 100.0),
        ]);
        record
    }

    #[test]
    fn a_study_survives_export_and_import_intact() {
        let shared = export(&searched()).expect("a study with a surface");
        let back = import(&shared.to_json(), KNOWN).expect("its own output reads");
        assert_eq!(back, shared);
    }

    #[test]
    fn the_file_holds_the_search_not_the_winner() {
        let shared = export(&searched()).expect("exports");
        assert_eq!(
            shared.grid.size(),
            4,
            "the whole grid, recovered from what ran"
        );
        assert_eq!(
            shared.strategy.params,
            BTreeMap::from([("trade_size".to_owned(), 100.0)]),
            "the winning fast/slow are an answer, and do not travel as fixed values"
        );
        assert_eq!(
            shared.search.trials, 7,
            "its own four and the three it had imported: passing a file on does not reset it"
        );
    }

    #[test]
    fn no_answer_travels() {
        // Rule 3, checked on the wire rather than trusted to the type.
        let json = export(&searched()).expect("exports").to_json();
        for answer in [
            "dataset",
            "version\": \"v1",
            "verdict",
            "curve",
            "ledger",
            "sharpe",
        ] {
            assert!(!json.contains(answer), "{answer:?} leaked into the file");
        }
    }

    #[test]
    fn an_imported_family_counts_the_exporters_search_into_its_bar() {
        let shared = export(&searched()).expect("exports");
        let window = DateRange::new(
            chrono::NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid"),
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        )
        .expect("ordered");
        let family = shared.family(
            "AAPL.YF",
            window,
            DatasetRef {
                id: "AAPL.YF".to_owned(),
                version: "local".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
        );
        assert_eq!(family.prior_trials, 7);
        assert_eq!(family.grid.size(), 4);

        // And that counting it raises the bar a local run of four would set.
        let sharpes = [0.1, 0.2, 0.3, 0.4];
        let alone = expected_best_of(&sharpes, 4).expect("a bar");
        let counted = expected_best_of(&sharpes, 4 + family.prior_trials).expect("a bar");
        assert!(counted > alone, "{counted} against {alone}");
    }

    #[test]
    fn a_strategy_this_build_lacks_is_refused_by_name() {
        let mut shared = export(&searched()).expect("exports");
        shared.strategy.name = "pairs_trade".to_owned();
        let Err(ShareError::UnknownStrategy { name, known }) = import(&shared.to_json(), KNOWN)
        else {
            panic!("an unknown rule is refused, not skipped or substituted");
        };
        assert_eq!(name, "pairs_trade");
        assert!(known.contains("sma_cross"), "{known}");
    }

    #[test]
    fn a_file_from_a_newer_build_says_so_rather_than_naming_a_field() {
        let mut value: serde_json::Value =
            serde_json::from_str(&export(&searched()).expect("exports").to_json()).expect("json");
        value["version"] = serde_json::json!(VERSION + 1);
        value["something_new"] = serde_json::json!(true);
        assert_eq!(
            import(&value.to_string(), KNOWN),
            Err(ShareError::Newer {
                found: u64::from(VERSION + 1)
            })
        );
    }

    #[test]
    fn json_that_is_not_one_of_these_is_not_read_as_one() {
        assert!(matches!(
            import(r#"{"version": 1}"#, KNOWN),
            Err(ShareError::NotAnExperiment(_))
        ));
        assert!(matches!(
            import("not json", KNOWN),
            Err(ShareError::NotAnExperiment(_))
        ));
    }

    #[test]
    fn a_claim_of_zero_trials_is_refused() {
        let mut shared = export(&searched()).expect("exports");
        shared.search.trials = 0;
        assert!(matches!(
            import(&shared.to_json(), KNOWN),
            Err(ShareError::Invalid(_))
        ));
    }

    #[test]
    fn a_book_is_not_shared_as_if_it_were_one_instrument() {
        let mut record = searched();
        let Record::Study(found) = &mut record else {
            unreachable!("a study")
        };
        found.selected.alongside = vec!["AAPL.NASDAQ".to_owned()];
        assert!(matches!(export(&record), Err(ShareError::Unshareable(_))));
    }

    #[test]
    fn only_a_study_with_its_surface_can_be_shared() {
        assert!(matches!(
            export(&study("MSFT.NASDAQ", "v1")),
            Err(ShareError::Unshareable(_))
        ));
    }
}
