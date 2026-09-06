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
pub mod evaluation;
pub mod family;
pub mod memory;
pub mod panel;
pub mod trade;
pub mod walk_forward;

pub use advice::{recommend, Recommendation, Severity};
pub use evaluation::{
    evaluate_against_benchmark, Evaluation, EvaluationCriteria, Evidence, Metrics, Verdict,
};
pub use family::{
    run_family, ExperimentFamily, FamilyEvidence, ParameterGrid, ScoredTrial, Selection,
};
pub use memory::{EvidenceStore, Loaded, MemoryError, Record, StoredRecord};
pub use panel::{run_panel, InstrumentOutcome, PanelEvidence, PanelStudy, PooledOutcome};
pub use trade::{Direction, ExitReason, Trade, TradeStats};
pub use walk_forward::{run_walk_forward, AxisStability, WalkForward, WalkForwardEvidence};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A claim about the world that an experiment can support or contradict.
///
/// Deliberately prose. The LLM proposes these; *evidence* — not the LLM —
/// decides what survives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: HypothesisId,
    /// e.g. "12-month momentum predicts positive 20-day forward returns in
    /// large-cap US equities".
    pub claim: String,
}

macro_rules! id_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

id_newtype!(
    /// Identifies a hypothesis across its whole life, including after the
    /// experiments that tested it have been superseded.
    HypothesisId
);
id_newtype!(
    /// Identifies one *run*. Two experiments differing in any pinned field —
    /// a parameter, the cost model, the seed — are different experiments and
    /// get different ids.
    ExperimentId
);

/// The exact data an experiment ran against.
///
/// A plain reference rather than a `Dataset` value, so this crate does not
/// depend on `arvo-data`. Reproducibility needs the *identity* of the input,
/// not the input itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetRef {
    pub id: String,
    /// Immutable once published. Re-running an experiment against a mutated
    /// dataset is a new experiment, not a repeat of the old one.
    pub version: String,
}

/// What a fill is assumed to cost.
///
/// Pinned into the experiment because it changes results more than most
/// strategy parameters do, and because an unstated cost assumption is the
/// most common way a backtest flatters itself.
///
/// Two proportional rates were the whole model at first, which quietly
/// assumed every cost scales with notional. Real US equity schedules do not:
/// a flat ticket charge falls hardest on small positions, per-share fees
/// scale with size rather than value, and the regulatory charges fall on
/// *sells* only. Each field below is a different shape for that reason, and
/// every one of them defaults to zero, so a schedule that does not have a
/// charge simply does not state it.
///
/// Taxes are deliberately absent — see [`crate::trade`] for why they are not
/// a per-fill cost.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    /// Proportional commission, charged on every fill, both sides.
    pub commission_bps: f64,
    /// How much worse than the quoted price a fill is assumed to land.
    pub slippage_bps: f64,
    /// Flat charge per fill, in account currency.
    ///
    /// The one cost that does not scale at all, so it is the one that decides
    /// whether a strategy trading small size often is viable. Zero at a
    /// commission-free US equity broker.
    #[serde(default)]
    pub per_fill: f64,
    /// Charged per unit sold — the shape of FINRA's Trading Activity Fee.
    ///
    /// Sell side only, and per *share* rather than per dollar, so it bites
    /// hardest on cheap instruments where a share is worth little.
    #[serde(default)]
    pub per_unit_sold: f64,
    /// Basis points of sale proceeds — the shape of the SEC Section 31 fee.
    ///
    /// Sell side only. The rate is reset by the SEC periodically and is not a
    /// constant worth hardcoding anywhere; it belongs in whatever states the
    /// broker schedule, checked against a current one.
    #[serde(default)]
    pub sell_notional_bps: f64,
}

impl CostModel {
    /// Only the two proportional costs — what the model was before venue and
    /// regulatory fees existed.
    ///
    /// Kept because most callers genuinely have nothing else to say, and
    /// spelling three zeroes at every construction site invites one of them
    /// being wrong.
    #[must_use]
    pub const fn proportional(commission_bps: f64, slippage_bps: f64) -> Self {
        Self {
            commission_bps,
            slippage_bps,
            per_fill: 0.0,
            per_unit_sold: 0.0,
            sell_notional_bps: 0.0,
        }
    }

