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
pub mod report;
pub mod staleness;
pub mod study;
pub mod views;

// Re-exported because the integration tests in `tests/` reach them through
// this module, and because `study_view` and friends read as research
// vocabulary rather than as "the views submodule's business".
pub use study::{list_strategies, panel_for, study_data, study_for, walk_forward_for};
pub use views::{metrics_view, study_view, trades_view, walk_forward_view};

// Helpers the submodules share. These were mutually visible when all four jobs
// lived in one file; re-exporting through the parent is what keeps that true
// without making them part of the crate's outward surface. `use super::*` in a
// submodule reaches the parent, not a sibling.
pub use study::panel_dataset_version;
pub use views::{curve_points, panel_view, verdict_label};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use arvo_data::{BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{
    memory::{EvidenceStore, Record, StoredRecord},
    CostModel, DatasetRef, DateRange, Experiment, ExperimentFamily, ExperimentId, HypothesisId,
    Metrics, ParameterGrid, SimulationProvider, StrategyDocument, StrategySpec, Verdict,
};

// The view shapes live in `arvo-views` so the window cannot drift from
// them. See that crate for what two hand-mirrored copies cost.
//
// `pub use`, not `use`: these are re-exported as this module's own surface
// and `tests/chart_alignment.rs` reaches them through it.
pub use arvo_views::{
    AttachmentView, EventKindView, EventView, ResearchProblemView, SeverityView, TradeRowExport,
    BookView, BreadthView, CandlePoint, ComparisonRowView, ComparisonView, CurvePoint,
    AfterTaxView, DataFindingView, DataLibraryView, DivergenceView, DividendGapView, FetchView, FoldView,
    HistoryEntryView,
    HistoryView, InstrumentView, MatchView, MemberView, MetricsView, MonthlyReturnView, NamedCurveView,
    InstrumentChartView, OutcomeView, PanelView, QuoteView, RecommendationView, RecordView, ReplayView, StabilityView,
    SourceComparisonView, SourceView, StrategyView, StudyView, SurfaceCell, SurfaceView,
    TradeMarkerView, TradeRowView,
    TradesView, UnreadableView, WalkForwardView,
};

use crate::CommandError;

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
#[derive(Debug)]
pub struct StrategyPlan {
    /// What this strategy is called: the picker's entry, and the name a
    /// person chooses. For everything Arvo ships it is also the engine's
    /// rule; for a contributed document (#162) it is the document's own
    /// namespaced name and [`Self::rule`] says what actually runs.
    name: &'static str,
    /// The engine rule this searches, when that is not the name. `None` for
    /// everything Arvo ships, where the two are the same thing.
    rule: Option<&'static str>,
    /// What to call it in a menu.
    pub label: &'static str,
    /// One line on what it trades, because a name is not a description and
    /// the difference between these rules is the whole point of having them.
    pub premise: &'static str,
    /// Parameters every trial shares.
    pub fixed: &'static [(&'static str, f64)],
    /// What the search varies.
    pub axes: &'static [(&'static str, &'static [f64])],
    /// The resolution this rule is defined at.
    ///
    /// Two of them are anchored to a trading session and mean nothing on
    /// daily bars — the engine refuses that combination rather than running
    /// it, so the choice belongs here where the data can be checked for it.
    intraday: bool,
    /// Whether the rule trades its instrument's option chain rather than the
    /// instrument (#86). Its costs are an option spread, its data is the chain
    /// as well as the bars, and its window is where both exist.
    options: bool,
}

