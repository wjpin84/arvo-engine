//! The shapes the backend and the window agree on.
//!
//! # Why this is a crate and not two mirrored files
//!
//! It used to be two. `arvo-runtime` defined these with `Serialize`, the
//! editor redefined all twenty-six of them by hand with `Deserialize`, and
//! nothing checked that the two agreed — a mismatch is invisible to the
//! compiler and appears at runtime as a deserialisation error, if it appears
//! at all.
//!
//! It cost two bugs in one afternoon. `CurvePoint.time` became `i64` on one
//! side and stayed `String` on the other, which broke every study the app
//! could run; and the editor never declared `WalkForwardView.recommendations`
//! at all, so serde quietly ignored the field and it could never have been
//! shown. The second was found only by writing a throwaway script to diff the
//! two files, which is not a thing anyone will remember to do again.
//!
//! One definition removes the class. That is the whole argument.
//!
//! # What belongs here
//!
//! Data, and nothing else. No behaviour, no dependency on the research
//! domain, no knowledge of Tauri or the DOM. A method here would be a place
//! for the two sides to start disagreeing about what a number means, and a
//! dependency here is one the WebAssembly build has to carry.
//!
//! Conversions from domain types live in `arvo-runtime`, as free functions
//! rather than `From` impls — with the types here and the domain there,
//! neither is local to that crate and the orphan rule forbids it. That is the
//! rule doing its job.

use serde::{Deserialize, Serialize};

/// What the workbench shows before anything has been run.
/// Everything in research memory, and what could not be read of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryView {
    pub entries: Vec<HistoryEntryView>,
    pub unreadable: Vec<UnreadableView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataLibraryView {
    /// Shown so a user with no data knows where to put some.
    pub directory: String,
    pub instruments: Vec<InstrumentView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonthlyReturnView {
    pub year: i32,
    pub month: u32,
    pub value: f64,
}

/// One point on a curve, in the shape a chart library wants: an ISO date and
/// a value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurvePoint {
    /// Seconds since the epoch, not a date string.
    ///
    /// A date was enough while every curve was daily and is not once two
    /// points can share a day: the chart keys points by time, so an intraday
    /// series collapsed to dates loses every point but the last of each day.
    /// The previous version did exactly that, deliberately and with a comment
    /// saying so — this is what removing the limitation instead looks like.
    pub time: i64,
    pub value: f64,
}

/// One bar, as the chart wants it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandlePoint {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Where a trade happened, to be drawn on the price.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeMarkerView {
    pub time: i64,
    /// `entry` or `exit`; the chart decides shape and side from it.
    pub kind: String,
    /// `stop` or `signal` for an exit, so a stop-out is visually distinct
    /// from a rule that chose to leave. Empty for an entry.
    pub reason: String,
    pub label: String,
}

/// One thing to do about a finding, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The full result of a study, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyView {
    pub instrument: String,
    pub verdict: String,
    pub reasons: Vec<String>,

    // How hard the search was, and what that costs in credibility.
    pub trials: usize,
    pub best_sharpe: f64,
    pub expected_best_under_null: Option<f64>,
    pub survived_deflation: bool,
    /// Every configuration the search tried, not only the one it picked.
    pub surface: Option<SurfaceView>,

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
    /// The instrument's own bars over the judged period, with every entry and
    /// exit marked on them.
    ///
    /// The view that shows what the rule *did* rather than what it added up
    /// to. A summary cannot say the entries all landed on three days, or that
    /// every winner came out of one gap; this says it at a glance.
    pub price: Vec<CandlePoint>,
    pub markers: Vec<TradeMarkerView>,
    /// Depth below the running peak, as a percentage.
    pub underwater: Vec<CurvePoint>,
    /// What is wrong with the bars this was produced from.
    pub data_findings: Vec<DataFindingView>,
    /// Every round trip, so nobody has to take the summary on trust.
    pub trades: Vec<TradeRowView>,
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

/// One fold of a walk-forward, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldView {
    pub chose_on: String,
    pub judged_on: String,
    pub params: Vec<(String, f64)>,
    pub strategy_return: f64,
    pub benchmark_return: f64,
    pub trades: u32,
    /// Whether this fold's winner beat what a no-skill search of that size
    /// would produce. Per fold, because a procedure that selects noise in most
    /// periods has not been shown to select.
    pub survived_deflation: bool,
}