    /// Whether anything beyond the proportional rates is charged.
    ///
    /// Used to decide whether the engine needs Arvo's own fee model at all:
    /// with nothing extra to charge, the venue default already computes the
    /// same number.
    #[must_use]
    pub fn has_venue_fees(&self) -> bool {
        self.per_fill != 0.0 || self.per_unit_sold != 0.0 || self.sell_notional_bps != 0.0
    }

    /// Rejects a schedule that cannot mean anything.
    ///
    /// # Errors
    ///
    /// Returns a reason if any rate is negative or not finite. A negative fee
    /// is a rebate, and a backtest that pays the trader to trade is the single
    /// most flattering bug available.
    pub fn check(&self) -> Result<(), String> {
        for (name, value) in [
            ("commission_bps", self.commission_bps),
            ("slippage_bps", self.slippage_bps),
            ("per_fill", self.per_fill),
            ("per_unit_sold", self.per_unit_sold),
            ("sell_notional_bps", self.sell_notional_bps),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{name} must be zero or positive, got {value}"));
            }
        }
        Ok(())
    }
}

/// How a strategy protects itself.
///
/// Pinned into the experiment beside the cost model, and for the same reason:
/// it changes the result more than most strategy parameters do. For most
/// systematic strategies the stop is not a detail bolted on afterwards — it
/// *is* part of the rule, and "momentum breakout with a 2× ATR stop" and
/// "momentum breakout" are two different strategies with different return
/// distributions.
///
/// Running without one is allowed and is the honest default for a control,
/// but it should be a stated choice rather than an omission: an unstopped
/// strategy has a fatter left tail than the stopped version of itself, so a
/// backtest that quietly leaves the stop out flatters the idea.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskModel {
    /// Stop distance as a multiple of ATR. `None` runs with no stop at all.
    ///
    /// A multiple rather than a fixed price: a stop has to scale with how
    /// much the instrument actually moves, or it is arbitrarily tight on a
    /// volatile name and arbitrarily loose on a quiet one.
    pub stop_atr_multiple: Option<f64>,
    /// Bars of history the ATR is averaged over.
    pub atr_period: usize,
    /// Fraction of starting capital risked on one trade, sizing the position
    /// so that a stop-out costs about that much.
    ///
    /// `None` trades a fixed quantity instead. Requires a stop: risking 1%
    /// means nothing without a distance to the exit, and there is no
    /// defensible default to invent for it.
    pub risk_per_trade: Option<f64>,
    /// The largest fraction of starting capital one position may occupy.
    ///
    /// Not a refinement — without it, risk sizing is unbounded. Position size
    /// is capital-at-risk divided by the stop distance, and a *tight* stop
    /// therefore buys a *bigger* position. At a daily resolution ATR is wide
    /// enough that this never binds; on five-minute bars a one-dollar stop on
    /// a five-hundred-dollar share asks for a position several times the
    /// account, every order is rejected, and the backtest reports zero trades
    /// with no error anywhere. Found exactly that way.
    ///
    /// Defaults to one whole account: you cannot spend money you do not have,
    /// which is arithmetic rather than a strategy choice. Above 1.0 is
    /// leverage and has to be asked for.
    pub max_position_fraction: Option<f64>,
}

impl Default for RiskModel {
    /// No stop and fixed sizing — what a backtest does when nobody has said
    /// otherwise. Named rather than implied, so the absence is visible in the
    /// record.
    fn default() -> Self {
        Self {
            stop_atr_multiple: None,
            atr_period: 14,
            risk_per_trade: None,
            max_position_fraction: Some(1.0),
        }
    }
}

impl RiskModel {
    /// Rejects combinations that cannot mean anything.
    ///
    /// # Errors
    ///
    /// Returns a reason if the model is self-contradictory — most importantly
    /// sizing by risk with no stop to measure the risk against, which would
    /// otherwise silently fall back to some invented quantity.
    pub fn check(&self) -> Result<(), String> {
        if self.atr_period == 0 {
            return Err("atr_period must be at least 1 bar".to_owned());
        }
        if let Some(multiple) = self.stop_atr_multiple {
            if !multiple.is_finite() || multiple <= 0.0 {
                return Err(format!(
                    "stop_atr_multiple must be positive, got {multiple}"
                ));
            }
        }
        if let Some(cap) = self.max_position_fraction {
            if !cap.is_finite() || cap <= 0.0 {
                return Err(format!("max_position_fraction must be positive, got {cap}"));
            }
        }
        if let Some(risk) = self.risk_per_trade {
            if !risk.is_finite() || risk <= 0.0 || risk >= 1.0 {
                return Err(format!(
                    "risk_per_trade is a fraction of capital between 0 and 1, got {risk}"
                ));
            }
            if self.stop_atr_multiple.is_none() {
                return Err(
                    "risk_per_trade needs a stop: position size is capital-at-risk divided by                      the distance to the exit, and without a stop there is no distance"
                        .to_owned(),
                );
            }
        }
        Ok(())
    }
}

