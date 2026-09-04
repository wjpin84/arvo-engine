//! Research commands: what the workbench can ask of the research loop.
//!
//! The UI never learns that Nautilus exists. It names an instrument, gets back
//! a verdict and the numbers behind it, and every type crossing this boundary
//! is an Arvo concept. Swapping the engine underneath would not change one
//! line of the front end.
//!
//! Everything reported here is *stated*, not implied — the split, the number
//! of configurations tried, the bar a no-skill search would clear, the costs
//! assumed. A verdict shown without those is a number that looks like a fact.

use std::path::PathBuf;
use std::sync::Arc;

use arvo_data::{BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{
    memory::{EvidenceStore, Record, StoredRecord},
    CostModel, DatasetRef, DateRange, Experiment, ExperimentFamily, ExperimentId, HypothesisId,
    Metrics, ParameterGrid, SimulationProvider, StrategySpec, Verdict,
};
use serde::Serialize;

use crate::commands::CommandError;

/// Where the workbench looks for daily bars: `<app data dir>/data`, one
/// `SYMBOL.VENUE.csv` per instrument.
pub const DATA_SUBDIR: &str = "data";

/// Where findings are kept, one JSON file each.
pub const EVIDENCE_SUBDIR: &str = "evidence";

/// The starting assumptions for a workbench-launched study.
///
/// Fixed for now, and surfaced in the result rather than hidden: a cost
/// assumption nobody sees is the most common way a backtest flatters itself.
/// These become editable when there is a reason to edit them.
const STARTING_CASH: f64 = 100_000.0;
const COMMISSION_BPS: f64 = 1.0;
const TRADE_SIZE: f64 = 100.0;
const STRATEGY: &str = "sma_cross";

/// Holds the wiring a research run needs, built once at startup.
pub struct ResearchService {
    simulation: Arc<NautilusSimulation<CsvBars>>,
    bars: CsvBars,
    data_dir: PathBuf,
    memory: EvidenceStore,
}

impl ResearchService {
    #[must_use]
    pub fn new(data_dir: PathBuf, evidence_dir: PathBuf) -> Self {
        Self {
            simulation: Arc::new(NautilusSimulation::new(CsvBars::new(data_dir.clone()))),
            bars: CsvBars::new(data_dir.clone()),
            data_dir,
            memory: EvidenceStore::new(evidence_dir),
        }
    }
}

/// What the workbench shows before anything has been run.
#[derive(Serialize)]
pub struct DataLibraryView {
    /// Shown so a user with no data knows where to put some.
    pub directory: String,
    pub instruments: Vec<InstrumentView>,
}

#[derive(Serialize)]
pub struct InstrumentView {
    pub id: String,
    /// `None` when the file exists but holds no usable bars.
    pub from: Option<String>,
    pub to: Option<String>,
    pub bars: usize,
    /// Content hash of the data as it stands right now. The workbench compares
    /// this against the hash recorded in a held result to tell whether that
    /// result still describes the data on disk.
    pub fingerprint: Option<String>,
}

#[derive(Serialize)]
pub struct MetricsView {
    pub total_return: f64,
    pub cagr: f64,
    pub max_drawdown: f64,
    pub volatility: f64,
    pub sharpe: Option<f64>,
    pub trades: u32,
}

impl From<&Metrics> for MetricsView {
    fn from(metrics: &Metrics) -> Self {
        Self {
            total_return: metrics.total_return,
            cagr: metrics.cagr,
            max_drawdown: metrics.max_drawdown,
            volatility: metrics.volatility,
            sharpe: metrics.sharpe,
            trades: metrics.trades,
        }
    }
}

/// The full result of a study, flattened for display.
#[derive(Serialize)]
pub struct StudyView {
    pub instrument: String,
    pub verdict: String,
    pub reasons: Vec<String>,

    // How hard the search was, and what that costs in credibility.
    pub trials: usize,
    pub best_sharpe: f64,
    pub expected_best_under_null: Option<f64>,
    pub survived_deflation: bool,

    // Which days chose the configuration, and which days judged it.
    pub in_sample: String,
    pub out_of_sample: String,
    pub selected_params: Vec<(String, f64)>,

    // The out-of-sample comparison itself.
    pub strategy: MetricsView,
    pub benchmark: MetricsView,
    pub excess_return: f64,

    // Stated assumptions, because a verdict without them is decoration.
    /// The dataset this result was produced from, as a content hash. Compared
    /// against the live one to decide whether the result is still current.
    pub dataset_version: String,
    pub strategy_name: String,
    pub starting_cash: f64,
    pub commission_bps: f64,
    pub engine: String,
}

/// A stored finding, summarised for the history list.
#[derive(Serialize)]
pub struct HistoryEntryView {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub verdict: String,
    pub recorded_at: String,
    /// True when the data this was produced from no longer matches disk.
    /// `None` when the data it referenced can no longer be found at all.
    pub stale: Option<bool>,
}

/// A stored finding, reopened.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordView {
    Study(Box<StudyView>),
    Panel(Box<PanelView>),
}

