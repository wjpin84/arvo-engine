//! What was asked, of which data, under which assumptions.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{CostModel, RiskModel};

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
    /// What corporate actions these prices are adjusted for.
    ///
    /// Part of the dataset's *identity*, which is what this type is for: a
    /// split-adjusted series and a total-return one are two datasets, not one
    /// dataset with a setting. See [ADR-0013].
    ///
    /// It decides whether the dividend gap is a correction or a description —
    /// on a total-return series the distribution is already in the returns, so
    /// subtracting the gap would double-count. `dividend::DividendGap` carries
    /// this through for exactly that reason.
    ///
    /// `default` because this is a persisted format, and `Split` because that
    /// is the only basis any source has ever asked for: every record written
    /// before this field existed genuinely ran on it.
    ///
    /// [ADR-0013]: https://github.com/wjpin84/arvo-desktop/blob/master/https://github.com/wjpin84/arvo-adrs/blob/main/0013-dividends-arrive-as-reinvestment.md
    #[serde(default)]
    pub adjustment: arvo_data::source::Adjustment,
}

/// Which strategy to run, and with what parameters.
///
/// `BTreeMap` rather than `HashMap`: the ordering is part of the record, so
/// two runs of the same experiment serialise identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategySpec {
    pub name: String,
    pub params: BTreeMap<String, f64>,
    /// The rule as data when `name` is one (#225), so a finding on it
    /// replays without the file. `None` for a rule compiled into the engine.
    /// `default` because this is a persisted format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<crate::rule::RuleDefinition>,
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
    /// Instruments held *alongside* [`Self::instrument`], out of one account.
    ///
    /// Empty is the single-instrument run, unchanged in every respect. When it
    /// is not, the engine loads every one of them, runs the same rule on each,
    /// and settles them all against the same balance — which is the only way
    /// capital contention shows up at all. Two positions that a rule wanted at
    /// once and could only half afford look identical to two it wanted in turn
    /// when each is simulated with the whole account behind it.
    ///
    /// [`Self::instrument`] stays the head of the set rather than becoming one
    /// of a bag, because it is the experiment's identity: it names the file the
    /// dataset hash is anchored to and the series a benchmark is drawn against.
    ///
    /// `default` because this is a persisted format: every experiment recorded
    /// before a run could hold more than one loads as what it was.
    #[serde(default)]
    pub alongside: Vec<String>,
    /// The stock an option run settles against, as a library series —
    /// `SPY.AIEX` — when [`Self::instrument`] is an option contract.
    ///
    /// Not traded, and not one of [`Self::instruments`]: nothing runs the rule
    /// on it. Its close on a contract's expiration date is what that contract
    /// settles at. A contract held to expiry with no underlying cannot be
    /// settled, so an option run without one is refused.
    ///
    /// Skipped when absent, so every record without one serialises unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underlying: Option<String>,
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

impl Experiment {
    /// Every instrument this run holds, head first.
    ///
    /// The one place that knows [`Self::instrument`] and [`Self::alongside`]
    /// are halves of the same set, so nothing downstream has to remember to
    /// chain them and nothing can chain them in a different order.
    #[must_use]
    pub fn instruments(&self) -> Vec<String> {
        let mut all = Vec::with_capacity(1 + self.alongside.len());
        all.push(self.instrument.clone());
        all.extend(self.alongside.iter().cloned());
        all
    }

    /// Whether the set is usable, as a reason it is not.
    ///
    /// # Errors
    ///
    /// Returns the reason if an instrument appears twice. A duplicate is never
    /// what was meant and would double the rule's exposure to one name while
    /// reporting the count of a diversified book.
    pub fn check_instruments(&self) -> Result<(), String> {
        let all = self.instruments();
        for (index, name) in all.iter().enumerate() {
            if all[..index].contains(name) {
                return Err(format!(
                    "{name} appears twice: a run cannot hold the same instrument \
                     alongside itself"
                ));
            }
        }
        Ok(())
    }
}

/// Why an experiment could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    #[error("window ends {to} before it starts {from}")]
    BackwardsRange { from: NaiveDate, to: NaiveDate },
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
}
