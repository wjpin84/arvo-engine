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

pub mod evaluation;
pub mod family;

pub use evaluation::{
    evaluate_against_benchmark, Evaluation, EvaluationCriteria, Evidence, Metrics, Verdict,
};
pub use family::{run_family, ExperimentFamily, FamilyEvidence, ParameterGrid, Selection};

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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    pub commission_bps: f64,
    pub slippage_bps: f64,
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
    pub dataset: DatasetRef,
    pub strategy: StrategySpec,
    pub costs: CostModel,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    pub experiment: ExperimentId,
    /// Identifies the engine that produced this, e.g. `"nautilus 0.63.0"`.
    /// Part of the reproducibility record: a result is only comparable to
    /// another from the same engine version.
    pub engine: String,
    pub trades: u32,
    /// Account equity, one point per bar, starting at the opening balance.
    pub equity_curve: Vec<f64>,
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
        let first = *self.equity_curve.first()?;
        let last = *self.equity_curve.last()?;
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
            equity_curve: vec![100_000.0],
        };
        assert_eq!(result.total_return(), None, "one point is not a return");

        result.equity_curve.push(110_000.0);
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