/// Which strategy to run, and with what parameters.
///
/// `BTreeMap` rather than `HashMap`: the ordering is part of the record, so
/// two runs of the same experiment serialise identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategySpec {
    pub name: String,
    pub params: BTreeMap<String, f64>,
}

/// A closed date range, inclusive at both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl DateRange {
    /// # Errors
    ///
    /// Returns [`ExperimentError::BackwardsRange`] if `to` precedes `from`.
    pub fn new(from: NaiveDate, to: NaiveDate) -> Result<Self, ExperimentError> {
        if to < from {
            return Err(ExperimentError::BackwardsRange { from, to });
        }
        Ok(Self { from, to })
    }

    #[must_use]
    pub fn contains(&self, date: NaiveDate) -> bool {
        date >= self.from && date <= self.to
    }

    /// Days spanned, counting both ends.
    #[must_use]
    pub fn days(&self) -> i64 {
        (self.to - self.from).num_days() + 1
    }

    /// Splits into an in-sample head and an out-of-sample tail.
    ///
    /// `head_fraction` is of the calendar span, not of the bar count — a
    /// split on trading days would move when the exchange calendar does, and
    /// the boundary has to be reproducible from the record alone.
    ///
    /// Returns `None` if the range is too short to split, or the fraction
    /// would leave either side empty. A degenerate split silently producing
    /// a one-day out-of-sample period is worse than refusing.
    #[must_use]
    pub fn split(&self, head_fraction: f64) -> Option<(Self, Self)> {
        if !(0.0..=1.0).contains(&head_fraction) {
            return None;
        }
        let days = self.days();
        if days < 2 {
            return None;
        }

        #[expect(
            clippy::cast_possible_truncation,
            reason = "days is bounded by the window, and the result is clamped below"
        )]
        let head_days = (days as f64 * head_fraction) as i64;
        if head_days < 1 || head_days >= days {
            return None;
        }

        let boundary = self.from + chrono::Duration::days(head_days - 1);
        Some((
            Self {
                from: self.from,
                to: boundary,
            },
            Self {
                from: boundary + chrono::Duration::days(1),
                to: self.to,
            },
        ))
    }
}

/// Everything needed to reproduce a run.
///
/// The field list *is* the reproducibility contract: if a run's output can
/// change without one of these changing, the record is incomplete and the
/// missing input belongs here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experiment {
    pub id: ExperimentId,
    pub hypothesis: HypothesisId,
    /// Canonical `SYMBOL.VENUE`, validated at the Nautilus boundary rather
    /// than here — this crate has no opinion on venue naming.
    pub instrument: String,
    pub window: DateRange,
    /// The resolution the rule was evaluated at.
    ///
    /// Pinned, because the same rule at five minutes and at one day is not
    /// the same experiment: it sees different prices, trades at different
    /// times, and its statistics annualise by a different factor.
    #[serde(default)]
    pub interval: arvo_data::BarInterval,
    pub dataset: DatasetRef,
    pub strategy: StrategySpec,
    pub costs: CostModel,
    /// How the strategy protects itself. Pinned for the same reason the cost
    /// model is: it changes the answer.
    #[serde(default)]
    pub risk: RiskModel,
    /// Opening account balance. Pinned because position sizing and therefore
    /// the whole equity curve depend on it — a return is not interpretable
    /// without the capital it was earned on.
    pub starting_cash: f64,
    /// Pinned so a stochastic strategy replays identically.
    pub seed: u64,
}

