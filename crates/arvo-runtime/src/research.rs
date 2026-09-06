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
// The view shapes live in `arvo-views` so the window cannot drift from
// them. See that crate for what two hand-mirrored copies cost.
pub use arvo_views::{CandlePoint, DataFindingView, HistoryView, MatchView, UnreadableView, CurvePoint, DataLibraryView, FetchView, FoldView, HistoryEntryView, InstrumentView, MetricsView, MonthlyReturnView, OutcomeView, PanelView, RecommendationView, RecordView, StabilityView, StrategyView, StudyView, SurfaceCell, SurfaceView, TradeMarkerView, TradeRowView, TradesView, WalkForwardView};

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

/// A metrics summary, as the window sees it.
///
/// A free function rather than a `From` impl: [`MetricsView`] lives in
/// `arvo-views` and `Metrics` in `arvo-research`, so neither is local here and
/// the orphan rule forbids the impl.
pub fn metrics_view(metrics: &Metrics) -> MetricsView {

MetricsView {
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

/// Flattens an equity curve for charting, collapsing any repeated day.
///
/// Charting libraries reject non-ascending or duplicated times, and typically
/// by throwing — which in a webview means a blank panel and no explanation.
/// An equity curve as the chart wants it.
///
/// One point per bar, in epoch seconds. Nothing is collapsed or deduplicated:
/// the curve is already one point per bar by construction, and thinning it
/// here would draw a different line from the one the metrics were computed on.
fn curve_points(curve: &[arvo_research::EquityPoint]) -> Vec<CurvePoint> {
    curve
        .iter()
        .map(|point| CurvePoint {
            time: point.at.and_utc().timestamp(),
            value: point.equity,
        })
        .collect()
}

/// What is wrong with the bars a result was produced from.
///
/// Run at report time rather than at fetch time, and attached to the *result*.
/// A verdict is only as good as the series under it, and the place a person
/// will actually read "these bars have a hole in them" is next to the number
/// it undermines — not in a data screen they would have to think to open.
fn data_findings(
    bars: &dyn arvo_data::BarProvider,
    instrument: &str,
    interval: arvo_data::BarInterval,
    window: &DateRange,
) -> Vec<DataFindingView> {
    let series = bars
        .bars(instrument, interval, window.from, window.to)
        .unwrap_or_default();

    arvo_data::quality::inspect(&series, interval)
        .findings
        .into_iter()
        .map(|finding| DataFindingView {
            severity: match finding.severity {
                arvo_data::quality::Severity::Fault => "fault",
                arvo_data::quality::Severity::Suspect => "suspect",
            }
            .to_owned(),
            kind: finding.kind.to_owned(),
            at: finding.at.map(|at| at.format("%Y-%m-%d %H:%M").to_string()),
            detail: finding.detail,
        })
        .collect()
}

/// The instrument's own bars over a window.
///
/// Read from the library at render time rather than stored in the finding.
/// The bars are the *input* to an experiment and are already identified by a
/// content hash; copying them into every stored result would duplicate
/// megabytes to say something the hash already says. If the file has changed
/// since, the finding is marked stale by the machinery that exists for it.
fn candles(
    bars: &dyn arvo_data::BarProvider,
    instrument: &str,
    interval: arvo_data::BarInterval,
    window: &DateRange,
) -> Vec<CandlePoint> {
    bars.bars(instrument, interval, window.from, window.to)
        .unwrap_or_default()
        .into_iter()
        .map(|bar| CandlePoint {
            time: bar.at.and_utc().timestamp(),
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
        })
        .collect()
}

/// Every entry and exit in a ledger, as chart markers on the bars that caused
/// them.
///
/// # The interval is not decoration
///
/// A trade's timestamp is the instant it *filled*, and a fill happens at the
/// close of the bar the signal was read from — that is the whole look-ahead
/// convention this platform is built on. A candle, meanwhile, is stamped at
/// the instant it *opens*. So a fill on the bar opening at 09:30 carries the
/// time 09:35, and drawing it there puts every marker one bar to the right of
/// the bar that actually produced it.
///
/// Shifting back by one interval is what lines them up. It is invisible if you
/// do not look for it: the chart would render, the markers would sit on
/// plausible candles, and every entry would appear to have been taken one bar
/// after the rule fired.
fn markers(
    ledger: &[arvo_research::Trade],
    interval: arvo_data::BarInterval,
) -> Vec<TradeMarkerView> {
    let step = interval.duration();
    let on_bar = |at: chrono::NaiveDateTime| (at - step).and_utc().timestamp();

    let mut out = Vec::with_capacity(ledger.len() * 2);
    for trade in ledger {
        out.push(TradeMarkerView {
            time: on_bar(trade.opened),
            kind: "entry".to_owned(),
            reason: String::new(),
            label: format!("{:.0} @ {:.2}", trade.quantity, trade.entry),
        });
        if let (Some(closed), Some(exit)) = (trade.closed, trade.exit) {
            out.push(TradeMarkerView {
                time: on_bar(closed),
                kind: "exit".to_owned(),
                // A stop-out, a halt and a signal exit look identical in a
                // summary and could not be more different in what they say
                // about the rule: the first says the trade failed, the second
                // says the *account* did, and the third says the rule chose to
                // leave. Matched exhaustively so a fourth reason cannot
                // silently join the third.
                reason: match trade.exit_reason {
                    arvo_research::ExitReason::Stop => "stop",
                    arvo_research::ExitReason::Halted => "halt",
                    arvo_research::ExitReason::Signal
                    | arvo_research::ExitReason::StillOpen => "signal",
                }
                .to_owned(),
                label: format!("{exit:.2} ({:+.0})", trade.pnl),
            });
        }
    }
    // The chart requires markers in time order and throws on anything else,
    // and an exception crossing back into wasm takes the calling future with
    // it — so this is not a tidiness sort.
    out.sort_by_key(|marker| marker.time);
    out
}

/// The equity curve expressed as depth below its own running peak.
///
/// Drawn rather than summarised because a single worst-drawdown number cannot
/// distinguish one deep hole from a decade spent underwater, and those are
/// different things to have lived through.
fn underwater(curve: &[arvo_research::EquityPoint]) -> Vec<CurvePoint> {
    let mut peak = f64::NEG_INFINITY;
    curve
        .iter()
        .map(|point| {
            peak = peak.max(point.equity);
            CurvePoint {
                time: point.at.and_utc().timestamp(),
                // Negative, so the series hangs below zero the way every
                // underwater plot in the literature does.
                value: if peak > 0.0 {
                    (point.equity - peak) / peak * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect()
}

/// What the round trips looked like, as the window sees it.
///
/// A free function for the same reason as the two above.
pub fn trades_view(stats: &arvo_research::TradeStats, starting_cash: f64) -> TradesView {

    TradesView {
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

/// Everything in research memory, newest first.
///
/// Reads the store's *summaries* rather than its findings. A stored finding is
/// around 400 KB — curves, ledgers, per-fold evidence, the search surface —
/// and this renders a column of names; parsing every byte of every one to do
/// that cost 1.7 MB for five findings and would cost seconds for five hundred.
///
/// Findings that could not be read come back too. They used to be a log line,
/// which is how four of them were lost to a field rename without anyone being
/// told.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the directory cannot be listed.
#[tauri::command]
pub async fn list_history(
    service: tauri::State<'_, ResearchService>,
) -> Result<HistoryView, CommandError> {
    let (summaries, unreadable) = service
        .memory
        .summaries()
        .map_err(|err| CommandError::Failed(err.to_string()))?;

    Ok(HistoryView {
        entries: summaries
            .iter()
            .map(|summary| {
                let live = live_version(&service, summary);
                HistoryEntryView {
                    id: summary.id.clone(),
                    kind: summary.kind.clone(),
                    subject: summary.subject.clone(),
                    verdict: verdict_label(summary.verdict).to_owned(),
                    recorded_at: summary.recorded_at.format("%Y-%m-%d %H:%M").to_string(),
                    stale: live.map(|live| live != summary.dataset_version),
                }
            })
            .collect(),
        unreadable: unreadable
            .into_iter()
            .map(|item| UnreadableView {
                id: item.id,
                reason: item.reason,
            })
            .collect(),
    })
}

/// What the data behind a finding hashes to *now*, or `None` if it is gone.
///
/// From the summary, so staleness costs no parsing either. A panel's identity
/// is every member's hash combined and is recomputed the same way it was
/// produced — over the instruments present today, so one added or removed also
/// reads as stale, which is correct: the panel would not run the same twice.
fn live_version(service: &ResearchService, summary: &arvo_research::Summary) -> Option<String> {
    match (&summary.instrument, summary.interval) {
        (Some(instrument), Some(interval)) => {
            service.bars.fingerprint(instrument, interval).ok().flatten()
        }
        _ => panel_dataset_version(&service.bars).map(|(version, _, _, _)| version),
    }
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
        Record::Study(evidence) => {
            RecordView::Study(Box::new(study_view(&evidence, &service.bars, engine)))
        }
        Record::Panel(evidence) => RecordView::Panel(Box::new(panel_view(&evidence, engine))),
        Record::WalkForward(evidence) => {
            RecordView::WalkForward(Box::new(walk_forward_view(
                &evidence,
                &service.bars,
                engine,
            )))
        }
    })
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

    // A copy for the blocking closure: rendering the report reads the bars
    // back to draw them, and the `State` cannot cross that boundary.
    let library = service.bars.clone();
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

        let view = study_view(&found, &library, &engine);
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

/// Whether a broker connection is stored, so the UI knows what to offer.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the keychain cannot be read.
#[tauri::command]
pub fn feed_connected() -> Result<bool, CommandError> {
    crate::feed::is_connected().map_err(|err| CommandError::Failed(err.to_string()))
}

/// Signs in to the broker: opens the browser and waits for the redirect.
///
/// One command rather than two — begin, then finish — because the flow holds a
/// bound socket and a PKCE verifier between those halves, and parking that in
/// shared state so a second command could find it would mean a half-finished
/// sign-in outliving the window that started it. Here the whole flow lives on
/// one stack and ends when it ends.
///
/// It is therefore a slow command: it returns when someone finishes in their
/// browser, or after [`arvo_oauth::DEFAULT_TIMEOUT`].
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the browser cannot be opened, consent
/// is refused, or nobody completes the sign-in.
#[tauri::command]
pub async fn connect_feed(app: tauri::AppHandle) -> Result<bool, CommandError> {
    use tauri_plugin_opener::OpenerExt as _;

    // Logged at every step. The first attempt at this failed and left no
    // trace anywhere: the error went to the UI and only to the UI, and the UI
    // was showing something else at the time. An authorization flow has five
    // places to fail across two processes and a browser, and "it failed" is
    // not a diagnosis.
    tracing::info!("starting the {} sign-in", crate::feed::FEED_ID);
    let pending = crate::feed::begin_sign_in().await.map_err(|err| {
        tracing::error!(error = %err, "could not start the sign-in");
        CommandError::Failed(err.to_string())
    })?;
    tracing::info!(
        client_id = %pending.client_id,
        url = %pending.url,
        "registered; waiting for the browser redirect"
    );

    // The system browser, not a window in this app. A sign-in page rendered
    // inside the app cannot be told apart from one the app drew itself, so a
    // user has no way to check what they are typing a password into — which
    // is the whole reason RFC 8252 says to use the external agent.
    app.opener()
        .open_url(pending.url.clone(), None::<&str>)
        .map_err(|err| {
            tracing::error!(error = %err, "could not open a browser");
            CommandError::Failed(format!(
                "could not open a browser for the sign-in ({err}); the address is {}",
                pending.url
            ))
        })?;

    crate::feed::complete_sign_in(pending).await.map_err(|err| {
        // The server's own words, not a summary of them. An expired grant, a
        // rejected redirect and an unknown parameter are three different
        // problems with three different fixes.
        tracing::error!(error = %err, "the sign-in did not complete");
        CommandError::Failed(err.to_string())
    })?;
    tracing::info!("{} sign-in complete", crate::feed::FEED_ID);
    Ok(true)
}

/// Forgets the stored broker connection.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the keychain rejects the delete.
#[tauri::command]
pub fn disconnect_feed() -> Result<bool, CommandError> {
    crate::feed::disconnect().map_err(|err| CommandError::Failed(err.to_string()))?;
    Ok(false)
}

/// Finds instruments by name or ticker.
///
/// Exists because the alternative was typing `MSFT.NASDAQ` into an empty box
/// and knowing both halves of it — the ticker, and a venue convention that is
/// Arvo's rather than the market's.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if there is no broker connection or the
/// search call fails.
#[tauri::command]
pub async fn search_instruments(
    query: String,
    service: tauri::State<'_, ResearchService>,
) -> Result<Vec<MatchView>, CommandError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let held: std::collections::HashSet<String> =
        service.bars.instruments().unwrap_or_default().into_iter().collect();

    Ok(crate::feed::search(&service.data_dir, query, 10)
        .await
        .map_err(|err| CommandError::Failed(err.to_string()))?
        .into_iter()
        .map(|found| MatchView {
            held: held.contains(&found.instrument),
            instrument: found.instrument,
            symbol: found.symbol,
            name: found.name,
            price: found.price,
            change: found.change,
        })
        .collect())
}

/// Pulls one instrument's bars into the data library.
///
/// Fetching is a separate act from running, deliberately. An experiment pins
/// its dataset as a content hash of the bars it ran on, so a provider that
/// went to the network mid-backtest would give a different answer whenever the
/// vendor revised a bar and every stored verdict would quietly stop being
/// checkable. See [`crate::feed`].
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if there is no token, the resolution is
/// one the broker does not serve, or nothing comes back.
#[tauri::command]
pub async fn fetch_bars(
    instrument: String,
    interval: String,
    days: Option<u32>,
    service: tauri::State<'_, ResearchService>,
) -> Result<FetchView, CommandError> {
    let interval: arvo_data::BarInterval = interval
        .parse()
        .map_err(|err| CommandError::Failed(format!("{interval:?}: {err}")))?;

    // A default that is generous for a daily pull and modest for an intraday
    // one, where the same span is two orders of magnitude more bars.
    let days = days.unwrap_or(if interval.is_intraday() { 30 } else { 3_650 });
    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(i64::from(days));

    let report = crate::feed::fetch(&service.data_dir, &instrument, interval, from, to)
        .await
        .map_err(|err| CommandError::Failed(err.to_string()))?;

    Ok(FetchView {
        instrument: report.instrument,
        interval: report.interval.to_string(),
        bars: report.bars,
        interpolated: report.interpolated,
        from: report.from.map(|at| at.to_string()),
        to: report.to.map(|at| at.to_string()),
        data_findings: report
            .quality
            .findings
            .into_iter()
            .map(|finding| DataFindingView {
                severity: match finding.severity {
                    arvo_data::quality::Severity::Fault => "fault",
                    arvo_data::quality::Severity::Suspect => "suspect",
                }
                .to_owned(),
                kind: finding.kind.to_owned(),
                at: finding.at.map(|at| at.format("%Y-%m-%d %H:%M").to_string()),
                detail: finding.detail,
            })
            .collect(),
    })
}

/// Runs a rolling re-selection over an instrument's whole history.
///
/// Slower than a study by roughly the number of folds — every fold is a full
/// grid search plus an out-of-sample run — which is why the caller is told the
/// backtest count before it starts.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the instrument has no bars at the
/// strategy's resolution, or the span is too short to roll.
#[tauri::command]
pub async fn run_walk_forward(
    instrument: String,
    strategy: Option<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<WalkForwardView, CommandError> {
    let name = strategy.unwrap_or_else(|| STRATEGY.to_owned());
    let plan = StrategyPlan::find(&name)
        .ok_or_else(|| CommandError::Failed(format!("no strategy called {name:?}")))?;
    let interval = plan.interval();

    let simulation = service.simulation.clone();
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
    let fingerprint = service
        .bars
        .fingerprint(&instrument, interval)
        .map_err(|err| CommandError::Failed(format!("hashing {instrument}: {err}")))?
        .ok_or_else(missing)?;
    let engine = simulation.engine().to_owned();

    let library = service.bars.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let window = DateRange::new(coverage.0, coverage.1)
            .map_err(|err| CommandError::Failed(err.to_string()))?;
        let procedure = walk_forward_for(&instrument, plan, window, &fingerprint);

        let found = arvo_research::run_walk_forward(
            simulation.as_ref(),
            &procedure,
            &arvo_research::EvaluationCriteria::default(),
        )
        .map_err(|err| CommandError::Failed(err.to_string()))?;

        let view = walk_forward_view(&found, &library, &engine);
        Ok((view, Record::WalkForward(Box::new(found))))
    })
    .await
    .map_err(|err| CommandError::Failed(format!("the walk-forward did not finish: {err}")))
    .and_then(|outcome| outcome.and_then(|outcome| remember(&service, outcome)))
}

/// Where exports are written.
pub const EXPORTS_SUBDIR: &str = "exports";

/// Writes a ledger to CSV and reveals it in the file manager.
///
/// A fixed directory beside the evidence rather than a save dialog: a dialog
/// needs another Tauri plugin and another capability, and the thing anyone
/// actually wants is the file, in a place they can find twice. Revealing it
/// with the opener already in the app is the whole of the "where did it go"
/// problem.
///
/// Quoting is real, not assumed away. A ledger holds timestamps and numbers
/// today, and the moment a strategy name or a note reaches a cell, an
/// unquoted writer silently shifts every column after it — the same failure
/// that ate a fund name in the portfolio importer.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the directory cannot be created, the
/// file cannot be written, or the file manager cannot be opened.
#[tauri::command]
pub fn export_trades(
    name: String,
    rows: Vec<TradeRowExport>,
    app: tauri::AppHandle,
    service: tauri::State<'_, ResearchService>,
) -> Result<String, CommandError> {
    use tauri_plugin_opener::OpenerExt as _;

    let directory = service.data_dir.parent().map_or_else(
        || service.data_dir.join(EXPORTS_SUBDIR),
        |root| root.join(EXPORTS_SUBDIR),
    );
    std::fs::create_dir_all(&directory)
        .map_err(|err| CommandError::Failed(format!("creating {}: {err}", directory.display())))?;

    // Slugged, because the name comes from an instrument id and a path
    // separator in it would write somewhere nobody asked for.
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = directory.join(format!("{slug}-trades.csv"));

    let out = trades_csv(&rows);

    std::fs::write(&path, out)
        .map_err(|err| CommandError::Failed(format!("writing {}: {err}", path.display())))?;

    // Reveal rather than open: a CSV opened in whatever owns the extension is
    // a spreadsheet nobody asked to launch.
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|err| CommandError::Failed(format!("showing {}: {err}", path.display())))?;
    Ok(path.display().to_string())
}

/// A ledger as CSV.
///
/// Quoting is real, not assumed away. The rows hold timestamps and numbers
/// today, and the moment a strategy name or a note reaches a cell an unquoted
/// writer silently shifts every column after it — the same failure that ate a
/// fund name in the portfolio importer, found only because a file that should
/// have held forty holdings held none.
fn trades_csv(rows: &[TradeRowExport]) -> String {
    let cell = |text: &str| {
        if text.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_owned()
        }
    };
    // An absent number is an empty cell, not a zero: a still-open position has
    // no exit price, and writing 0 there would read as a trade closed at zero.
    let number = |value: Option<f64>| value.map(|v| format!("{v}")).unwrap_or_default();

    let mut out = String::with_capacity(rows.len() * 96 + 128);
    out.push_str(
        "opened,closed,direction,quantity,entry,exit,pnl,commission,held_days,exit_reason\n",
    );
    for row in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            cell(&row.opened),
            cell(&row.closed),
            cell(&row.direction),
            row.quantity,
            row.entry,
            number(row.exit),
            row.pnl,
            row.commission,
            number(row.held_days),
            cell(&row.exit_reason),
        ));
    }
    out
}

