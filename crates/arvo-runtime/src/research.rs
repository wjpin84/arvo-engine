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
/// A half-spread on a liquid US large cap, taken with a market order. Not
/// zero, because zero is the assumption that makes a backtest look best and
/// is true of no market — and it is now honoured by the engine rather than
/// merely recorded. See `arvo_nautilus`'s fill model for how it is charged.
const SLIPPAGE_BPS: f64 = 1.0;
const TRADE_SIZE: f64 = 100.0;

/// What the workbench offers, and what each one searches over.
///
/// The grid is part of the claim, not a convenience. Its size is deflated
/// against — the more configurations a study tries, the better the best of
/// them looks by luck alone — so every axis added here makes the verdict
/// harder to earn. They are deliberately small for that reason, and small
/// enough that a study can still reach a conclusion: an early version of the
/// crossover grid ran out to a 200-day average, which crosses about ten times
/// in twenty years and made every possible verdict `Inconclusive` before the
/// return check was reached. A bar nothing can clear is not conservative, it
/// is inert.
pub struct StrategyPlan {
    name: &'static str,
    /// What to call it in a menu.
    label: &'static str,
    /// One line on what it trades, because a name is not a description and
    /// the difference between these rules is the whole point of having them.
    premise: &'static str,
    /// Parameters every trial shares.
    fixed: &'static [(&'static str, f64)],
    /// What the search varies.
    axes: &'static [(&'static str, &'static [f64])],
    /// The resolution this rule is defined at.
    ///
    /// Two of them are anchored to a trading session and mean nothing on
    /// daily bars — the engine refuses that combination rather than running
    /// it, so the choice belongs here where the data can be checked for it.
    intraday: bool,
}

const PLANS: &[StrategyPlan] = &[
    StrategyPlan {
        name: "sma_cross",
        label: "Moving-average crossover",
        premise: "The control. Not a good idea, a rule nobody disputes.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("fast", &[5.0, 10.0, 20.0]),
            ("slow", &[30.0, 60.0, 120.0]),
        ],
        intraday: false,
    },
    StrategyPlan {
        name: "volatility_breakout",
        label: "Volatility breakout",
        premise: "A thrust measured in ATRs, so it means the same on any price.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("entry_atr_multiple", &[0.5, 1.0, 1.5]),
            ("atr_period", &[10.0, 20.0]),
        ],
        intraday: false,
    },
    StrategyPlan {
        name: "momentum_breakout",
        label: "Momentum breakout",
        premise: "Buy a new channel high, leave on a trailing channel low.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("entry_period", &[20.0, 55.0, 100.0]),
            ("exit_period", &[10.0, 20.0]),
        ],
        intraday: false,
    },
    StrategyPlan {
        name: "opening_range",
        label: "Opening range breakout",
        premise: "The session's first bars set a range; trade the break, once a day.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("range_bars", &[3.0, 6.0, 12.0]),
            ("target_range_multiple", &[1.0, 2.0]),
        ],
        intraday: true,
    },
    StrategyPlan {
        name: "vwap_reversion",
        label: "VWAP reversion",
        premise: "Stretch away from the session's average price is expected to close.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[("entry_deviations", &[1.0, 1.5, 2.0])],
        intraday: true,
    },
];

/// The resolution intraday studies run at.
///
/// Five minutes because that is what the data library holds and what the
/// Robinhood feed serves without special pleading. Not a parameter: changing
/// it changes what every session-anchored rule means, so it belongs in the
/// experiment record rather than in a UI field.
const INTRADAY: arvo_data::BarInterval =
    arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);

/// The default when nobody has chosen: the control.
const STRATEGY: &str = "sma_cross";

impl StrategyPlan {
    /// Looks a strategy up by the name the record stores.
    #[must_use]
    pub fn find(name: &str) -> Option<&'static Self> {
        PLANS.iter().find(|plan| plan.name == name)
    }

    fn interval(&self) -> arvo_data::BarInterval {
        if self.intraday {
            INTRADAY
        } else {
            arvo_data::BarInterval::DAILY
        }
    }

    fn grid(&self) -> ParameterGrid {
        self.axes.iter().fold(ParameterGrid::new(), |grid, (name, values)| {
            grid.axis(name, values.to_vec())
        })
    }

    /// How many backtests a study of this plan runs, so a progress message can
    /// say something truer than "working".
    fn backtests(&self) -> usize {
        // Every configuration in-sample, then the winner and its benchmark
        // out-of-sample.
        self.axes
            .iter()
            .map(|(_, values)| values.len())
            .product::<usize>()
            + 2
    }
}

