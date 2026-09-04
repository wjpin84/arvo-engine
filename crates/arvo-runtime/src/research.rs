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
    CostModel, DatasetRef, DateRange, Experiment, ExperimentFamily, ExperimentId, HypothesisId,
    Metrics, ParameterGrid, SimulationProvider, StrategySpec, Verdict,
};
use serde::Serialize;

use crate::commands::CommandError;

/// Where the workbench looks for daily bars: `<app data dir>/data`, one
/// `SYMBOL.VENUE.csv` per instrument.
pub const DATA_SUBDIR: &str = "data";

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
}

impl ResearchService {
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            simulation: Arc::new(NautilusSimulation::new(CsvBars::new(data_dir.clone()))),
            bars: CsvBars::new(data_dir.clone()),
            data_dir,
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

        let evaluation = &found.out_of_sample_evidence.evaluation;
        Ok(StudyView {
            instrument,
            verdict: match found.verdict {
                Verdict::Supported => "Supported",
                Verdict::NotSupported => "Not supported",
                Verdict::Inconclusive => "Inconclusive",
            }
            .to_owned(),
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
            engine,
        })
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the study did not finish: {err}")))?
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
    let template = Experiment {
        id: ExperimentId(format!("study-{instrument}")),
        hypothesis: HypothesisId(format!("trend-following predicts returns in {instrument}")),
        instrument: instrument.to_owned(),
        window,
        dataset: DatasetRef {
            id: instrument.to_owned(),
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
    };

    ExperimentFamily::new(
        template,
        // Sized so the study can actually reach a conclusion. The first
        // version of this grid ran out to a 200-day average, which crosses
        // roughly ten times in twenty years of held-back data — against a
        // 30-trade minimum, that made every possible verdict Inconclusive
        // before the return and drawdown checks were even reached. A bar that
        // nothing can clear is not conservative, it is inert.
        //
        // The honest reading of that: slow trend following cannot be
        // validated on one instrument's history at all. It needs breadth
        // across many instruments, which the machinery does not do yet.
        ParameterGrid::new()
            .axis("fast", vec![5.0, 10.0, 20.0])
            .axis("slow", vec![30.0, 60.0, 120.0]),
    )
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