/// How much one parameter moved across the folds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StabilityView {
    pub axis: String,
    pub distinct: usize,
    pub modal: f64,
    pub modal_share: f64,
}

/// A walk-forward run, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardView {
    pub instrument: String,
    pub verdict: String,
    pub reasons: Vec<String>,

    pub folds: Vec<FoldView>,
    pub folds_surviving_deflation: usize,
    /// Folds in which the selected configuration never opened a position — the
    /// symptom of a step too short for the rule's warm-up.
    pub folds_without_trades: usize,
    /// How the selection moved. The thing only a rolling procedure can show.
    pub stability: Vec<StabilityView>,

    // The stitched out-of-sample record.
    pub strategy: MetricsView,
    pub benchmark: MetricsView,
    pub excess_return: f64,
    pub strategy_curve: Vec<CurvePoint>,
    pub benchmark_curve: Vec<CurvePoint>,
    pub price: Vec<CandlePoint>,
    pub markers: Vec<TradeMarkerView>,
    pub underwater: Vec<CurvePoint>,
    /// What is wrong with the bars this was produced from.
    pub data_findings: Vec<DataFindingView>,
    pub trades: Vec<TradeRowView>,
    pub trades_detail: TradesView,

    pub in_sample_days: i64,
    pub step_days: i64,
    pub anchored: bool,
    pub dataset_version: String,
    pub strategy_name: String,
    pub starting_cash: f64,
    pub commission_bps: f64,
    pub slippage_bps: f64,
    pub engine: String,
}

/// One round trip, as a table row.
///
/// Every field the ledger holds, because the point of a table is that nobody
/// has to decide in advance which column someone will want to sort by. The
/// aggregate statistics above it answer "how did it do"; this answers "what
/// did it actually do", and those are different questions with different
/// failure modes — an expectancy of +£300 built from one +£9,000 trade and
/// nineteen losses is a fact only the rows show.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRowView {
    pub opened: String,
    /// Empty while the position is still open at the end of the run.
    pub closed: String,
    pub direction: String,
    pub quantity: f64,
    pub entry: f64,
    /// `None` while still open, so the table shows a gap rather than a price
    /// nobody traded at.
    pub exit: Option<f64>,
    pub pnl: f64,
    pub commission: f64,
    /// Days held. Fractional, because an intraday trade held forty minutes is
    /// not "0 days" — it is 0.03, and rounding it away would make every
    /// intraday ledger look like a column of zeroes.
    pub held_days: Option<f64>,
    /// `signal`, `stop`, or `open`.
    pub exit_reason: String,
}

/// One configuration's cell on the search surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceCell {
    pub x: f64,
    pub y: f64,
    pub sharpe: f64,
    /// Whether this is the configuration that was chosen.
    pub selected: bool,
    /// Whether it beat what a no-skill search of this size would produce.
    ///
    /// The distinction the whole chart is drawn around. A cell below the bar
    /// is not a weak result, it is *not a result* — a score a coin-flipping
    /// search of the same size would have been expected to reach anyway.
    pub above_null: bool,
}

/// The in-sample score of every configuration the search tried.
///
/// # Why this exists at all
///
/// Reporting only the winner shows two completely different situations
/// identically: a broad region of configurations that all scored well, which
/// suggests something real and robust to the exact parameters; and one bright
/// cell surrounded by nothing, which is what fitting noise looks like from
/// above. The verdict machinery already deflates for the *size* of the search;
/// this is the part a person has to look at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceView {
    /// The two axes drawn, by name.
    pub x_axis: String,
    pub y_axis: String,
    pub x_values: Vec<f64>,
    pub y_values: Vec<f64>,
    pub cells: Vec<SurfaceCell>,
    /// The bar a cell has to clear to be worth anything. `None` when there
    /// were too few trials to say.
    pub null_bar: Option<f64>,
    pub best: f64,
    /// Axes not drawn, because a surface has two dimensions and a grid may
    /// have more. Named so nobody reads the chart as the whole search.
    pub collapsed: Vec<String>,
}

/// A stored finding, summarised for the history list.
/// Something wrong with the price series a result was produced from.
///
/// Shown beside the verdict rather than in a data screen nobody opens. A
/// verdict is only as good as the bars under it, and a backtest cannot tell an
/// unadjusted split from a crash — it will trade both and report a number
/// either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataFindingView {
    /// `fault` for something that cannot legitimately be true of a price
    /// series, `suspect` for something merely unusual.
    pub severity: String,
    pub kind: String,
    pub at: Option<String>,
    pub detail: String,
}