/// Everything held in research memory, newest first.
#[tauri::command]
pub async fn list_history(
    service: tauri::State<'_, ResearchService>,
) -> Result<Vec<HistoryEntryView>, CommandError> {
    let loaded = service
        .memory
        .load()
        .map_err(|err| CommandError::Failed(err.to_string()))?;

    // Reported, not swallowed: a record that cannot be read is a lost finding
    // and should look like one.
    for problem in &loaded.problems {
        tracing::warn!(problem, "could not read a stored finding");
    }

    Ok(loaded
        .records
        .iter()
        .map(|stored| {
            let live = live_dataset_version(&service, &stored.record);
            HistoryEntryView {
                id: stored.id.clone(),
                kind: match stored.record {
                    Record::Study(_) => "study",
                    Record::Panel(_) => "panel",
                }
                .to_owned(),
                subject: stored.record.subject(),
                verdict: verdict_label(stored.record.verdict()).to_owned(),
                recorded_at: stored.recorded_at.format("%Y-%m-%d %H:%M").to_string(),
                stale: live.map(|live| live != stored.record.dataset_version()),
            }
        })
        .collect())
}

/// Reopens one stored finding.
#[tauri::command]
pub async fn open_record(
    id: String,
    service: tauri::State<'_, ResearchService>,
) -> Result<RecordView, CommandError> {
    let loaded = service
        .memory
        .load()
        .map_err(|err| CommandError::Failed(err.to_string()))?;
    let stored = loaded
        .records
        .into_iter()
        .find(|stored| stored.id == id)
        .ok_or_else(|| CommandError::Failed(format!("no stored finding {id:?}")))?;

    let engine = service.simulation.engine();
    Ok(match stored.record {
        Record::Study(evidence) => RecordView::Study(Box::new(study_view(&evidence, engine))),
        Record::Panel(evidence) => RecordView::Panel(Box::new(panel_view(&evidence, engine))),
    })
}

/// What the data behind a finding hashes to *now*, or `None` if it is gone.
///
/// A panel's identity is every member's hash combined, so it is recomputed the
/// same way it was produced — over the instruments present today. An
/// instrument added or removed since therefore also reads as stale, which is
/// correct: the panel would not run the same way twice.
fn live_dataset_version(service: &ResearchService, record: &Record) -> Option<String> {
    match record {
        Record::Study(evidence) => service
            .bars
            .fingerprint(&evidence.selected.instrument)
            .ok()
            .flatten(),
        Record::Panel(_) => panel_dataset_version(&service.bars).map(|(version, _, _, _)| version),
    }
}

/// Lists the instruments the workbench can study.
#[tauri::command]
pub async fn list_instruments(
    service: tauri::State<'_, ResearchService>,
) -> Result<DataLibraryView, CommandError> {
    let directory = service.data_dir.display().to_string();
    let ids = service
        .bars
        .instruments()
        .map_err(|err| CommandError::Failed(format!("reading {directory}: {err}")))?;

    let instruments = ids
        .into_iter()
        .map(|id| {
            // A file that fails to parse should not blank the whole library;
            // it shows as an instrument with no coverage, which is visible
            // and recoverable rather than silently missing.
            let coverage = service.bars.coverage(&id).ok().flatten();
            let bars = coverage
                .map(|(from, to)| {
                    service
                        .bars
                        .daily_bars(&id, from, to)
                        .map_or(0, |bars| bars.len())
                })
                .unwrap_or_default();
            let fingerprint = service.bars.fingerprint(&id).ok().flatten();
            InstrumentView {
                id,
                from: coverage.map(|(from, _)| from.to_string()),
                to: coverage.map(|(_, to)| to.to_string()),
                bars,
                fingerprint,
            }
        })
        .collect();

    Ok(DataLibraryView {
        directory,
        instruments,
    })
}

