//! The shapes the backend sends.
//!
//! One module for all of them because they are a *contract*, not a feature:
//! every one mirrors a `Serialize` struct in `arvo-runtime`, and the two have
//! to be read side by side whenever either changes. Scattering them next to
//! the components that render them would mean checking six files against one.
//!
//! Everything here is `Deserialize` and nothing here has behaviour. A method
//! on one of these would be a place for the UI and the backend to disagree
//! about what a number means.

use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(tag = "state")]
pub(crate) enum PluginStatusView {
    Reachable {
        name: String,
        version: String,
        capabilities: Vec<String>,
    },
    Unreachable {
        reason: String,
    },
}

#[derive(Clone, Deserialize)]
pub(crate) struct PluginView {
    pub(crate) id: String,
    pub(crate) address: Option<String>,
    pub(crate) status: PluginStatusView,
}

#[derive(Clone, Deserialize)]
pub(crate) struct MetricsView {
    pub(crate) total_return: f64,
    pub(crate) cagr: f64,
    pub(crate) max_drawdown: f64,
    pub(crate) volatility: f64,
    pub(crate) sharpe: Option<f64>,
    pub(crate) sortino: Option<f64>,
    pub(crate) calmar: Option<f64>,
    pub(crate) trades: u32,
}

#[derive(Clone, Deserialize)]
pub(crate) struct MonthlyReturnView {
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) value: f64,
}

#[derive(Clone, Deserialize)]
pub(crate) struct InstrumentView {
    pub(crate) id: String,
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
    pub(crate) bars: usize,
    pub(crate) fingerprint: Option<String>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct DataLibraryView {
    pub(crate) directory: String,
    pub(crate) instruments: Vec<InstrumentView>,
}

/// Mirrors `arvo_runtime::research::StudyView`. Nothing here names an engine,
/// a broker or an order — the workbench works in research concepts only.
#[derive(Clone, Deserialize)]
pub(crate) struct StudyView {
    pub(crate) instrument: String,
    pub(crate) verdict: String,
    pub(crate) reasons: Vec<String>,
    pub(crate) trials: usize,
    pub(crate) best_sharpe: f64,
    pub(crate) expected_best_under_null: Option<f64>,
    pub(crate) survived_deflation: bool,
    pub(crate) surface: Option<SurfaceView>,
    pub(crate) in_sample: String,
    pub(crate) out_of_sample: String,
    pub(crate) selected_params: Vec<(String, f64)>,
    pub(crate) strategy: MetricsView,
    pub(crate) benchmark: MetricsView,
    pub(crate) excess_return: f64,
    pub(crate) strategy_curve: Vec<CurvePoint>,
    pub(crate) benchmark_curve: Vec<CurvePoint>,
    pub(crate) price: Vec<CandlePoint>,
    pub(crate) markers: Vec<TradeMarkerView>,
    pub(crate) underwater: Vec<CurvePoint>,
    pub(crate) trades: Vec<TradeRowView>,
    pub(crate) monthly: Vec<MonthlyReturnView>,
    pub(crate) trades_detail: TradesView,
    pub(crate) recommendations: Vec<RecommendationView>,
    pub(crate) dataset_version: String,
    pub(crate) strategy_name: String,
    pub(crate) starting_cash: f64,
    pub(crate) commission_bps: f64,
    pub(crate) slippage_bps: f64,
    pub(crate) engine: String,
}

/// One fold of a walk-forward.
#[derive(Clone, Deserialize)]
pub(crate) struct FoldView {
    pub(crate) chose_on: String,
    pub(crate) judged_on: String,
    pub(crate) params: Vec<(String, f64)>,
    pub(crate) strategy_return: f64,
    pub(crate) benchmark_return: f64,
    pub(crate) trades: u32,
    pub(crate) survived_deflation: bool,
}

/// How much one parameter moved across the folds.
#[derive(Clone, Deserialize)]
pub(crate) struct StabilityView {
    pub(crate) axis: String,
    pub(crate) distinct: usize,
    pub(crate) modal: f64,
    pub(crate) modal_share: f64,
}

/// A rolling re-selection run.
#[derive(Clone, Deserialize)]
pub(crate) struct WalkForwardView {
    pub(crate) instrument: String,
    pub(crate) verdict: String,
    pub(crate) reasons: Vec<String>,
    pub(crate) folds: Vec<FoldView>,
    pub(crate) folds_surviving_deflation: usize,
    pub(crate) folds_without_trades: usize,
    pub(crate) stability: Vec<StabilityView>,
    pub(crate) strategy: MetricsView,
    pub(crate) benchmark: MetricsView,
    pub(crate) excess_return: f64,
    pub(crate) strategy_curve: Vec<CurvePoint>,
    pub(crate) benchmark_curve: Vec<CurvePoint>,
    pub(crate) price: Vec<CandlePoint>,
    pub(crate) markers: Vec<TradeMarkerView>,
    pub(crate) underwater: Vec<CurvePoint>,
    pub(crate) trades: Vec<TradeRowView>,
    pub(crate) trades_detail: TradesView,
    pub(crate) in_sample_days: i64,
    pub(crate) step_days: i64,
    pub(crate) anchored: bool,
    pub(crate) dataset_version: String,
    pub(crate) strategy_name: String,
    pub(crate) starting_cash: f64,
    pub(crate) commission_bps: f64,
    pub(crate) slippage_bps: f64,
    pub(crate) engine: String,
}

/// What a fetch pulled in.
#[derive(Clone, Deserialize)]
pub(crate) struct FetchView {
    pub(crate) instrument: String,
    pub(crate) interval: String,
    pub(crate) bars: usize,
    pub(crate) interpolated: usize,
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
}

/// A strategy the workbench can run.
#[derive(Clone, Deserialize)]
pub(crate) struct StrategyView {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) premise: String,
    pub(crate) interval: String,
    pub(crate) backtests: usize,
}