/// A row as it comes back from the view.
///
/// Deserialized rather than re-derived from the stored finding: the table
/// exports what is on screen, including whatever sort the reader applied. An
/// export that silently differed from the table above it would be worse than
/// none.
#[derive(serde::Deserialize)]
pub struct TradeRowExport {
    pub opened: String,
    pub closed: String,
    pub direction: String,
    pub quantity: f64,
    pub entry: f64,
    pub exit: Option<f64>,
    pub pnl: f64,
    pub commission: f64,
    pub held_days: Option<f64>,
    pub exit_reason: String,
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
/// The ledger as table rows.
fn trade_rows(ledger: &[arvo_research::Trade]) -> Vec<TradeRowView> {
    ledger
        .iter()
        .map(|trade| TradeRowView {
            opened: trade.opened.format("%Y-%m-%d %H:%M").to_string(),
            closed: trade
                .closed
                .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default(),
            direction: match trade.direction {
                arvo_research::Direction::Long => "long",
                arvo_research::Direction::Short => "short",
            }
            .to_owned(),
            quantity: trade.quantity,
            entry: trade.entry,
            exit: trade.exit,
            pnl: trade.pnl,
            commission: trade.commission,
            held_days: trade
                .holding_period()
                .map(|held| held.num_seconds() as f64 / 86_400.0),
            exit_reason: match trade.exit_reason {
                arvo_research::ExitReason::Signal => "signal",
                arvo_research::ExitReason::Stop => "stop",
                arvo_research::ExitReason::Halted => "halted",
                arvo_research::ExitReason::StillOpen => "open",
            }
            .to_owned(),
        })
        .collect()
}

/// Turns the search surface into something drawable.
///
/// `None` when the grid varies fewer than two parameters — a surface needs two
/// dimensions, and a single axis is a list, which the fold table and the
/// winning-parameters line already say.
///
/// # Choosing which two axes
///
/// The two with the most distinct values, because those are the ones the
/// search actually explored. Any others are *collapsed by taking the best*
/// score over them, and named in [`SurfaceView::collapsed`] so the chart is
/// never mistaken for the whole search. Taking the best rather than the mean
/// is deliberate: this chart answers "was there a good region", and averaging
/// a good configuration together with a bad one on a hidden axis would hide
/// exactly the region being looked for.
///
/// Axes tied on distinct-value count break by name. Arbitrary, and
/// deterministic — a surface that redrew itself differently between runs
/// would be worse than one that picked oddly.
fn surface(selection: &arvo_research::Selection) -> Option<SurfaceView> {
    use std::collections::BTreeMap;

    let mut axes: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for trial in &selection.scored {
        for (name, value) in &trial.params {
            let values = axes.entry(name.clone()).or_default();
            if !values.iter().any(|held| (held - value).abs() < f64::EPSILON) {
                values.push(*value);
            }
        }
    }
    axes.retain(|_, values| values.len() > 1);
    if axes.len() < 2 {
        return None;
    }

    let mut ranked: Vec<(String, Vec<f64>)> = axes.into_iter().collect();
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    let collapsed = ranked
        .iter()
        .skip(2)
        .map(|(name, _)| name.clone())
        .collect();
    let (y_axis, mut y_values) = ranked.remove(1);
    let (x_axis, mut x_values) = ranked.remove(0);
    x_values.sort_by(f64::total_cmp);
    y_values.sort_by(f64::total_cmp);

    let best_params = selection
        .scored
        .iter()
        .max_by(|a, b| a.sharpe.total_cmp(&b.sharpe))
        .map(|trial| trial.params.clone())
        .unwrap_or_default();

    let mut cells: BTreeMap<(String, String), SurfaceCell> = BTreeMap::new();
    for trial in &selection.scored {
        let (Some(x), Some(y)) = (trial.params.get(&x_axis), trial.params.get(&y_axis)) else {
            continue;
        };
        let key = (x.to_string(), y.to_string());
        let selected = trial.params == best_params;
        let cell = cells.entry(key).or_insert(SurfaceCell {
            x: *x,
            y: *y,
            sharpe: f64::NEG_INFINITY,
            selected: false,
            above_null: false,
        });
        // Best over the collapsed axes, not mean. See the note above.
        if trial.sharpe > cell.sharpe {
            cell.sharpe = trial.sharpe;
        }
        cell.selected |= selected;
    }

    let mut cells: Vec<SurfaceCell> = cells.into_values().collect();
    for cell in &mut cells {
        cell.above_null = selection
            .expected_best_under_null
            .is_none_or(|bar| cell.sharpe > bar);
    }

    Some(SurfaceView {
        x_axis,
        y_axis,
        x_values,
        y_values,
        best: selection.best_sharpe,
        null_bar: selection.expected_best_under_null,
        cells,
        collapsed,
    })
}

/// Flattens a study for display.
///
/// Public because the projection *is* what this crate does, and because the
/// test that proves trade markers land on real candles has to run in its own
/// process — a backtest installs Nautilus's logger, and there is a guard test
/// in this crate asserting nothing has claimed that global.
pub fn study_view(
    found: &arvo_research::FamilyEvidence,
    bars: &dyn arvo_data::BarProvider,
    engine: &str,
) -> StudyView {
    let evaluation = &found.out_of_sample_evidence.evaluation;
    StudyView {
        instrument: found.selected.instrument.clone(),
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        trials: found.selection.trials,
        best_sharpe: found.selection.best_sharpe,
        expected_best_under_null: found.selection.expected_best_under_null,
        survived_deflation: found.selection.survived_deflation,
        surface: surface(&found.selection),
        in_sample: format!("{} → {}", found.in_sample.from, found.in_sample.to),
        out_of_sample: format!("{} → {}", found.out_of_sample.from, found.out_of_sample.to),
        selected_params: found
            .selected
            .strategy
            .params
            .iter()
            .map(|(name, value)| (name.clone(), *value))
            .collect(),
        strategy: metrics_view(&evaluation.strategy),
        benchmark: metrics_view(&evaluation.benchmark),
        excess_return: evaluation.excess_return,
        strategy_curve: curve_points(&evaluation.strategy_curve),
        benchmark_curve: curve_points(&evaluation.benchmark_curve),
        price: candles(
            bars,
            &found.selected.instrument,
            found.selected.interval,
            &found.out_of_sample,
        ),
        markers: markers(&evaluation.strategy_ledger, found.selected.interval),
        trades: trade_rows(&evaluation.strategy_ledger),
        data_findings: data_findings(
            bars,
            &found.selected.instrument,
            found.selected.interval,
            &found.out_of_sample,
        ),
        underwater: underwater(&evaluation.strategy_curve),
        monthly: arvo_research::evaluation::monthly_returns(&evaluation.strategy_curve)
            .into_iter()
            .map(|month| MonthlyReturnView {
                year: month.year,
                month: month.month,
                value: month.value,
            })
            .collect(),
        trades_detail: trades_view(
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

/// Flattens a walk-forward run for display. Same reasoning as [`study_view`]:
/// one projection, so a finding read back from memory renders exactly as the
/// run that produced it.
pub fn walk_forward_view(
    found: &arvo_research::WalkForwardEvidence,
    bars: &dyn arvo_data::BarProvider,
    engine: &str,
) -> WalkForwardView {
    let template = &found.template;
    WalkForwardView {
        instrument: template.instrument.clone(),
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        folds: found
            .folds
            .iter()
            .map(|fold| {
                let evaluation = &fold.out_of_sample_evidence.evaluation;
                FoldView {
                    chose_on: format!("{} → {}", fold.in_sample.from, fold.in_sample.to),
                    judged_on: format!("{} → {}", fold.out_of_sample.from, fold.out_of_sample.to),
                    // Only what the grid varied. Carrying the fixed parameters
                    // into every row would bury the one thing this table is
                    // for, which is watching the selection move.
                    params: fold
                        .selected
                        .strategy
                        .params
                        .iter()
                        .filter(|(name, _)| {
                            found.stability.iter().any(|axis| &axis.axis == *name)
                        })
                        .map(|(name, value)| (name.clone(), *value))
                        .collect(),
                    strategy_return: evaluation.strategy.total_return,
                    benchmark_return: evaluation.benchmark.total_return,
                    trades: evaluation.strategy.trades,
                    survived_deflation: fold.selection.survived_deflation,
                }
            })
            .collect(),
        folds_surviving_deflation: found.folds_surviving_deflation,
        folds_without_trades: found.folds_without_trades,
        stability: found
            .stability
            .iter()
            .map(|axis| StabilityView {
                axis: axis.axis.clone(),
                distinct: axis.distinct,
                modal: axis.modal,
                modal_share: axis.modal_share,
            })
            .collect(),
        strategy: metrics_view(&found.combined),
        benchmark: metrics_view(&found.benchmark),
        excess_return: found.excess_return,
        strategy_curve: curve_points(&found.combined_curve),
        benchmark_curve: curve_points(&found.benchmark_curve),
        // Every fold's judged period, end to end — which is the whole span
        // after the first selection window, so the price chart covers exactly
        // what the stitched record covers.
        price: found
            .folds
            .first()
            .zip(found.folds.last())
            .and_then(|(first, last)| {
                DateRange::new(first.out_of_sample.from, last.out_of_sample.to).ok()
            })
            .map(|window| {
                candles(
                    bars,
                    &template.instrument,
                    template.interval,
                    &window,
                )
            })
            .unwrap_or_default(),
        markers: markers(
            &found
                .folds
                .iter()
                .flat_map(|fold| {
                    fold.out_of_sample_evidence
                        .evaluation
                        .strategy_ledger
                        .iter()
                        .cloned()
                })
                .collect::<Vec<_>>(),
            template.interval,
        ),
        underwater: underwater(&found.combined_curve),
        data_findings: found
            .folds
            .first()
            .zip(found.folds.last())
            .and_then(|(first, last)| {
                DateRange::new(first.out_of_sample.from, last.out_of_sample.to).ok()
            })
            .map(|window| {
                data_findings(bars, &template.instrument, template.interval, &window)
            })
            .unwrap_or_default(),
        trades: trade_rows(
            &found
                .folds
                .iter()
                .flat_map(|fold| {
                    fold.out_of_sample_evidence
                        .evaluation
                        .strategy_ledger
                        .iter()
                        .cloned()
                })
                .collect::<Vec<_>>(),
        ),
        trades_detail: trades_view(&found.combined_trades, template.starting_cash),
        in_sample_days: found.in_sample_days,
        step_days: found.step_days,
        anchored: found.anchored,
        dataset_version: template.dataset.version.clone(),
        strategy_name: template.strategy.name.clone(),
        starting_cash: template.starting_cash,
        commission_bps: template.costs.commission_bps,
        slippage_bps: template.costs.slippage_bps,
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
/// How long each selection window looks back, and how often it re-selects.
///
/// Three years to choose on, two to be judged on. The step is two rather than
/// one for a measured reason: every fold's out-of-sample period is an
/// independent backtest that starts cold, so a rule with a 120-bar slow
/// average cannot trade in the first 120 bars of it. At a one-year step that
/// is half the fold, and this grid's slowest configuration then produced no
/// trades at all in nine folds out of sixteen. Two years halves the waste.
///
/// These are a claim, not a setting: a walk-forward run at a different cadence
/// is a different experiment, and the pair is part of what the record pins.
const IN_SAMPLE_DAYS: i64 = 365 * 3;
const STEP_DAYS: i64 = 365 * 2;

/// Builds the rolling procedure for one instrument.
///
/// Same template and grid as [`study_for`], deliberately: the point of a
/// walk-forward is to be comparable with the single split it replaces, and a
/// different grid would make the two incomparable while looking like a
/// stronger result.
#[must_use]
pub fn walk_forward_for(
    instrument: &str,
    plan: &StrategyPlan,
    window: DateRange,
    dataset_version: &str,
) -> arvo_research::WalkForward {
    arvo_research::WalkForward {
        hypothesis: HypothesisId(format!("trend-following predicts returns in {instrument}")),
        template: template_for(instrument, plan, window, dataset_version),
        grid: plan.grid(),
        in_sample_days: IN_SAMPLE_DAYS,
        step_days: STEP_DAYS,
        // Anchored: every selection sees all history. The alternative assumes
        // old data stops applying, which is a claim about the market nobody
        // here has evidence for.
        anchored: true,
    }
}

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
mod csv_tests {
    use super::*;

    fn row(reason: &str) -> TradeRowExport {
        TradeRowExport {
            opened: "2024-01-02 00:00".to_owned(),
            closed: "2024-01-05 00:00".to_owned(),
            direction: "long".to_owned(),
            quantity: 100.0,
            entry: 10.5,
            exit: Some(11.25),
            pnl: 74.0,
            commission: 1.0,
            held_days: Some(3.0),
            exit_reason: reason.to_owned(),
        }
    }

    #[test]
    fn a_header_and_one_line_per_row() {
        let text = trades_csv(&[row("signal"), row("stop")]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("opened,closed,direction"));
        assert_eq!(lines[0].split(',').count(), 10);
        assert_eq!(lines[1].split(',').count(), 10);
    }

    #[test]
    fn a_comma_in_a_cell_does_not_shift_every_column_after_it() {
        // The failure this quoting exists for, and one this codebase has
        // already met: a naive comma split ate a quoted fund name in the
        // portfolio importer and silently produced an empty file.
        let text = trades_csv(&[row("stopped, then re-entered")]);
        let line = text.lines().nth(1).expect("one row");
        assert!(
            line.contains("\"stopped, then re-entered\""),
            "the cell must be quoted: {line}"
        );
    }

    #[test]
    fn a_quote_in_a_cell_is_doubled_rather_than_ending_the_field() {
        let mut awkward = row("signal");
        awkward.direction = "he said \"long\"".to_owned();
        let line = trades_csv(&[awkward]).lines().nth(1).expect("one row").to_owned();
        assert!(line.contains("\"he said \"\"long\"\"\""), "{line}");
    }

    #[test]
    fn an_open_position_writes_an_empty_cell_not_a_zero() {
        // A zero exit price reads as a trade closed at nothing, which is a
        // real-looking number for something that did not happen.
        let mut open = row("open");
        open.closed = String::new();
        open.exit = None;
        open.held_days = None;

        let line = trades_csv(&[open]).lines().nth(1).expect("one row").to_owned();
        let cells: Vec<&str> = line.split(',').collect();
        assert_eq!(cells[1], "", "no close time");
        assert_eq!(cells[5], "", "no exit price");
        assert_eq!(cells[8], "", "no holding period");
    }

    #[test]
    fn an_empty_ledger_is_a_header_and_nothing_else() {
        // Not an empty file: a spreadsheet opening a zero-byte CSV shows an
        // error, and the honest thing to say is "these are the columns, there
        // were no trades".
        let text = trades_csv(&[]);
        assert_eq!(text.lines().count(), 1);
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;
    use arvo_research::{ScoredTrial, Selection};

    fn trial(pairs: &[(&str, f64)], sharpe: f64) -> ScoredTrial {
        ScoredTrial {
            params: pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect(),
            sharpe,
        }
    }

    fn selection(scored: Vec<ScoredTrial>, bar: Option<f64>) -> Selection {
        let best = scored
            .iter()
            .map(|trial| trial.sharpe)
            .fold(f64::NEG_INFINITY, f64::max);
        Selection {
            trials: scored.len(),
            best_sharpe: best,
            expected_best_under_null: bar,
            survived_deflation: bar.is_none_or(|bar| best > bar),
            scored,
        }
    }

    #[test]
    fn a_grid_that_varies_one_thing_has_no_surface_to_draw() {
        // A surface has two dimensions. One axis is a list, and the winning
        // parameters line already says what it would say.
        let scored = vec![
            trial(&[("fast", 5.0), ("trade_size", 100.0)], 0.4),
            trial(&[("fast", 10.0), ("trade_size", 100.0)], 0.6),
        ];
        assert!(surface(&selection(scored, Some(0.5))).is_none());
    }

    #[test]
    fn two_axes_become_the_two_axes() {
        let mut scored = Vec::new();
        for fast in [5.0, 10.0] {
            for slow in [30.0, 60.0, 120.0] {
                scored.push(trial(&[("fast", fast), ("slow", slow)], fast + slow));
            }
        }
        let drawn = surface(&selection(scored, Some(1.0))).expect("two axes");

        // The one with more distinct values goes on x, so the grid is wider
        // than it is tall rather than the other way round.
        assert_eq!(drawn.x_axis, "slow");
        assert_eq!(drawn.y_axis, "fast");
        assert_eq!(drawn.x_values, vec![30.0, 60.0, 120.0]);
        assert_eq!(drawn.y_values, vec![5.0, 10.0]);
        assert_eq!(drawn.cells.len(), 6);
        assert!(drawn.collapsed.is_empty());
    }

    #[test]
    fn a_third_axis_is_collapsed_by_taking_the_best_and_is_named() {
        // Averaging over a hidden axis would blend a good configuration with a
        // bad one and hide exactly the region this chart is drawn to find.
        // Saying which axis was collapsed is what stops the picture being read
        // as the whole search.
        // Distinct counts, no ties: `fast` explores four values, `slow`
        // three, `atr` two. So `fast` and `slow` are the axes the search
        // actually explored and the ones drawn, and `atr` is collapsed.
        let mut scored = Vec::new();
        for fast in [5.0, 10.0, 15.0, 20.0] {
            for slow in [30.0, 60.0, 120.0] {
                for atr in [1.0, 2.0] {
                    // One standout, hidden on the collapsed axis.
                    let sharpe = if fast == 5.0 && slow == 30.0 && atr == 2.0 {
                        1.8
                    } else {
                        0.3
                    };
                    scored.push(trial(
                        &[("fast", fast), ("slow", slow), ("atr", atr)],
                        sharpe,
                    ));
                }
            }
        }
        let drawn = surface(&selection(scored, Some(1.0))).expect("three axes");

        assert_eq!(drawn.x_axis, "fast", "the most-explored axis goes across");
        assert_eq!(drawn.y_axis, "slow");
        assert_eq!(drawn.collapsed, vec!["atr".to_owned()]);
        assert_eq!(drawn.cells.len(), 12, "one cell per drawn pair, not per trial");

        let corner = drawn
            .cells
            .iter()
            .find(|cell| {
                (cell.x - 5.0).abs() < f64::EPSILON && (cell.y - 30.0).abs() < f64::EPSILON
            })
            .expect("fast 5, slow 30");
        assert!(
            (corner.sharpe - 1.8).abs() < 1e-9,
            "the best over the collapsed axis, not the mean: {}",
            corner.sharpe
        );
    }

    #[test]
    fn a_cell_below_the_no_skill_bar_is_marked_as_not_a_result() {
        // The distinction the whole chart is drawn around. A score a
        // coin-flipping search of this size would have been expected to reach
        // anyway is not a weak finding, it is not a finding.
        let scored = vec![
            trial(&[("fast", 5.0), ("slow", 30.0)], 0.9),
            trial(&[("fast", 5.0), ("slow", 60.0)], 1.4),
            trial(&[("fast", 10.0), ("slow", 30.0)], 0.2),
            trial(&[("fast", 10.0), ("slow", 60.0)], 0.5),
        ];
        let drawn = surface(&selection(scored, Some(1.0))).expect("two axes");

        assert_eq!(
            drawn.cells.iter().filter(|cell| cell.above_null).count(),
            1,
            "only the 1.4 clears a bar of 1.0"
        );
        let chosen: Vec<_> = drawn.cells.iter().filter(|cell| cell.selected).collect();
        assert_eq!(chosen.len(), 1);
        assert!((chosen[0].sharpe - 1.4).abs() < 1e-9);
    }

    #[test]
    fn with_no_bar_to_clear_nothing_is_claimed_to_have_cleared_it() {
        // Too few trials to say what a no-skill search would produce. Marking
        // everything as beating a bar that was never computed would be the
        // most flattering possible default.
        let scored = vec![
            trial(&[("fast", 5.0), ("slow", 30.0)], 0.9),
            trial(&[("fast", 10.0), ("slow", 60.0)], 1.4),
        ];
        let drawn = surface(&selection(scored, None)).expect("two axes");
        assert_eq!(drawn.null_bar, None);
        assert!(
            drawn.cells.iter().all(|cell| cell.above_null),
            "with no bar there is nothing to fail, and the UI shades none of it"
        );
    }

    #[test]
    fn a_configuration_that_never_ran_leaves_a_hole_not_a_zero() {
        // It did not score badly; it did not score. A zero would be drawn as
        // a real, poor result.
        let scored = vec![
            trial(&[("fast", 5.0), ("slow", 30.0)], 0.9),
            trial(&[("fast", 5.0), ("slow", 60.0)], 1.4),
            trial(&[("fast", 10.0), ("slow", 30.0)], 0.2),
        ];
        let drawn = surface(&selection(scored, Some(1.0))).expect("two axes");
        assert_eq!(drawn.x_values.len() * drawn.y_values.len(), 4);
        assert_eq!(drawn.cells.len(), 3, "the fourth pair is absent, not zero");
    }
}

#[cfg(test)]
mod chart_tests {
    use super::*;
    use arvo_research::{Direction, ExitReason, Trade};

    fn at(day: u32, hour: u32, minute: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_hms_opt(hour, minute, 0)
            .expect("valid")
    }

    fn trade(opened: chrono::NaiveDateTime, closed: Option<chrono::NaiveDateTime>) -> Trade {
        Trade {
            opened,
            closed,
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: closed.map(|_| 110.0),
            pnl: 100.0,
            commission: 1.0,
            exit_reason: closed.map_or(ExitReason::StillOpen, |_| ExitReason::Stop),
        }
    }

    #[test]
    fn a_marker_lands_on_the_bar_that_caused_it_not_the_one_after() {
        // The off-by-one this shift exists for. A fill is stamped at the close
        // of the bar the signal was read from; a candle is stamped at its
        // open. Without the shift every entry appears one bar late, and the
        // chart renders perfectly while saying something false.
        let interval = arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);
        let bar_opens = at(2, 9, 30);
        let filled_at_its_close = at(2, 9, 35);

        let out = markers(&[trade(filled_at_its_close, None)], interval);
        assert_eq!(out[0].time, bar_opens.and_utc().timestamp());
    }

    #[test]
    fn a_daily_marker_lands_on_its_own_day() {
        let out = markers(&[trade(at(3, 0, 0), None)], arvo_data::BarInterval::DAILY);
        assert_eq!(out[0].time, at(2, 0, 0).and_utc().timestamp());
    }

    #[test]
    fn a_closed_trade_yields_two_markers_and_an_open_one_yields_one() {
        let interval = arvo_data::BarInterval::DAILY;
        let out = markers(
            &[
                trade(at(2, 0, 0), Some(at(4, 0, 0))),
                trade(at(6, 0, 0), None),
            ],
            interval,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().filter(|m| m.kind == "entry").count(), 2);
        assert_eq!(out.iter().filter(|m| m.kind == "exit").count(), 1);
    }

    #[test]
    fn markers_come_out_in_time_order() {
        // Not tidiness: the chart library throws on unsorted markers, and an
        // exception crossing back into wasm takes the calling future with it.
        let interval = arvo_data::BarInterval::DAILY;
        let out = markers(
            &[
                trade(at(8, 0, 0), Some(at(9, 0, 0))),
                trade(at(2, 0, 0), Some(at(3, 0, 0))),
            ],
            interval,
        );
        assert!(out.windows(2).all(|pair| pair[0].time <= pair[1].time));
    }

    #[test]
    fn a_stop_exit_is_marked_differently_from_a_signal_exit() {
        let out = markers(
            &[trade(at(2, 0, 0), Some(at(4, 0, 0)))],
            arvo_data::BarInterval::DAILY,
        );
        let exit = out.iter().find(|m| m.kind == "exit").expect("closed");
        assert_eq!(exit.reason, "stop");
    }

    fn point(day: u32, equity: f64) -> arvo_research::EquityPoint {
        arvo_research::EquityPoint {
            at: at(day, 0, 0),
            equity,
        }
    }

    #[test]
    fn underwater_is_depth_below_the_running_peak() {
        let curve = [
            point(1, 100.0),
            point(2, 120.0),
            point(3, 90.0),
            point(4, 120.0),
        ];
        let plot = underwater(&curve);
        assert!((plot[0].value - 0.0).abs() < 1e-9, "a new peak is the surface");
        assert!((plot[1].value - 0.0).abs() < 1e-9);
        // 90 against a peak of 120 is 25% down.
        assert!((plot[2].value + 25.0).abs() < 1e-9, "{:?}", plot[2].value);
        assert!((plot[3].value - 0.0).abs() < 1e-9, "back to the peak");
        assert!(
            plot.iter().all(|p| p.value <= 0.0),
            "the plot hangs below zero, always"
        );
    }

    #[test]
    fn an_intraday_curve_keeps_every_point_rather_than_one_a_day() {
        // The previous version keyed points by date and deduplicated, which
        // silently threw away all but the last point of each day — an
        // intraday curve of 780 bars became nine points.
        let curve: Vec<_> = (0..12)
            .map(|index| arvo_research::EquityPoint {
                at: at(2, 9, 30) + chrono::Duration::minutes(5 * index),
                equity: 100.0 + index as f64,
            })
            .collect();
        assert_eq!(curve_points(&curve).len(), 12);
    }
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