/// A finding that could not be read, and why.
///
/// Shown rather than logged. Four findings were once lost to a field rename
/// and the only trace was a warning nobody had reason to look at — a research
/// store that quietly forgets things is worse than one that says it has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnreadableView {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordView {
    Study(Box<StudyView>),
    Panel(Box<PanelView>),
    WalkForward(Box<WalkForwardView>),
}

/// One instrument's out-of-sample outcome under the panel's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeView {
    pub instrument: String,
    pub strategy_return: f64,
    pub benchmark_return: f64,
    pub excess_return: f64,
    pub max_drawdown: f64,
    pub trades: u32,
}

/// A panel study, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// One instrument the broker knows about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchView {
    /// The Arvo id it would be filed under, ready to fetch.
    pub instrument: String,
    pub symbol: String,
    pub name: String,
    pub price: Option<f64>,
    /// Move since the previous close, as a fraction.
    pub change: Option<f64>,
    /// Whether the data library already holds this instrument.
    pub held: bool,
}

/// What a fetch pulled in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchView {
    pub instrument: String,
    pub interval: String,
    pub bars: usize,
    /// Gap-fill bars the server synthesised, which were dropped. Surfaced
    /// rather than hidden: a series that is a quarter invented is one to know
    /// about before drawing a conclusion from it.
    pub interpolated: usize,
    pub from: Option<String>,
    pub to: Option<String>,
    pub data_findings: Vec<DataFindingView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldingView {
    pub instrument: String,
    /// `None` when the source reported money without units — a collective
    /// trust in a 401(k) does exactly that.
    pub quantity: Option<f64>,
    pub price: Option<f64>,
    pub market_value: f64,
    pub cost_basis: Option<f64>,
    pub unrealized: Option<f64>,
    pub unrealized_pct: Option<f64>,
    pub weight: f64,
    /// "statement", "last_close" or "face" — so a price nobody verified is
    /// visibly different from one that came off a statement.
    pub priced_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioView {
    pub name: String,
    pub as_of: String,
    pub total_value: f64,
    pub total_cost: Option<f64>,
    pub unrealized: Option<f64>,
    pub unrealized_pct: Option<f64>,
    /// How many holdings reported no cost basis. Normal for a 401(k).
    pub without_cost_basis: usize,
    pub cash: f64,
    pub holdings: Vec<HoldingView>,
    pub unpriced: Vec<String>,
    /// Value on every day this portfolio has been looked at. A holdings file
    /// says what you hold now; almost everything interesting is a change, and
    /// a single export cannot express one.
    pub value_history: Vec<ValuePoint>,
    /// `None` until a portfolio has been valued on two different days.
    pub change: Option<ChangeView>,
    /// How the file was read. Shown, not hidden: an importer that guessed a
    /// column wrong produces a portfolio that looks entirely plausible, and
    /// this is the only thing that would reveal it.
    pub import: ImportView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportView {
    /// Role → the column heading used for it.
    pub columns: Vec<(String, String)>,
    pub ignored: Vec<String>,
    pub rows_imported: usize,
    pub rows_skipped: Vec<String>,
    /// Cost basis came from a per-share column multiplied by quantity.
    pub cost_basis_derived: bool,
}

/// One day on the value line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValuePoint {
    /// Seconds since the epoch, the same convention every other chart series
    /// uses. One convention rather than two: the portfolio chart reuses the
    /// research chart component, and a date string here against epoch seconds
    /// there is a mismatch the compiler cannot see and the reader meets as a
    /// deserialisation error at runtime.
    pub time: i64,
    pub value: f64,
}

/// The move between the two most recent valuations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeView {
    pub from: String,
    pub to: String,
    pub absolute: f64,
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioLibraryView {
    /// Shown so somebody with no holdings file knows where to put one.
    pub directory: String,
    pub portfolios: Vec<PortfolioView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginView {
    pub id: String,
    pub address: Option<String>,
    pub status: PluginStatusView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum PluginStatusView {
    Reachable {
        name: String,
        version: String,
        capabilities: Vec<String>,
    },
    Unreachable {
        reason: String,
    },
}