/// One thing to do about a finding.
#[derive(Clone, Deserialize)]
pub(crate) struct RecommendationView {
    pub(crate) severity: String,
    pub(crate) finding: String,
    pub(crate) action: String,
    pub(crate) evidence: String,
}

/// The round trips behind a return, and what they cost.
#[derive(Clone, Deserialize)]
pub(crate) struct TradesView {
    pub(crate) closed: u32,
    pub(crate) still_open: u32,
    pub(crate) win_rate: Option<f64>,
    pub(crate) profit_factor: Option<f64>,
    pub(crate) expectancy: Option<f64>,
    pub(crate) average_win: Option<f64>,
    pub(crate) average_loss: Option<f64>,
    pub(crate) average_holding_days: Option<f64>,
    pub(crate) fees_paid: f64,
    pub(crate) fees_fraction: f64,
    pub(crate) signal_exits: u32,
    pub(crate) stop_exits: u32,
}

/// The workspace as it was left.
///
/// `Serialize` too: this goes back out on every layout change. Every field is
/// optional so a session written by an older build still opens, as a session
/// that knows less rather than as a failure that costs the layout.
#[derive(Clone, Default, Deserialize, serde::Serialize)]
pub(crate) struct SessionView {
    /// dockview's own serialisation, as text. See `arvo_runtime::session`
    /// for why it is not a structured value.
    pub(crate) layout: Option<String>,
    pub(crate) active_view: Option<String>,
    pub(crate) output_visible: bool,
    pub(crate) theme: Option<String>,
    pub(crate) strategy: Option<String>,
}

/// One round trip, as a table row.
///
/// `Serialize` as well as `Deserialize`: the export sends back exactly the
/// rows on screen, in the order the reader sorted them. An export that
/// silently differed from the table above it would be worse than none.
#[derive(Clone, Deserialize, serde::Serialize)]
pub(crate) struct TradeRowView {
    pub(crate) opened: String,
    pub(crate) closed: String,
    pub(crate) direction: String,
    pub(crate) quantity: f64,
    pub(crate) entry: f64,
    pub(crate) exit: Option<f64>,
    pub(crate) pnl: f64,
    pub(crate) commission: f64,
    pub(crate) held_days: Option<f64>,
    pub(crate) exit_reason: String,
}

/// One configuration's cell on the search surface.
#[derive(Clone, Deserialize)]
pub(crate) struct SurfaceCell {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) sharpe: f64,
    pub(crate) selected: bool,
    pub(crate) above_null: bool,
}

/// The in-sample score of every configuration the search tried.
#[derive(Clone, Deserialize)]
pub(crate) struct SurfaceView {
    pub(crate) x_axis: String,
    pub(crate) y_axis: String,
    pub(crate) x_values: Vec<f64>,
    pub(crate) y_values: Vec<f64>,
    pub(crate) cells: Vec<SurfaceCell>,
    pub(crate) null_bar: Option<f64>,
    pub(crate) best: f64,
    pub(crate) collapsed: Vec<String>,
}