/// What came back from the engine.
///
/// The equity curve is the primitive on purpose: Sharpe, drawdown, hit rate
/// and the rest all derive from it, so evaluation can grow without the engine
/// boundary changing shape every time a new metric is wanted.
/// Account equity at one instant.
///
/// Dated, not just ordered. A bare `Vec<f64>` was enough to compute a return
/// and is not enough to draw one, to align two runs against each other, or to
/// say when a drawdown happened — and the engine knows the dates already, so
/// discarding them was throwing away something free.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    /// When, to the resolution the experiment ran at. A date was enough
    /// while everything was daily and is not once two points can share one.
    pub at: chrono::NaiveDateTime,
    pub equity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    pub experiment: ExperimentId,
    /// Identifies the engine that produced this, e.g. `"nautilus 0.63.0"`.
    /// Part of the reproducibility record: a result is only comparable to
    /// another from the same engine version.
    pub engine: String,
    /// Round trips completed. Derived from [`Self::ledger`] rather than
    /// counted separately, so the two cannot drift apart.
    pub trades: u32,
    /// Account equity, one point per bar, opening at the starting balance.
    pub equity_curve: Vec<EquityPoint>,
    /// Every position the run opened, in the order it opened them.
    ///
    /// `default` so evidence stored before the ledger existed still loads —
    /// it reads back as an empty ledger beside a non-zero `trades`, which is
    /// the honest description of a result recorded before this was captured.
    #[serde(default)]
    pub ledger: Vec<Trade>,
}

impl SimulationResult {
    /// Total return over the run, as a fraction.
    ///
    /// Returns `None` for a curve too short to have moved, rather than
    /// inventing a zero that reads like a real flat result.
    #[must_use]
    pub fn total_return(&self) -> Option<f64> {
        if self.equity_curve.len() < 2 {
            return None;
        }
        let first = self.equity_curve.first()?.equity;
        let last = self.equity_curve.last()?.equity;
        if first == 0.0 {
            return None;
        }
        Some((last - first) / first)
    }
}

/// Why a simulation could not be run, or could not be trusted once run.
#[derive(Debug, thiserror::Error)]
pub enum SimulationError {
    #[error("no data for {instrument} covering {from}..={to}")]
    NoData {
        instrument: String,
        from: NaiveDate,
        to: NaiveDate,
    },
    #[error("unknown strategy {0:?}")]
    UnknownStrategy(String),
    #[error("experiment rejected by the engine: {0}")]
    Rejected(String),
    /// The experiment is well-formed but asks for something the engine does
    /// not yet honour. Distinct from [`SimulationError::Rejected`] on purpose:
    /// silently ignoring a pinned input would make the reproducibility record
    /// a lie, so an unwired assumption fails loudly instead.
    #[error("not supported by this engine: {0}")]
    Unsupported(String),
    #[error("engine failed during the run")]
    Engine(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Why an experiment could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    #[error("window ends {to} before it starts {from}")]
    BackwardsRange { from: NaiveDate, to: NaiveDate },
}

/// Runs an [`Experiment`] and returns what happened.
///
/// Synchronous by design. A backtest is CPU-bound work over an in-memory
/// dataset, not I/O — making it `async` would buy nothing and cost
/// dyn-compatibility. Callers that must not block a reactor wrap the call in
/// `spawn_blocking`, which is what they would have to do anyway.
pub trait SimulationProvider: Send + Sync {
    /// Identifies the engine and its version, for the reproducibility record.
    fn engine(&self) -> &str;

    /// # Errors
    ///
    /// Returns [`SimulationError`] if the data, the strategy, or the engine
    /// cannot honour the experiment as specified. Never partially succeeds:
    /// a result that came back is a result that ran to the end of the window.
    fn run(&self, experiment: &Experiment) -> Result<SimulationResult, SimulationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("test date is valid")
    }

    #[test]
    fn sizing_by_risk_without_a_stop_is_refused() {
        // The important one. Position size is capital-at-risk divided by the
        // distance to the exit; with no stop there is no distance, and the
        // only alternatives are to invent a quantity or to silently ignore
        // the risk setting. Both are worse than refusing.
        let model = RiskModel {
            stop_atr_multiple: None,
            risk_per_trade: Some(0.01),
            ..RiskModel::default()
        };
        let err = model.check().expect_err("should refuse");
        assert!(err.contains("needs a stop"), "{err}");
    }

    #[test]
    fn a_stop_and_a_risk_fraction_together_are_valid() {
        let model = RiskModel {
            stop_atr_multiple: Some(2.0),
            risk_per_trade: Some(0.01),
            ..RiskModel::default()
        };
        assert!(model.check().is_ok());
    }