/// Risk settings for a workbench study, from the middle of the range the
/// systematic-trading literature actually uses: a 2x ATR stop and 1% of
/// capital at risk per trade.
///
/// Stated here rather than left absent. Running with no stop is a different
/// strategy with a fatter left tail, and a study that quietly omitted one
/// would be answering an easier question than the one asked.
const STOP_ATR_MULTIPLE: f64 = 2.0;
const ATR_PERIOD: usize = 14;
const RISK_PER_TRADE: f64 = 0.01;

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
    pub sortino: Option<f64>,
    pub calmar: Option<f64>,
    pub trades: u32,
}

/// One month of the strategy's return, for the heatmap.
#[derive(Serialize)]
pub struct MonthlyReturnView {
    pub year: i32,
    pub month: u32,
    pub value: f64,
}

impl From<&Metrics> for MetricsView {
    fn from(metrics: &Metrics) -> Self {
        Self {
            total_return: metrics.total_return,
            cagr: metrics.cagr,
            max_drawdown: metrics.max_drawdown,
            volatility: metrics.volatility,
            sharpe: metrics.sharpe,
            sortino: metrics.sortino,
            calmar: metrics.calmar,
            trades: metrics.trades,
        }
    }
}

/// One point on a curve, in the shape a chart library wants: an ISO date and
/// a value.
#[derive(Serialize)]
pub struct CurvePoint {
    pub time: String,
    pub value: f64,
}

/// Flattens an equity curve for charting, collapsing any repeated day.
///
/// Charting libraries reject non-ascending or duplicated times, and typically
/// by throwing — which in a webview means a blank panel and no explanation.
/// Cheaper to guarantee the shape here than to debug it there.
fn curve_points(curve: &[arvo_research::EquityPoint]) -> Vec<CurvePoint> {
    let mut points: Vec<CurvePoint> = Vec::with_capacity(curve.len());
    for point in curve {
        // Date only: the chart draws a daily series, and the library keys
        // points by day. An intraday curve needs a time-aware axis, which is
        // a chart change rather than a data one.
        let time = point.at.date().to_string();
        match points.last_mut() {
            Some(last) if last.time == time => last.value = point.equity,
            _ => points.push(CurvePoint {
                time,
                value: point.equity,
            }),
        }
    }
    points
}

/// One thing to do about a finding, flattened for display.
#[derive(Serialize)]
pub struct RecommendationView {
    pub severity: String,
    pub finding: String,
    pub action: String,
    pub evidence: String,
}

/// What the round trips looked like, flattened for display.
///
/// Beside the metrics rather than inside them: metrics come from the equity
/// curve and these come from the ledger, and keeping the two apart is what
/// makes it obvious which is which when they say different things.
#[derive(Serialize)]
pub struct TradesView {
    pub closed: u32,
    pub still_open: u32,
    pub win_rate: Option<f64>,
    pub profit_factor: Option<f64>,
    pub expectancy: Option<f64>,
    pub average_win: Option<f64>,
    pub average_loss: Option<f64>,
    /// Mean holding period in days, which is the unit a reader thinks in and
    /// the one that decides short- versus long-term tax treatment.
    pub average_holding_days: Option<f64>,
    /// Every fee and commission the venue charged, in account currency.
    ///
    /// Fees only — slippage is charged inside the fill prices and is already
    /// reflected in the return, not here. Naming this `fees` rather than
    /// `cost` is the whole point: a reader who took it for the total cost of
    /// trading would be short by the spread assumption on every round trip.
    pub fees_paid: f64,
    /// Fees as a fraction of starting capital, so they can be read against
    /// the return directly.
    pub fees_fraction: f64,
    pub signal_exits: u32,
    pub stop_exits: u32,
}