/// One bar, as the chart wants it.
#[derive(Clone, Deserialize, serde::Serialize)]
pub(crate) struct CandlePoint {
    pub(crate) time: i64,
    pub(crate) open: f64,
    pub(crate) high: f64,
    pub(crate) low: f64,
    pub(crate) close: f64,
}

/// Where a trade happened, to be drawn on the price.
#[derive(Clone, Deserialize, serde::Serialize)]
pub(crate) struct TradeMarkerView {
    pub(crate) time: i64,
    pub(crate) kind: String,
    pub(crate) reason: String,
    pub(crate) label: String,
}

#[derive(Clone, Deserialize, serde::Serialize)]
pub(crate) struct CurvePoint {
    pub(crate) time: String,
    pub(crate) value: f64,
}

#[derive(Clone, Deserialize)]
pub(crate) struct OutcomeView {
    pub(crate) instrument: String,
    pub(crate) strategy_return: f64,
    pub(crate) benchmark_return: f64,
    pub(crate) excess_return: f64,
    pub(crate) max_drawdown: f64,
    pub(crate) trades: u32,
}

/// Mirrors `arvo_runtime::research::PanelView`.
#[derive(Clone, Deserialize)]
pub(crate) struct PanelView {
    pub(crate) verdict: String,
    pub(crate) reasons: Vec<String>,
    pub(crate) instruments: usize,
    pub(crate) total_trades: u32,
    pub(crate) mean_excess_return: f64,
    pub(crate) beat_benchmark: usize,
    pub(crate) mean_max_drawdown: f64,
    pub(crate) worst_max_drawdown: f64,
    pub(crate) trials: usize,
    pub(crate) best_sharpe: f64,
    pub(crate) expected_best_under_null: Option<f64>,
    pub(crate) survived_deflation: bool,
    pub(crate) in_sample: String,
    pub(crate) out_of_sample: String,
    pub(crate) selected_params: Vec<(String, f64)>,
    pub(crate) per_instrument: Vec<OutcomeView>,
    pub(crate) failures: Vec<String>,
    pub(crate) dataset_version: String,
    pub(crate) strategy_name: String,
    pub(crate) starting_cash: f64,
    pub(crate) commission_bps: f64,
    pub(crate) slippage_bps: f64,
    pub(crate) engine: String,
}

#[derive(Clone, Deserialize)]
pub(crate) struct HoldingView {
    pub(crate) instrument: String,
    pub(crate) quantity: Option<f64>,
    pub(crate) price: Option<f64>,
    pub(crate) market_value: f64,
    pub(crate) cost_basis: Option<f64>,
    pub(crate) unrealized: Option<f64>,
    pub(crate) unrealized_pct: Option<f64>,
    pub(crate) weight: f64,
    pub(crate) priced_by: String,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ImportView {
    pub(crate) columns: Vec<(String, String)>,
    pub(crate) ignored: Vec<String>,
    pub(crate) rows_imported: usize,
    pub(crate) rows_skipped: Vec<String>,
    pub(crate) cost_basis_derived: bool,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ValuePoint {
    pub(crate) time: String,
    pub(crate) value: f64,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ChangeView {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) absolute: f64,
    pub(crate) percent: Option<f64>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct PortfolioView {
    pub(crate) name: String,
    pub(crate) as_of: String,
    pub(crate) total_value: f64,
    pub(crate) total_cost: Option<f64>,
    pub(crate) unrealized: Option<f64>,
    pub(crate) unrealized_pct: Option<f64>,
    pub(crate) without_cost_basis: usize,
    pub(crate) cash: f64,
    pub(crate) holdings: Vec<HoldingView>,
    pub(crate) unpriced: Vec<String>,
    pub(crate) value_history: Vec<ValuePoint>,
    pub(crate) change: Option<ChangeView>,
    pub(crate) import: ImportView,
}

#[derive(Clone, Deserialize)]
pub(crate) struct PortfolioLibraryView {
    pub(crate) directory: String,
    pub(crate) portfolios: Vec<PortfolioView>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct HistoryEntryView {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) subject: String,
    pub(crate) verdict: String,
    pub(crate) recorded_at: String,
    pub(crate) stale: Option<bool>,
}

/// Mirrors `arvo_runtime::research::RecordView`.
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RecordView {
    // Both boxed: each carries curves and tables, so an unboxed enum would
    // size every record to whichever view is currently the larger.
    Study(Box<StudyView>),
    Panel(Box<PanelView>),
    WalkForward(Box<WalkForwardView>),
}
