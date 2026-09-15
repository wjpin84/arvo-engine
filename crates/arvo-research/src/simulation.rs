//! The engine boundary: what a run returns, and how it fails.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::{EquityPoint, Experiment, ExperimentId, Trade};

/// Orders the venue would not take, by what they were for.
///
/// Counted because an order refused *after* the gate accepted it used to
/// vanish: the strategy believed it held a position it never bought, sat out
/// the rest of the session, and the run reported a rule that rarely traded.
/// On two years of five-minute AAPL an opening-range rule took 3 of 72
/// breakouts and nothing anywhere said so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    /// Entries refused — signals the result never acted on.
    pub entries: usize,
    /// Exits refused — positions held longer than the rule decided to.
    pub exits: usize,
}

impl Refused {
    #[must_use]
    pub const fn any(&self) -> bool {
        self.entries > 0 || self.exits > 0
    }
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
    /// Orders the venue refused. `default`: a result from before this was
    /// counted says nothing about refusals, which is what it knew.
    #[serde(default)]
    pub refused: Refused,
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

    /// Cash distributions for the experiment's instruments over its window,
    /// keyed by instrument.
    ///
    /// # Why the engine is asked for this
    ///
    /// It is not really the engine's business — distributions are data, and the
    /// engine is Nautilus. But this trait is the only seam `arvo-research` has
    /// to the data behind a run: it deliberately depends on no concrete
    /// provider, and everything else it knows about an experiment's bars it
    /// learns by asking here. Threading a second data handle through
    /// `run_family`, `run_panel` and `run_walk_forward` to reach one report line
    /// would be a wider change for the same answer.
    ///
    /// A missing key means no distribution series exists for that instrument —
    /// unknown, not zero. An empty map therefore means nothing is known at all,
    /// and [`crate::DividendGap`] is not recorded. See [`crate::dividend`] for
    /// why that distinction is the whole point.
    ///
    /// Defaulted to empty so every test double and in-memory fixture is
    /// unaffected: they have no such series, and saying so is what empty means.
    fn dividends(
        &self,
        _experiment: &Experiment,
    ) -> std::collections::HashMap<String, Vec<arvo_data::Dividend>> {
        std::collections::HashMap::new()
    }

    /// One instrument's bars, for a report that needs a price the run's own
    /// result does not carry — the underlying's, to replay an option run
    /// through a crash (#85). Asked here for the reason [`Self::dividends`] is.
    ///
    /// Defaulted to none, which leaves such a report unmeasured rather than
    /// wrong.
    fn bars_for(
        &self,
        _instrument: &str,
        _interval: arvo_data::BarInterval,
        _from: NaiveDate,
        _to: NaiveDate,
    ) -> Option<Vec<arvo_data::Bar>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_return_needs_two_points_to_mean_anything() {
        let mut result = SimulationResult {
            experiment: ExperimentId::from("e-1"),
            engine: "test 0".to_owned(),
            trades: 0,
            ledger: Vec::new(),
            refused: Refused::default(),
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
