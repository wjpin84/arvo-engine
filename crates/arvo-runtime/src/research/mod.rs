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
//!
//! # Why this is a directory
//!
//! It was one 3,279-line file holding four unrelated jobs: the commands the
//! window calls, the mapping from engine types to view types, the research
//! runs themselves, and the data-source plumbing. They changed for different
//! reasons and every change to any of them touched the same file.
//!
//! The split that matters is [`data`]. Fetching used to name one vendor in the
//! body of `fetch_bars`, which is why a second source existed and could not be
//! reached; it now takes a [`crate::source::Source`] like any other parameter.

pub mod data;
pub mod history;
pub mod study;
pub mod views;

// Re-exported because the integration tests in `tests/` reach them through
// this module, and because `study_view` and friends read as research
// vocabulary rather than as "the views submodule's business".
pub use study::{list_strategies, panel_for, study_for, walk_forward_for};
pub use views::{metrics_view, study_view, trades_view, walk_forward_view};

// Helpers the submodules share. These were mutually visible when all four jobs
// lived in one file; re-exporting through the parent is what keeps that true
// without making them part of the crate's outward surface. `use super::*` in a
// submodule reaches the parent, not a sibling.
pub(crate) use study::panel_dataset_version;
pub(crate) use views::{curve_points, panel_view, verdict_label};

use std::collections::BTreeSet;
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
//
// `pub use`, not `use`: these are re-exported as this module's own surface
// and `tests/chart_alignment.rs` reaches them through it.
pub use arvo_views::{
    BookView, BreadthView, CandlePoint, ComparisonRowView, ComparisonView, CurvePoint,
    DataFindingView, DataLibraryView, DivergenceView, FetchView, FoldView, HistoryEntryView,
    HistoryView, InstrumentView, MatchView, MemberView, MetricsView, MonthlyReturnView, NamedCurveView,
    InstrumentChartView, OutcomeView, PanelView, QuoteView, RecommendationView, RecordView, ReplayView, StabilityView,
    SourceComparisonView, SourceView, StrategyView, StudyView, SurfaceCell, SurfaceView,
    TradeMarkerView, TradeRowView,
    TradesView, UnreadableView, WalkForwardView,
};

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
        name: "cross_sectional_momentum",
        label: "Cross-sectional momentum",
        premise: "Hold the few that rose most, and nothing else. Ranks the set \
                  rather than judging each on its own.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("lookback", &[20.0, 60.0, 120.0]),
            ("hold_top", &[2.0, 3.0]),
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
    /// Whether this rule needs more than one instrument to mean anything.
    ///
    /// Asked of the engine rather than restated here. A second list of which
    /// rules rank is a second thing to forget to update, and the failure would
    /// be a ranking rule quietly run over a field of one — which produces a
    /// curve, a verdict, and no information.
    #[must_use]
    pub fn ranks_a_set(&self) -> bool {
        arvo_nautilus::CROSS_SECTIONAL_STRATEGIES.contains(&self.name)
    }

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