    #[test]
    fn a_position_cap_bounds_what_risk_sizing_can_ask_for() {
        // The intraday failure this exists to prevent: position size is
        // capital-at-risk over stop distance, so a one-dollar stop on a
        // five-hundred-dollar share asks for several accounts' worth. Every
        // order is then rejected and the backtest reports no trades at all,
        // with nothing anywhere saying why.
        let uncapped = RiskModel {
            stop_atr_multiple: Some(2.0),
            risk_per_trade: Some(0.01),
            max_position_fraction: None,
            ..RiskModel::default()
        };
        assert!(
            uncapped.check().is_ok(),
            "uncapped is legal, just unbounded"
        );
        assert_eq!(
            RiskModel::default().max_position_fraction,
            Some(1.0),
            "and the default is one whole account"
        );
    }

    #[test]
    fn nonsensical_risk_settings_are_refused() {
        for (model, why) in [
            (
                RiskModel {
                    stop_atr_multiple: Some(-1.0),
                    ..RiskModel::default()
                },
                "a negative stop distance",
            ),
            (
                RiskModel {
                    atr_period: 0,
                    ..RiskModel::default()
                },
                "an ATR over zero bars",
            ),
            (
                RiskModel {
                    stop_atr_multiple: Some(2.0),
                    risk_per_trade: Some(1.5),
                    ..RiskModel::default()
                },
                "risking more than all the capital",
            ),
        ] {
            assert!(model.check().is_err(), "{why} should be refused");
        }
    }

    #[test]
    fn the_default_risk_model_is_no_stop_and_says_so() {
        let model = RiskModel::default();
        assert_eq!(model.stop_atr_multiple, None);
        assert_eq!(model.risk_per_trade, None);
        assert_eq!(
            model.max_position_fraction,
            Some(1.0),
            "spending more than the account holds is not a strategy choice"
        );
        assert!(model.check().is_ok(), "absence is valid, just visible");
    }

    #[test]
    fn a_backwards_window_is_rejected_at_construction() {
        let err = DateRange::new(date(2024, 12, 31), date(2024, 1, 1))
            .expect_err("should reject a backwards range");
        assert!(
            matches!(err, ExperimentError::BackwardsRange { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_single_day_window_is_valid() {
        let day = date(2024, 6, 3);
        let range = DateRange::new(day, day).expect("one day is a valid window");
        assert!(range.contains(day));
        assert!(!range.contains(date(2024, 6, 4)));
    }

    #[test]
    fn total_return_needs_two_points_to_mean_anything() {
        let mut result = SimulationResult {
            experiment: ExperimentId::from("e-1"),
            engine: "test 0".to_owned(),
            trades: 0,
            ledger: Vec::new(),
            equity_curve: vec![EquityPoint {
                at: NaiveDate::from_ymd_opt(2024, 1, 1)
                    .expect("valid")
                    .and_time(chrono::NaiveTime::MIN),
                equity: 100_000.0,
            }],
        };
        assert_eq!(result.total_return(), None, "one point is not a return");

        result.equity_curve.push(EquityPoint {
            at: NaiveDate::from_ymd_opt(2024, 1, 2)
                .expect("valid")
                .and_time(chrono::NaiveTime::MIN),
            equity: 110_000.0,
        });
        let total = result.total_return().expect("two points is a return");
        assert!((total - 0.1).abs() < 1e-12, "{total}");
    }

    struct NoopEngine;

    impl SimulationProvider for NoopEngine {
        fn engine(&self) -> &str {
            "noop 0"
        }

        fn run(&self, experiment: &Experiment) -> Result<SimulationResult, SimulationError> {
            Err(SimulationError::Rejected(experiment.id.to_string()))
        }
    }

    /// The research domain must stay expressible without the engine, and the
    /// trait must stay dyn-compatible — the runtime picks its engine at
    /// startup, so `Box<dyn SimulationProvider>` has to be legal. Both facts
    /// are asserted by compilation, at the place that explains why.
    #[test]
    fn the_provider_boundary_is_dyn_compatible_and_arvo_only() {
        let engine: Box<dyn SimulationProvider> = Box::new(NoopEngine);
        assert_eq!(engine.engine(), "noop 0");
    }
}