const PLANS: &[StrategyPlan] = &[
    StrategyPlan {
        name: "sma_cross",
        rule: None,
        label: "Moving-average crossover",
        premise: "The control. Not a good idea, a rule nobody disputes.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("fast", &[5.0, 10.0, 20.0]),
            ("slow", &[30.0, 60.0, 120.0]),
        ],
        intraday: false,
        options: false,
    },
    StrategyPlan {
        name: "volatility_breakout",
        rule: None,
        label: "Volatility breakout",
        premise: "A thrust measured in ATRs, so it means the same on any price.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("entry_atr_multiple", &[0.5, 1.0, 1.5]),
            ("atr_period", &[10.0, 20.0]),
        ],
        intraday: false,
        options: false,
    },
    StrategyPlan {
        name: "momentum_breakout",
        rule: None,
        label: "Momentum breakout",
        premise: "Buy a new channel high, leave on a trailing channel low.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("entry_period", &[20.0, 55.0, 100.0]),
            ("exit_period", &[10.0, 20.0]),
        ],
        intraday: false,
        options: false,
    },
    StrategyPlan {
        name: "cross_sectional_momentum",
        rule: None,
        label: "Cross-sectional momentum",
        premise: "Hold the few that rose most, and nothing else. Ranks the set \
                  rather than judging each on its own.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("lookback", &[20.0, 60.0, 120.0]),
            ("hold_top", &[2.0, 3.0]),
        ],
        intraday: false,
        options: false,
    },
    StrategyPlan {
        name: "opening_range",
        rule: None,
        label: "Opening range breakout",
        premise: "The session's first bars set a range; trade the break, once a day.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[
            ("range_bars", &[3.0, 6.0, 12.0]),
            ("target_range_multiple", &[1.0, 2.0]),
        ],
        intraday: true,
        options: false,
    },
    StrategyPlan {
        name: "vwap_reversion",
        rule: None,
        label: "VWAP reversion",
        premise: "Stretch away from the session's average price is expected to close.",
        fixed: &[("trade_size", TRADE_SIZE)],
        axes: &[("entry_deviations", &[1.0, 1.5, 2.0])],
        intraday: true,
        options: false,
    },
    StrategyPlan {
        name: "put_spread",
        rule: None,
        label: "SPY put spreads",
        premise: "Sell a put near a delta a month out, buy one a width below; keep the credit \
                  unless the market falls through it. Trades the instrument's option chain.",
        // Held fixed so the search stays small: every axis is a trial
        // deflated against. The rate and yield are stated, not implied — see
        // `arvo_nautilus`'s put spread rule for why.
        fixed: &[
            ("trade_size", 100.0),
            ("width", 5.0),
            ("take_profit", 0.5),
            ("exit_dte", 21.0),
            ("rate", 0.04),
            ("dividend_yield", 0.013),
        ],
        axes: &[
            ("short_delta", &[0.15, 0.20, 0.30]),
            ("dte", &[30.0, 45.0]),
        ],
        intraday: false,
        options: true,
    },
    StrategyPlan {
        name: "zero_dte_breakout",
        rule: None,
        label: "SPY 0DTE breakout options",
        premise: "The session's first half hour sets a range; buy a same-day call on a close \
                  above it or a put on a close below, sell at a multiple or a loss, or let it \
                  settle. Trades the instrument's option chain.",
        fixed: &[
            ("trade_size", 100.0),
            ("range_bars", 6.0),
            ("delta", 0.5),
            ("rate", 0.04),
            ("dividend_yield", 0.013),
        ],
        axes: &[
            ("target_multiple", &[1.5, 2.0]),
            ("stop_fraction", &[0.3, 0.5]),
        ],
        intraday: true,
        options: true,
    },
    StrategyPlan {
        name: "zero_dte_put_spread",
        rule: None,
        label: "SPY 0DTE put spreads",
        premise: "Sell a same-day put near a delta half an hour after the open, buy one a width \
                  below; stop out at a multiple of the credit, or let it settle. Trades the \
                  instrument's option chain.",
        // Held to settlement unless stopped: a 100% target never closes early.
        fixed: &[
            ("trade_size", 100.0),
            ("width", 2.0),
            ("take_profit", 1.0),
            ("entry_minutes", 30.0),
            ("rate", 0.04),
            ("dividend_yield", 0.013),
        ],
        axes: &[
            ("short_delta", &[0.05, 0.10, 0.20]),
            ("stop_multiple", &[2.0, 4.0]),
        ],
        intraday: true,
        options: true,
    },
];

/// The resolution intraday studies run at.
///
/// Five minutes because that is what the data library holds and what the
/// Robinhood feed serves without special pleading. Not a parameter: changing
/// it changes what every session-anchored rule means, so it belongs in the
/// experiment record rather than in a UI field.
pub const INTRADAY: arvo_data::BarInterval =
    arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);

/// The default when nobody has chosen: the control.
pub const STRATEGY: &str = "sma_cross";

impl StrategyPlan {
    /// Whether this rule needs more than one instrument to mean anything.
    ///
    /// Asked of the engine rather than restated here. A second list of which
    /// rules rank is a second thing to forget to update, and the failure would
    /// be a ranking rule quietly run over a field of one — which produces a
    /// curve, a verdict, and no information.
    #[must_use]
    pub fn ranks_a_set(&self) -> bool {
        arvo_nautilus::CROSS_SECTIONAL_STRATEGIES.contains(&self.rule())
    }