impl TradesView {
    fn build(stats: &arvo_research::TradeStats, starting_cash: f64) -> Self {
        Self {
            closed: stats.closed,
            still_open: stats.still_open,
            win_rate: stats.win_rate,
            profit_factor: stats.profit_factor,
            expectancy: stats.expectancy(),
            average_win: stats.average_win,
            average_loss: stats.average_loss,
            average_holding_days: stats.average_holding_secs.map(|secs| secs / 86_400.0),
            fees_paid: stats.total_commission,
            fees_fraction: if starting_cash > 0.0 {
                stats.total_commission / starting_cash
            } else {
                0.0
            },
            signal_exits: stats.signal_exits,
            stop_exits: stats.stop_exits,
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

    /// The two curves behind the numbers. A table says a strategy returned
    /// less than the market; a chart says whether it did so steadily or lost
    /// it all in one month, and those are different findings.
    pub strategy_curve: Vec<CurvePoint>,
    pub benchmark_curve: Vec<CurvePoint>,
    /// Month-by-month, so a total return can be read as steady or as one
    /// lucky quarter. Derived from the same curve, not a second measurement.
    pub monthly: Vec<MonthlyReturnView>,
    /// The round trips behind the return, and what they cost.
    pub trades_detail: TradesView,
    /// What to do about this finding, most stopping first.
    ///
    /// Derived on read rather than stored, so a finding pulled out of memory
    /// is read against today's rules rather than the ones in force when it
    /// was recorded.
    pub recommendations: Vec<RecommendationView>,

    // Stated assumptions, because a verdict without them is decoration.
    /// The dataset this result was produced from, as a content hash. Compared
    /// against the live one to decide whether the result is still current.
    pub dataset_version: String,
    pub strategy_name: String,
    pub starting_cash: f64,
    pub commission_bps: f64,
    pub slippage_bps: f64,
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
            // At the resolution the finding was produced at, not a default:
            // the same instrument at two resolutions is two datasets, and
            // hashing the wrong one would call a current result stale.
            .fingerprint(&evidence.selected.instrument, evidence.selected.interval)
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
            let coverage = service
                .bars
                .coverage(&id, arvo_data::BarInterval::DAILY)
                .ok()
                .flatten();
            let bars = coverage
                .map(|(from, to)| {
                    service
                        .bars
                        .daily_bars(&id, from, to)
                        .map_or(0, |bars| bars.len())
                })
                .unwrap_or_default();
            let fingerprint = service
                .bars
                .fingerprint(&id, arvo_data::BarInterval::DAILY)
                .ok()
                .flatten();
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
    strategy: Option<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<StudyView, CommandError> {
    let name = strategy.unwrap_or_else(|| STRATEGY.to_owned());
    let plan = StrategyPlan::find(&name)
        .ok_or_else(|| CommandError::Failed(format!("no strategy called {name:?}")))?;
    let interval = plan.interval();

    let simulation = service.simulation.clone();
    // The resolution the *strategy* needs, not whatever the library happens to
    // hold. Saying which resolution is missing is the difference between a
    // usable message and "holds no bars" on an instrument the sidebar just
    // listed.
    let missing = || {
        CommandError::Failed(format!(
            "{instrument} holds no {interval} bars; {} is defined at that resolution",
            plan.label
        ))
    };
    let coverage = service
        .bars
        .coverage(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("reading {instrument}: {err}")))?
        .ok_or_else(missing)?;

    let engine = simulation.engine().to_owned();
    let fingerprint = service
        .bars
        .fingerprint(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
        .ok_or_else(missing)?;

    tauri::async_runtime::spawn_blocking(move || {
        let window = DateRange::new(coverage.0, coverage.1)
            .map_err(|err| CommandError::Failed(err.to_string()))?;
        let family = study_for(&instrument, plan, window, &fingerprint);

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
        let Ok(Some((first, last))) = bars.coverage(&id, arvo_data::BarInterval::DAILY) else {
            continue;
        };
        if let Ok(Some(fingerprint)) = bars.fingerprint(&id, arvo_data::BarInterval::DAILY) {
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
    pub slippage_bps: f64,
    pub engine: String,
}

/// A strategy the workbench can run.
#[derive(Serialize)]
pub struct StrategyView {
    pub name: String,
    pub label: String,
    pub premise: String,
    /// Spelled out rather than a boolean, because it is the reason a study
    /// may refuse an instrument the sidebar just listed.
    pub interval: String,
    /// Backtests one study will run. A spinner that says how much work is
    /// coming is the difference between waiting and suspecting a hang.
    pub backtests: usize,
}

/// What can be run, so the UI offers the engine's actual list rather than a
/// copy of it that drifts.
///
/// # Errors
///
/// Never. Fallible only to match the shape every other command has.
#[tauri::command]
#[allow(clippy::unnecessary_wraps, reason = "uniform command signature")]
pub fn list_strategies() -> Result<Vec<StrategyView>, CommandError> {
    Ok(PLANS
        .iter()
        .map(|plan| StrategyView {
            name: plan.name.to_owned(),
            label: plan.label.to_owned(),
            premise: plan.premise.to_owned(),
            interval: plan.interval().to_string(),
            backtests: plan.backtests(),
        })
        .collect())
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
        strategy_curve: curve_points(&evaluation.strategy_curve),
        benchmark_curve: curve_points(&evaluation.benchmark_curve),
        monthly: arvo_research::evaluation::monthly_returns(&evaluation.strategy_curve)
            .into_iter()
            .map(|month| MonthlyReturnView {
                year: month.year,
                month: month.month,
                value: month.value,
            })
            .collect(),
        trades_detail: TradesView::build(
            &evaluation.strategy_trades,
            found.selected.starting_cash,
        ),
        recommendations: arvo_research::recommend(found)
            .into_iter()
            .map(|item| RecommendationView {
                severity: item.severity.label().to_owned(),
                finding: item.finding,
                action: item.action,
                evidence: item.evidence,
            })
            .collect(),
        dataset_version: found.selected.dataset.version.clone(),
        strategy_name: found.selected.strategy.name.clone(),
        starting_cash: found.selected.starting_cash,
        commission_bps: found.selected.costs.commission_bps,
        slippage_bps: found.selected.costs.slippage_bps,
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
        slippage_bps: SLIPPAGE_BPS,
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
    let plan = StrategyPlan::find(STRATEGY).expect("the default strategy is in PLANS");
    let template = template_for("panel", plan, window, dataset_version);
    arvo_research::PanelStudy::new(template, instruments, plan.grid())
}

fn template_for(
    subject: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
) -> Experiment {
    Experiment {
        id: ExperimentId(format!("study-{subject}")),
        hypothesis: HypothesisId(format!("trend-following predicts returns in {subject}")),
        instrument: subject.to_owned(),
        window,
        // From the strategy, not fixed. An opening range on daily bars is not
        // a slower opening range, it is a different rule — and the engine
        // refuses the combination rather than producing a curve for it.
        interval: plan.interval(),
        dataset: DatasetRef {
            id: subject.to_owned(),
            // A content hash of the bars, so a stored result knows exactly
            // which data produced it. This used to be the fixed string
            // "local-csv", which meant editing a CSV left every earlier result
            // still claiming to be reproducible against it.
            version: dataset_version.to_owned(),
        },
        strategy: StrategySpec {
            name: plan.name.to_owned(),
            params: plan
                .fixed
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect(),
        },
        costs: CostModel::proportional(COMMISSION_BPS, SLIPPAGE_BPS),
        risk: arvo_research::RiskModel {
            stop_atr_multiple: Some(STOP_ATR_MULTIPLE),
            atr_period: ATR_PERIOD,
            risk_per_trade: Some(RISK_PER_TRADE),
            ..arvo_research::RiskModel::default()
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
pub fn study_for(
    instrument: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
) -> ExperimentFamily {
    ExperimentFamily::new(
        template_for(instrument, plan, window, dataset_version),
        plan.grid(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn window() -> DateRange {
        DateRange::new(
            NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid"),
            NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
        )
        .expect("ordered")
    }

    #[test]
    fn every_offered_strategy_can_be_planned_by_the_engine() {
        // `PLANS` is what the sidebar menu shows. A plan the engine rejects —
        // a missing parameter, a period pair the rule refuses — is a failure
        // the user only discovers after choosing it and waiting.
        for plan in PLANS {
            let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
            for combination in family.grid.combinations() {
                let mut spec = family.template.strategy.clone();
                spec.params.extend(combination);
                assert!(
                    arvo_nautilus::check_plan(&spec, family.template.interval).is_ok(),
                    "{} cannot be planned: {:?}",
                    plan.name,
                    spec.params
                );
            }
        }
    }

    #[test]
    fn a_session_anchored_strategy_asks_for_intraday_bars() {
        // The reason the interval lives on the plan: an opening range on daily
        // bars is a different rule, and the engine refuses it. Getting this
        // wrong means the study fails at the last moment instead of asking
        // for the right data.
        for plan in PLANS {
            let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
            assert_eq!(
                family.template.interval.is_intraday(),
                plan.intraday,
                "{} asked for the wrong resolution",
                plan.name
            );
        }
    }

    #[test]
    fn the_study_grid_is_nine_configurations_and_says_so() {
        let plan = StrategyPlan::find(STRATEGY).expect("the default is offered");
        let family = study_for("AAPL.NASDAQ", plan, window(), "test-fingerprint");
        assert_eq!(
            family.grid.size(),
            9,
            "the trial count is deflated against, so it must be what it claims"
        );
        assert_eq!(
            plan.backtests(),
            11,
            "the spinner promises this many, so it has to be what runs"
        );
        assert_eq!(family.template.strategy.name, STRATEGY);
        assert_eq!(
            family.template.dataset.version, "test-fingerprint",
            "the dataset identity must reach the record, or nothing can be found stale"
        );
        assert!(
            family.template.costs.slippage_bps > 0.0,
            "a study that assumes free fills is the optimistic one, and the engine              honours slippage now"
        );
    }
}