/// Runs a parameter study on one instrument and reports what survives.
///
/// Offloaded to a blocking thread: a family is `trials + 2` backtests of
/// CPU-bound work, and running it on the async runtime would freeze the
/// window for the duration.
#[tauri::command]
pub async fn run_study(
    instrument: String,
    service: tauri::State<'_, ResearchService>,
) -> Result<StudyView, CommandError> {
    let simulation = service.simulation.clone();
    let coverage = service
        .bars
        .coverage(&instrument)
        .map_err(|err| CommandError::Failed(format!("reading {instrument}: {err}")))?
        .ok_or_else(|| CommandError::Failed(format!("{instrument} holds no bars")))?;

    let engine = simulation.engine().to_owned();
    let fingerprint = service
        .bars
        .fingerprint(&instrument)
        .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
        .ok_or_else(|| CommandError::Failed(format!("{instrument} holds no bars")))?;

    tauri::async_runtime::spawn_blocking(move || {
        let window = DateRange::new(coverage.0, coverage.1)
            .map_err(|err| CommandError::Failed(err.to_string()))?;
        let family = study_for(&instrument, window, &fingerprint);

        let found = arvo_research::run_family(
            simulation.as_ref(),
            &family,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = study_view(&found, &engine);
        Ok((view, Record::Study(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the study did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
}

/// Persists a finding and hands back its view.
///
/// A failed write does not fail the run: the result is real and already on
/// screen, and refusing to show it because a file could not be written would
/// throw away the expensive half over the cheap half. It is logged loudly
/// instead, since a store that silently stops recording is worse than one that
/// never started.
fn remember<V>(
    service: &tauri::State<'_, ResearchService>,
    (view, record): (V, Record),
) -> Result<V, CommandError> {
    let stored = StoredRecord::new(record, chrono::Utc::now());
    match service.memory.save(&stored) {
        Ok(path) => tracing::info!(id = %stored.id, path = %path.display(), "recorded a finding"),
        Err(err) => tracing::error!(error = %err, id = %stored.id, "could not record a finding"),
    }
    Ok(view)
}

/// The panel's window and combined dataset identity, over whatever
/// instruments currently have data.
///
/// The window is the *overlap* of what the members cover, not the union:
/// instruments judged over different periods are not a cross-section, and a
/// mean across them would compare different markets.
///
/// The identity is every member's hash combined, so editing any one file — or
/// adding or removing an instrument — marks the whole panel result stale.
fn panel_dataset_version(
    bars: &CsvBars,
) -> Option<(String, Vec<String>, chrono::NaiveDate, chrono::NaiveDate)> {
    let mut from = chrono::NaiveDate::MIN;
    let mut to = chrono::NaiveDate::MAX;
    let mut hasher = blake3::Hasher::new();
    let mut instruments = Vec::new();

    for id in bars.instruments().ok()? {
        let Ok(Some((first, last))) = bars.coverage(&id) else {
            continue;
        };
        if let Ok(Some(fingerprint)) = bars.fingerprint(&id) {
            hasher.update(fingerprint.as_bytes());
        }
        from = from.max(first);
        to = to.min(last);
        instruments.push(id);
    }

    if instruments.is_empty() {
        return None;
    }
    Some((
        hasher.finalize().to_hex().to_string(),
        instruments,
        from,
        to,
    ))
}

/// One instrument's out-of-sample outcome under the panel's configuration.
#[derive(Serialize)]
pub struct OutcomeView {
    pub instrument: String,
    pub strategy_return: f64,
    pub benchmark_return: f64,
    pub excess_return: f64,
    pub max_drawdown: f64,
    pub trades: u32,
}

/// A panel study, flattened for display.
#[derive(Serialize)]
pub struct PanelView {
    pub verdict: String,
    pub reasons: Vec<String>,

    pub instruments: usize,
    pub total_trades: u32,
    pub mean_excess_return: f64,
    pub beat_benchmark: usize,
    pub mean_max_drawdown: f64,
    pub worst_max_drawdown: f64,

    pub trials: usize,
    pub best_sharpe: f64,
    pub expected_best_under_null: Option<f64>,
    pub survived_deflation: bool,

    pub in_sample: String,
    pub out_of_sample: String,
    pub selected_params: Vec<(String, f64)>,
    pub per_instrument: Vec<OutcomeView>,
    pub failures: Vec<String>,

    pub dataset_version: String,
    pub strategy_name: String,
    pub starting_cash: f64,
    pub commission_bps: f64,
    pub engine: String,
}

/// Runs one configuration across every instrument that has data.
///
/// This is the study that can actually reach a verdict: a single instrument
/// produces a dozen or two round trips against a thirty-trade bar, and no
/// amount of history fixes that. Pooling across instruments does.
#[tauri::command]
pub async fn run_panel(
    service: tauri::State<'_, ResearchService>,
) -> Result<PanelView, CommandError> {
    let (dataset, instruments, from, to) = panel_dataset_version(&service.bars)
        .ok_or_else(|| CommandError::Failed("no instruments with usable data".to_owned()))?;

    let simulation = service.simulation.clone();
    let engine = simulation.engine().to_owned();

    tauri::async_runtime::spawn_blocking(move || {
        let window =
            DateRange::new(from, to).map_err(|err| CommandError::Failed(err.to_string()))?;
        let study = panel_for(instruments, window, &dataset);

        let found = arvo_research::run_panel(
            simulation.as_ref(),
            &study,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = panel_view(&found, &engine);
        Ok((view, Record::Panel(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the panel did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
}

/// Flattens a stored study for display.
///
/// Separate from the command so a finding read back from memory renders
/// identically to one just produced. Two projections would drift, and a
/// history that showed something subtly different from the live run would be
/// worse than no history.
fn study_view(found: &arvo_research::FamilyEvidence, engine: &str) -> StudyView {
    let evaluation = &found.out_of_sample_evidence.evaluation;
    StudyView {
        instrument: found.selected.instrument.clone(),
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        trials: found.selection.trials,
        best_sharpe: found.selection.best_sharpe,
        expected_best_under_null: found.selection.expected_best_under_null,
        survived_deflation: found.selection.survived_deflation,
        in_sample: format!("{} → {}", found.in_sample.from, found.in_sample.to),
        out_of_sample: format!("{} → {}", found.out_of_sample.from, found.out_of_sample.to),
        selected_params: found
            .selected
            .strategy
            .params
            .iter()
            .map(|(name, value)| (name.clone(), *value))
            .collect(),
        strategy: MetricsView::from(&evaluation.strategy),
        benchmark: MetricsView::from(&evaluation.benchmark),
        excess_return: evaluation.excess_return,
        dataset_version: found.selected.dataset.version.clone(),
        strategy_name: found.selected.strategy.name.clone(),
        starting_cash: found.selected.starting_cash,
        commission_bps: found.selected.costs.commission_bps,
        engine: engine.to_owned(),
    }
}

/// Flattens a stored panel for display. Same reasoning as [`study_view`].
fn panel_view(found: &arvo_research::PanelEvidence, engine: &str) -> PanelView {
    PanelView {
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        instruments: found.pooled.instruments,
        total_trades: found.pooled.total_trades,
        mean_excess_return: found.pooled.mean_excess_return,
        beat_benchmark: found.pooled.beat_benchmark,
        mean_max_drawdown: found.pooled.mean_max_drawdown,
        worst_max_drawdown: found.pooled.worst_max_drawdown,
        trials: found.selection.trials,
        best_sharpe: found.selection.best_sharpe,
        expected_best_under_null: found.selection.expected_best_under_null,
        survived_deflation: found.selection.survived_deflation,
        in_sample: format!("{} → {}", found.in_sample.from, found.in_sample.to),
        out_of_sample: format!("{} → {}", found.out_of_sample.from, found.out_of_sample.to),
        selected_params: found
            .selected_params
            .iter()
            .map(|(name, value)| (name.clone(), *value))
            .collect(),
        per_instrument: found
            .per_instrument
            .iter()
            .map(|outcome| OutcomeView {
                instrument: outcome.instrument.clone(),
                strategy_return: outcome.strategy.total_return,
                benchmark_return: outcome.benchmark.total_return,
                excess_return: outcome.excess_return,
                max_drawdown: outcome.strategy.max_drawdown,
                trades: outcome.strategy.trades,
            })
            .collect(),
        failures: found.failures.clone(),
        dataset_version: found.dataset.version.clone(),
        strategy_name: STRATEGY.to_owned(),
        starting_cash: STARTING_CASH,
        commission_bps: COMMISSION_BPS,
        engine: engine.to_owned(),
    }
}

const fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Supported => "Supported",
        Verdict::NotSupported => "Not supported",
        Verdict::Inconclusive => "Inconclusive",
    }
}

/// The panel the workbench runs: the same grid, one configuration chosen
/// across every instrument that has data.
///
/// The window is the *intersection* of what the instruments cover. Running
/// each over its own span would mean the panel's instruments were judged on
/// different market conditions, and a mean across those is not a
/// cross-sectional result.
#[must_use]
pub fn panel_for(
    instruments: Vec<String>,
    window: DateRange,
    dataset_version: &str,
) -> arvo_research::PanelStudy {
    let template = template_for("panel", window, dataset_version);
    arvo_research::PanelStudy::new(template, instruments, grid())
}

/// The grid both the single-instrument study and the panel sweep.
///
/// One definition, because the trial count is deflated against and two
/// definitions that drifted apart would make one of the two verdicts a lie.
fn grid() -> ParameterGrid {
    // Sized so the study can actually reach a conclusion. The first version of
    // this grid ran out to a 200-day average, which crosses roughly ten times
    // in twenty years of held-back data — against a 30-trade minimum, that
    // made every possible verdict Inconclusive before the return and drawdown
    // checks were even reached. A bar that nothing can clear is not
    // conservative, it is inert.
    ParameterGrid::new()
        .axis("fast", vec![5.0, 10.0, 20.0])
        .axis("slow", vec![30.0, 60.0, 120.0])
}

fn template_for(subject: &str, window: DateRange, dataset_version: &str) -> Experiment {
    Experiment {
        id: ExperimentId(format!("study-{subject}")),
        hypothesis: HypothesisId(format!("trend-following predicts returns in {subject}")),
        instrument: subject.to_owned(),
        window,
        dataset: DatasetRef {
            id: subject.to_owned(),
            // A content hash of the bars, so a stored result knows exactly
            // which data produced it. This used to be the fixed string
            // "local-csv", which meant editing a CSV left every earlier result
            // still claiming to be reproducible against it.
            version: dataset_version.to_owned(),
        },
        strategy: StrategySpec {
            name: STRATEGY.to_owned(),
            params: [("trade_size".to_owned(), TRADE_SIZE)]
                .into_iter()
                .collect(),
        },
        costs: CostModel {
            commission_bps: COMMISSION_BPS,
            slippage_bps: 0.0,
        },
        starting_cash: STARTING_CASH,
        seed: 1,
    }
}

/// The study the workbench runs: a moving-average grid over the instrument's
/// whole history.
///
/// Public so the `study` example can run exactly what the view runs. A second
/// definition of "the study" that drifted from this one would make headless
/// verification worthless.
///
/// The grid is fixed at nine configurations. That number is itself part of the
/// claim — [`arvo_research::run_family`] deflates the result by it — so it is
/// written here in the open rather than tuned per run.
pub fn study_for(instrument: &str, window: DateRange, dataset_version: &str) -> ExperimentFamily {
    ExperimentFamily::new(template_for(instrument, window, dataset_version), grid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn the_study_grid_is_nine_configurations_and_says_so() {
        let window = DateRange::new(
            NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid"),
            NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        )
        .expect("ordered");

        let family = study_for("AAPL.NASDAQ", window, "test-fingerprint");
        assert_eq!(
            family.grid.size(),
            9,
            "the trial count is deflated against, so it must be what it claims"
        );
        assert_eq!(family.template.strategy.name, STRATEGY);
        assert_eq!(
            family.template.dataset.version, "test-fingerprint",
            "the dataset identity must reach the record, or nothing can be found stale"
        );
        assert!(
            (family.template.costs.slippage_bps - 0.0).abs() < f64::EPSILON,
            "non-zero slippage is not honoured by the engine yet and would fail the run"
        );
    }
}
