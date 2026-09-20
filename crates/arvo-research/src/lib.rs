//! The research domain — the part of Arvo that no trading engine provides.
//!
//! ```text
//! Hypothesis → Experiment → Simulation → Evaluation → Evidence
//! ```
//!
//! Simulation is delegated; **evaluation is not**. Deciding whether a result
//! means anything is the part no trading engine provides, and it lives in
//! [`evaluation`] alongside the evidence it produces.
//!
//! Everything here is Arvo-owned. Nothing here names a trading primitive:
//! no orders, no fills, no positions, no accounts. Those live entirely on the
//! Nautilus side of [`SimulationProvider`], which is what lets Arvo avoid
//! duplicating Nautilus's domain model *and* avoid coupling to it.
//!
//! # Why there is a provider trait here at all
//!
//! Not for portability — there is exactly one simulation engine and there is
//! no plan for a second. The trait exists to **invert a dependency**:
//! `arvo-nautilus` depends on this crate and implements the trait, so this
//! crate never names `arvo-nautilus`. That turns the containment rule from a
//! policy someone has to remember into something the compiler enforces.
//!
//! An abstraction earns its place by removing a dependency or a panic, not by
//! anticipating an implementation nobody has asked for.

pub mod advice;
pub mod agent_search;
pub mod book;
pub mod breadth;
pub mod correlation;
pub mod document;
pub mod dividend;
pub mod evaluation;
mod experiment;
pub mod family;
pub mod greeks;
pub mod memory;
pub mod overnight;
pub mod panel;
pub mod psr;
pub mod reconcile;
pub mod regime;
pub mod replay;
pub mod reported;
pub mod share;
mod simulation;
pub mod stress;
pub mod tax;
pub mod walk_forward;

pub use advice::{
    recommend, recommend_panel, recommend_walk_forward, Recommendation, Severity,
};
pub use agent_search::AgentSearch;
pub use book::combine;
pub use breadth::Breadth;
pub use correlation::RollingCorrelations;
pub use document::{Combine, Grid, Rule, RuleSet, Rules, StrategyDocument, StrategyKind};
pub use dividend::{measure_dividend_gap, DividendGap};
pub use evaluation::{
    evaluate_against_benchmark, Evaluation, EvaluationCriteria, Evidence, Metrics, Verdict,
};
pub use family::{
    run_family, ExperimentFamily, FamilyEvidence, ParameterGrid, ScoredTrial, Selection,
};
pub use memory::{
    Author, EvidenceStore, Loaded, MemoryError, Record, StoredRecord, Summary, Unreadable, SCHEMA,
};
pub use psr::{period_returns, probabilistic_sharpe};
pub use regime::{Breakdown, Regime, RegimeOutcome};
pub use reconcile::{reconcile, reconcile_parts, Discrepancy};
pub use replay::{replay, Divergence, Replay};
pub use risk::{
    day_trades_in_window, decide, AccountState, CorrelationCap, Correlations, DayTradingRule,
    Decision, Position, Proposal, Rejection, RiskGate, RiskModel, SectorCap, PDT_DAY_TRADES,
    PDT_EQUITY_FLOOR, PDT_WINDOW_DAYS,
};
pub use panel::{run_panel, InstrumentOutcome, KeptEvidence, PanelEvidence, PanelStudy, PooledOutcome};
pub use reported::{judge, Judgement, Reported, ReportedEvidence};
pub use trade::{Direction, ExitReason, Journal, Trade, TradeStats};
pub use walk_forward::{run_walk_forward, AxisStability, WalkForward, WalkForwardEvidence};

// The risk crate, where it has always been reached from: `arvo_research::risk`,
// `arvo_research::trade` and `arvo_research::collateral` are the same modules
// the live path uses directly.
pub use arvo_risk as risk;
pub use arvo_risk::{collateral, trade, CostModel, EquityPoint, OptionSpread};
pub use experiment::{
    DatasetRef, DateRange, Experiment, ExperimentError, ExperimentId, Hypothesis, HypothesisId,
    StrategySpec,
};
pub use simulation::{
    Refused, SimulationError, SimulationProvider, SimulationResult,
};