    /// The engine rule that runs, which is the name unless a contributed
    /// document said otherwise.
    #[must_use]
    pub const fn rule(&self) -> &'static str {
        match self.rule {
            Some(rule) => rule,
            None => self.name,
        }
    }

    /// What this is called in the picker and in a person's head.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Looks a strategy up by the name a person chose: Arvo's own first,
    /// then what an extension contributed (#162).
    #[must_use]
    pub fn find(name: &str) -> Option<&'static Self> {
        Self::find_shipped(name).or_else(|| {
            let table = CONTRIBUTED.lock().ok()?;
            table.get(name).map(|(_, plan)| *plan)
        })
    }

    /// Looks up one of Arvo's own, which is what a contributed document has
    /// to name: a contribution searches a rule the engine implements, and
    /// cannot contribute a rule.
    #[must_use]
    pub fn find_shipped(name: &str) -> Option<&'static Self> {
        PLANS.iter().find(|plan| plan.name == name)
    }

    /// Every rule Arvo implements, in menu order.
    #[must_use]
    pub fn shipped() -> &'static [Self] {
        PLANS
    }

    /// The resolution this rule is defined at, and so the bars it runs on.
    #[must_use]
    pub fn interval(&self) -> arvo_data::BarInterval {
        if self.intraday {
            INTRADAY
        } else {
            arvo_data::BarInterval::DAILY
        }
    }

    /// Whether the rule trades the instrument's option chain.
    #[must_use]
    pub const fn trades_options(&self) -> bool {
        self.options
    }

    /// This family as a document (#161).
    ///
    /// Arvo's own catalog is a search over data already, which is what makes
    /// it the second implementation of the document form: if a shipped
    /// family could not be said as one, the form would be an adapter for
    /// somebody else's catalog rather than a shape this platform uses.
    ///
    /// What the plan knows and the document does not — whether the rule
    /// ranks a set, whether it trades an option chain — stays out on
    /// purpose. Those are asked of the engine by rule name, and a second
    /// list of them is a second thing to forget to update.
    #[must_use]
    pub fn document(&self) -> arvo_research::StrategyDocument {
        arvo_research::StrategyDocument {
            name: self.name.to_owned(),
            label: self.label.to_owned(),
            premise: self.premise.to_owned(),
            interval: self.interval(),
            kind: arvo_research::StrategyKind::Grid(arvo_research::Grid {
                rule: self.name.to_owned(),
                fixed: self.fixed.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect(),
                axes: self
                    .axes
                    .iter()
                    .map(|(name, values)| ((*name).to_owned(), values.to_vec()))
                    .collect(),
            }),
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

/// Strategies extensions have contributed, by the name the picker shows
/// (#162).
///
/// Interned rather than owned by the caller: every run path takes a
/// `&'static StrategyPlan`, because Arvo's own catalog is a `const` array,
/// and threading a lifetime through the study, walk-forward, panel and
/// replay paths would be a large change for no behaviour. The set is
/// bounded by what is installed, and each document is interned once per
/// version it has.
///
/// ponytail: a document edited during a session leaks its previous form.
/// Bounded by edits, and the alternative is the lifetime refactor. If an
/// extension author's edit loop ever makes that matter, the fix is to make
/// the plan's fields owned and take `&StrategyPlan` throughout.
static CONTRIBUTED: std::sync::Mutex<BTreeMap<String, (String, &'static StrategyPlan)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Whether a contributed document is one this build can put in the picker,
/// or the reason it is not (#162).
///
/// The reasons are the ones a person can act on: a rule Arvo does not
/// implement, a search with nothing in it, a resolution the rule is not
/// defined at, parameters the engine will not take. A document is data —
/// the trust story is a theme's — so the checking is about whether it
/// describes a run, not about what it is allowed to do.
///
/// # Errors
///
/// The reason, phrased for the extensions page.
pub fn offerable(document: &StrategyDocument) -> Result<&'static StrategyPlan, String> {
    let arvo_research::StrategyKind::Grid(grid) = &document.kind else {
        // Recognised and refused, the way a webview is (ADR-0024). Arvo can
        // read a rules document (#161); running one needs a runner it does
        // not have, and that is #125's question.
        return Err("a rules strategy: this build can read one but has no runner for it yet (#125)".to_owned());
    };
    let rule = StrategyPlan::find_shipped(&grid.rule)
        .ok_or_else(|| format!("no rule called {:?}; this build implements {}", grid.rule, shipped_names()))?;
    if grid.configurations() == 0 {
        return Err("searches nothing: an axis with no values in it".to_owned());
    }
    if document.interval != rule.interval() {
        return Err(format!(
            "{} is defined at {}, and the document says {}",
            grid.rule,
            rule.interval(),
            document.interval
        ));
    }
    // What the engine will actually be handed, checked once rather than
    // discovered after a person has chosen it and waited.
    let mut params = fixed_for(rule, grid);
    let first = grid
        .grid()
        .combinations()
        .into_iter()
        .next()
        .ok_or_else(|| "searches nothing".to_owned())?;
    params.extend(first);
    let spec = arvo_research::StrategySpec { name: rule.rule().to_owned(), params };
    arvo_nautilus::check_plan(&spec, rule.interval())
        .map_err(|err| format!("the engine will not run it: {err}"))?;
    Ok(rule)
}

/// A contributed grid's fixed parameters: the rule's own, with whatever the
/// document named on top.
///
/// The rule brings its own. `trade_size` is Arvo's sizing convention, the
/// same number for every strategy it ships, and not something a catalog
/// author chose or should have to know — a document that had to restate it
/// would be carrying this build's settings around inside someone else's
/// repository.
fn fixed_for(rule: &'static StrategyPlan, grid: &arvo_research::Grid) -> BTreeMap<String, f64> {
    let mut params: BTreeMap<String, f64> =
        rule.fixed.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect();
    params.extend(grid.fixed.clone());
    // What the search varies is not also fixed: an axis and a constant of the
    // same name would hand the engine two answers.
    params.retain(|name, _| !grid.axes.contains_key(name));
    params
}

fn shipped_names() -> String {
    PLANS.iter().map(|plan| plan.name).collect::<Vec<_>>().join(", ")
}

/// Replaces the contributed catalog with what the enabled extensions say
/// now, so removing or disabling one takes its strategies out of the picker.
///
/// Documents that cannot be offered are dropped here; the extensions page is
/// where their reason is shown.
pub fn set_contributed(documents: &[(String, StrategyDocument)]) {
    let Ok(mut table) = CONTRIBUTED.lock() else { return };
    let mut next = BTreeMap::new();
    for (id, document) in documents {
        let Ok(rule) = offerable(document) else { continue };
        let version = document.version();
        // Interned once per version: the same document read again is the
        // same plan, and an edited one is a new one.
        let plan = match table.get(id) {
            Some((held, plan)) if *held == version => *plan,
            _ => intern(id, document, rule),
        };
        next.insert(id.clone(), (version, plan));
    }
    *table = next;
}

/// Leaks one contributed document as a plan the run path can hold.
fn intern(id: &str, document: &StrategyDocument, rule: &'static StrategyPlan) -> &'static StrategyPlan {
    let arvo_research::StrategyKind::Grid(grid) = &document.kind else {
        unreachable!("offerable accepted a grid")
    };
    let text = |value: &str| -> &'static str { Box::leak(value.to_owned().into_boxed_str()) };
    let fixed: Vec<(&'static str, f64)> =
        fixed_for(rule, grid).iter().map(|(name, value)| (text(name), *value)).collect();
    let axes: Vec<(&'static str, &'static [f64])> = grid
        .axes
        .iter()
        .map(|(name, values)| (text(name), &*Box::leak(values.clone().into_boxed_slice())))
        .collect();
    Box::leak(Box::new(StrategyPlan {
        name: text(id),
        rule: Some(rule.rule()),
        label: text(&document.label),
        premise: text(&document.premise),
        fixed: Box::leak(fixed.into_boxed_slice()),
        axes: Box::leak(axes.into_boxed_slice()),
        intraday: rule.intraday,
        options: rule.options,
    }))
}

/// Every strategy the picker offers: Arvo's own, then what extensions have
/// contributed.
#[must_use]
pub fn offered() -> Vec<&'static StrategyPlan> {
    let mut all: Vec<&'static StrategyPlan> = PLANS.iter().collect();
    if let Ok(table) = CONTRIBUTED.lock() {
        all.extend(table.values().map(|(_, plan)| *plan));
    }
    all
}

/// Holds the wiring a research run needs, built once at startup.
pub struct ResearchService {
    pub simulation: Arc<NautilusSimulation<CsvBars>>,
    pub bars: CsvBars,
    pub data_dir: PathBuf,
    pub memory: EvidenceStore,
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

    /// Puts the project's risk model in force for the study about to run
    /// (`crate::risk`). Before every run, not once at startup, so an edit in
    /// the editor applies to the next study without a restart.
    ///
    /// # Errors
    ///
    /// A risk file the model cannot stand behind; the study does not run.
    pub fn load_risk(&self) -> Result<(), CommandError> {
        let root = self.data_dir.parent().unwrap_or(&self.data_dir);
        crate::risk::load(root).map(|_| ()).map_err(CommandError::Failed)
    }
}
