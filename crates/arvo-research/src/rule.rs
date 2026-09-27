//! A rule as data (#225): conditions over named indicators, stored in the
//! project beside the rulesets, and evaluated by the engine exactly as a
//! compiled rule is.
//!
//! # Shape
//!
//! ```json
//! {
//!   "name": "twin_cross",
//!   "label": "Moving-average crossover, as data",
//!   "premise": "The control, written down instead of compiled.",
//!   "interval": { "step": 1, "unit": "day" },
//!   "params": { "fast": 10, "slow": 30 },
//!   "indicators": {
//!     "fast": { "kind": "SMA", "period": "fast" },
//!     "slow": { "kind": "SMA", "period": "slow" }
//!   },
//!   "entry": { "cross_above": [ { "var": "fast" }, { "var": "slow" } ] },
//!   "exit":  { "cross_below": [ { "var": "fast" }, { "var": "slow" } ] }
//! }
//! ```
//!
//! Two open formats are borrowed rather than invented. The indicators carry
//! TA-Lib's names (`SMA`, `ATR`, `MAX`, `MIN`), which every charting tool and
//! Pine's `ta.*` map onto. The conditions are JSON Logic
//! (<https://jsonlogic.com>): one operator per object, operands in a list,
//! `{"var": name}` to read a value, a bare number for a literal, `and`, `or`
//! and `!` to combine. JSON Logic has no state, so `cross_above` and
//! `cross_below` are custom operators, which its specification allows; they
//! fire on the bar the relation changes, not on every bar it holds.
//!
//! A period may be a number or the name of a parameter, and a parameter is
//! what a ruleset's grid varies. The defaults in `params` make the file
//! runnable on its own, and a definition that names a parameter with no
//! default is refused when it is read, not when it is run.
//!
//! The definition travels inside the experiment
//! ([`crate::StrategySpec::rule`]), so a finding on a data rule replays on a
//! machine that never saw the file, the way a shared experiment carries its
//! search (ADR 0014).

use std::collections::BTreeMap;

use arvo_data::BarInterval;
use serde::{Deserialize, Serialize};

/// The most bars any indicator may look back over; matches the compiled
/// rules' bound in the plan.
const MAX_PERIOD: f64 = 10_000.0;

/// A rule as data. See the module documentation for the shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleDefinition {
    /// The stable name a ruleset's `rule` and a finding record.
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub premise: String,
    /// The resolution the rule is defined at.
    pub interval: BarInterval,
    /// Defaults for the numbers a grid may vary.
    #[serde(default)]
    pub params: BTreeMap<String, f64>,
    /// Named series over the bars.
    pub indicators: BTreeMap<String, Indicator>,
    pub entry: Condition,
    /// Absent is a rule that leaves on its stop alone.
    #[serde(default)]
    pub exit: Option<Condition>,
    /// Where the rule came from, when it was not written here (#228). A
    /// finding on an imported rule can then be traced to the text it was
    /// translated from, which is the only way to tell later whether the
    /// translation was faithful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

/// What a rule was translated from (#228).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// The language it was written in: `pine`.
    pub kind: String,
    /// The script's own title, as written.
    pub title: String,
    /// The author, as the script gives it. Empty when it names none: a
    /// guess here would be an attribution nobody made.
    #[serde(default)]
    pub author: String,
    /// A hash of the original text, so the same script is recognisable and
    /// an edited one is a different source.
    pub hash: String,
}

/// One indicator, by its TA-Lib name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "UPPERCASE")]
pub enum Indicator {
    /// Simple moving average of `input` over `period` bars.
    Sma {
        #[serde(default)]
        input: Input,
        period: Value,
    },
    /// Exponentially weighted moving average of `input` over `period` bars.
    ///
    /// Seeded with the simple average of the first `period` values, not with
    /// the first value alone.
    Ema {
        #[serde(default)]
        input: Input,
        period: Value,
    },
    /// Average true range over `period` bars.
    Atr { period: Value },
    /// Wilder's relative strength index over `period` bars, 0 to 100.
    ///
    /// Reports after `period + 1` bars: it is computed from changes, and n
    /// bars hold n-1 changes.
    Rsi {
        #[serde(default)]
        input: Input,
        period: Value,
    },
    /// Moving average convergence/divergence.
    ///
    /// One declaration is one series, chosen by `line`, because a rule reads
    /// named numbers. Declare it twice to compare the line against its signal.
    Macd {
        #[serde(default)]
        input: Input,
        fast: Value,
        slow: Value,
        signal: Value,
        #[serde(default)]
        line: MacdLine,
    },
    /// Highest `input` over the last `period` bars.
    Max {
        #[serde(default = "Input::high")]
        input: Input,
        period: Value,
    },
    /// Lowest `input` over the last `period` bars.
    Min {
        #[serde(default = "Input::low")]
        input: Input,
        period: Value,
    },
}

/// Which of MACD's three series a rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MacdLine {
    /// The fast average less the slow one.
    #[default]
    Macd,
    /// The average of the MACD line.
    Signal,
    /// MACD less signal — what a histogram draws.
    Histogram,
}

/// Which field of the bar an indicator reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    Open,
    High,
    Low,
    #[default]
    Close,
    Volume,
}

impl Input {
    const fn high() -> Self {
        Self::High
    }

    const fn low() -> Self {
        Self::Low
    }

    /// The name a condition reads this field under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::High => "high",
            Self::Low => "low",
            Self::Close => "close",
            Self::Volume => "volume",
        }
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "open" => Some(Self::Open),
            "high" => Some(Self::High),
            "low" => Some(Self::Low),
            "close" => Some(Self::Close),
            "volume" => Some(Self::Volume),
            _ => None,
        }
    }
}

/// A number, or the name of a parameter that supplies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Literal(f64),
    Param(String),
}

/// A JSON Logic condition over the rule's values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    /// Fires on the bar the first operand goes from at-or-below the second
    /// to above it. Not on every bar it is above.
    #[serde(rename = "cross_above")]
    CrossAbove(Vec<Operand>),
    /// Fires on the bar the first operand goes from above the second to
    /// at-or-below it.
    #[serde(rename = "cross_below")]
    CrossBelow(Vec<Operand>),
    #[serde(rename = ">")]
    Gt(Vec<Operand>),
    #[serde(rename = "<")]
    Lt(Vec<Operand>),
    #[serde(rename = "and")]
    And(Vec<Condition>),
    #[serde(rename = "or")]
    Or(Vec<Condition>),
    #[serde(rename = "!")]
    Not(Box<Condition>),
}

/// Something a condition compares: a named value or a number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Operand {
    Var { var: String },
    Number(f64),
}

/// Why a definition cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("{rule} names parameter {param:?} that nothing supplies")]
    MissingParam { rule: String, param: String },
    #[error("{rule}: {what} must be a whole number of bars between 1 and 10000, got {value}")]
    BadPeriod { rule: String, what: String, value: String },
    #[error("{rule} reads {var:?}, which is not an indicator, a bar field or a parameter")]
    UnknownVar { rule: String, var: String },
    #[error("{rule}: {op} takes exactly two operands, got {got}")]
    Arity { rule: String, op: &'static str, got: usize },
    #[error("{rule}: an empty {op} never holds; say what it should read")]
    Empty { rule: String, op: &'static str },
    #[error("{rule} declares no indicators and no condition can read a bar alone")]
    NoIndicators { rule: String },
}

/// A definition with every parameter supplied: what the engine builds.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub name: String,
    pub interval: BarInterval,
    pub indicators: BTreeMap<String, ResolvedIndicator>,
    /// Operands that named a parameter are numbers here.
    pub entry: Condition,
    pub exit: Option<Condition>,
}

impl Indicator {
    /// This indicator with its periods as counts of bars, the parameters
    /// supplied. `rule` and `name` are for the message when they are not.
    ///
    /// # Errors
    ///
    /// A period naming a parameter `params` does not hold, or a period that
    /// is not a whole number of bars from 1 to 10 000, or a MACD whose fast
    /// average is not faster.
    pub fn resolve(&self, rule: &str, name: &str, params: &BTreeMap<String, f64>) -> Result<ResolvedIndicator, RuleError> {
        let number = |value: &Value| -> Result<f64, RuleError> {
            match value {
                Value::Literal(number) => Ok(*number),
                Value::Param(param) => params
                    .get(param)
                    .copied()
                    .ok_or_else(|| RuleError::MissingParam { rule: rule.to_owned(), param: param.clone() }),
            }
        };
        let period = |what: &str, value: &Value| -> Result<usize, RuleError> {
            let number = number(value)?;
            if !number.is_finite() || number < 1.0 || number.fract() != 0.0 || number > MAX_PERIOD {
                return Err(RuleError::BadPeriod { rule: rule.to_owned(), what: what.to_owned(), value: number.to_string() });
            }
            // Bounded above by MAX_PERIOD, so the cast is exact.
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked just above")]
            Ok(number as usize)
        };
        Ok(match self {
            Self::Sma { input, period: value } => ResolvedIndicator::Sma { input: *input, period: period(name, value)? },
            Self::Ema { input, period: value } => ResolvedIndicator::Ema { input: *input, period: period(name, value)? },
            Self::Atr { period: value } => ResolvedIndicator::Atr { period: period(name, value)? },
            Self::Rsi { input, period: value } => ResolvedIndicator::Rsi { input: *input, period: period(name, value)? },
            Self::Macd { input, fast, slow, signal, line } => {
                let (fast, slow) = (period(name, fast)?, period(name, slow)?);
                // A fast average that is not faster says nothing: the line
                // would be zero or inverted, and a cross on it is noise.
                if fast >= slow {
                    return Err(RuleError::BadPeriod {
                        rule: rule.to_owned(),
                        what: format!("{name}: fast"),
                        value: format!("{fast}, which is not below slow {slow}"),
                    });
                }
                ResolvedIndicator::Macd { input: *input, fast, slow, signal: period(name, signal)?, line: *line }
            }
            Self::Max { input, period: value } => ResolvedIndicator::Max { input: *input, period: period(name, value)? },
            Self::Min { input, period: value } => ResolvedIndicator::Min { input: *input, period: period(name, value)? },
        })
    }
}

/// An indicator with its period as a count of bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedIndicator {
    Sma { input: Input, period: usize },
    Ema { input: Input, period: usize },
    Atr { period: usize },
    Rsi { input: Input, period: usize },
    Macd { input: Input, fast: usize, slow: usize, signal: usize, line: MacdLine },
    Max { input: Input, period: usize },
    Min { input: Input, period: usize },
}

impl ResolvedIndicator {
    /// The indicator as a chart legend names it: `EMA(close, 20)`,
    /// `MACD(close, 12, 26, 9) histogram`.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Sma { input, period } => format!("SMA({}, {period})", input.as_str()),
            Self::Ema { input, period } => format!("EMA({}, {period})", input.as_str()),
            Self::Atr { period } => format!("ATR({period})"),
            Self::Rsi { input, period } => format!("RSI({}, {period})", input.as_str()),
            Self::Macd { input, fast, slow, signal, line } => {
                let which = match line {
                    MacdLine::Macd => "",
                    MacdLine::Signal => " signal",
                    MacdLine::Histogram => " histogram",
                };
                format!("MACD({}, {fast}, {slow}, {signal}){which}", input.as_str())
            }
            Self::Max { input, period } => format!("MAX({}, {period})", input.as_str()),
            Self::Min { input, period } => format!("MIN({}, {period})", input.as_str()),
        }
    }

    /// How many bars it needs before it reports a value — the warmup the
    /// caller must allow for.
    #[must_use]
    pub const fn period(self) -> usize {
        match self {
            Self::Sma { period, .. }
            | Self::Ema { period, .. }
            | Self::Atr { period }
            | Self::Max { period, .. }
            | Self::Min { period, .. } => period,
            // One more than the period: an index over changes needs n+1 values
            // to hold n of them.
            Self::Rsi { period, .. } => period + 1,
            // The signal averages the MACD line, which does not begin until the
            // slow average is warm, so the two warmups add.
            Self::Macd { slow, signal, line, .. } => match line {
                MacdLine::Macd => slow,
                MacdLine::Signal | MacdLine::Histogram => slow + signal - 1,
            },
        }
    }
}

impl Resolved {
    /// Bars every indicator needs before the rule can act at all.
    #[must_use]
    pub fn min_bars(&self) -> usize {
        self.indicators.values().map(|indicator| indicator.period()).max().unwrap_or(1)
    }
}

impl RuleDefinition {
    /// A content address: the same rule hashes the same, and any change to
    /// what it says is a different rule. What a finding is stamped with.
    #[must_use]
    pub fn version(&self) -> String {
        let canonical = serde_json::to_vec(self).unwrap_or_default();
        blake3::hash(&canonical).to_hex().to_string()
    }

    /// Supplies the parameters and checks everything the engine will need.
    ///
    /// `params` override the definition's defaults; a grid's trial is one
    /// such map. Every name a condition reads must be an indicator, a bar
    /// field or a parameter, and every period must be a whole number of
    /// bars.
    ///
    /// # Errors
    ///
    /// [`RuleError`], naming what is wrong.
    pub fn resolve(&self, params: &BTreeMap<String, f64>) -> Result<Resolved, RuleError> {
        let rule = self.name.clone();
        if self.indicators.is_empty() {
            return Err(RuleError::NoIndicators { rule });
        }
        let mut supplied = self.params.clone();
        supplied.extend(params.iter().map(|(name, value)| (name.clone(), *value)));

        let mut indicators = BTreeMap::new();
        for (name, indicator) in &self.indicators {
            indicators.insert(name.clone(), indicator.resolve(&rule, name, &supplied)?);
        }

        let entry = self.entry.resolved(&rule, &indicators, &supplied)?;
        let exit = self.exit.as_ref().map(|exit| exit.resolved(&rule, &indicators, &supplied)).transpose()?;
        Ok(Resolved { name: rule, interval: self.interval, indicators, entry, exit })
    }
}

impl Condition {
    /// The operator's JSON Logic name, for a message.
    #[must_use]
    pub const fn op(&self) -> &'static str {
        match self {
            Self::CrossAbove(_) => "cross_above",
            Self::CrossBelow(_) => "cross_below",
            Self::Gt(_) => ">",
            Self::Lt(_) => "<",
            Self::And(_) => "and",
            Self::Or(_) => "or",
            Self::Not(_) => "!",
        }
    }

    fn resolved(
        &self,
        rule: &str,
        indicators: &BTreeMap<String, ResolvedIndicator>,
        params: &BTreeMap<String, f64>,
    ) -> Result<Self, RuleError> {
        let operand = |operand: &Operand| -> Result<Operand, RuleError> {
            match operand {
                Operand::Number(number) => Ok(Operand::Number(*number)),
                Operand::Var { var } if indicators.contains_key(var) || Input::named(var).is_some() => {
                    Ok(Operand::Var { var: var.clone() })
                }
                Operand::Var { var } => params
                    .get(var)
                    .map(|value| Operand::Number(*value))
                    .ok_or_else(|| RuleError::UnknownVar { rule: rule.to_owned(), var: var.clone() }),
            }
        };
        let pair = |operands: &[Operand]| -> Result<Vec<Operand>, RuleError> {
            if operands.len() != 2 {
                return Err(RuleError::Arity { rule: rule.to_owned(), op: self.op(), got: operands.len() });
            }
            operands.iter().map(operand).collect()
        };
        let each = |conditions: &[Self]| -> Result<Vec<Self>, RuleError> {
            if conditions.is_empty() {
                return Err(RuleError::Empty { rule: rule.to_owned(), op: self.op() });
            }
            conditions.iter().map(|condition| condition.resolved(rule, indicators, params)).collect()
        };
        Ok(match self {
            Self::CrossAbove(operands) => Self::CrossAbove(pair(operands)?),
            Self::CrossBelow(operands) => Self::CrossBelow(pair(operands)?),
            Self::Gt(operands) => Self::Gt(pair(operands)?),
            Self::Lt(operands) => Self::Lt(pair(operands)?),
            Self::And(conditions) => Self::And(each(conditions)?),
            Self::Or(conditions) => Self::Or(each(conditions)?),
            Self::Not(condition) => Self::Not(Box::new(condition.resolved(rule, indicators, params)?)),
        })
    }

    /// A short reading of the condition, for a trade's journal.
    #[must_use]
    pub fn describe(&self) -> String {
        fn operand(operand: &Operand) -> String {
            match operand {
                Operand::Var { var } => var.clone(),
                Operand::Number(number) => number.to_string(),
            }
        }
        fn pair(operands: &[Operand], word: &str) -> String {
            match operands {
                [a, b] => format!("{} {word} {}", operand(a), operand(b)),
                _ => word.to_owned(),
            }
        }
        match self {
            Self::CrossAbove(operands) => pair(operands, "crossed above"),
            Self::CrossBelow(operands) => pair(operands, "crossed below"),
            Self::Gt(operands) => pair(operands, ">"),
            Self::Lt(operands) => pair(operands, "<"),
            Self::And(conditions) => conditions.iter().map(Self::describe).collect::<Vec<_>>().join(" and "),
            Self::Or(conditions) => conditions.iter().map(Self::describe).collect::<Vec<_>>().join(" or "),
            Self::Not(condition) => format!("not ({})", condition.describe()),
        }
    }
}

#[cfg(test)]
mod tests {
    /// The warmup each indicator needs, which is what `min_bars` reports and
    /// what the engine uses to decide how much history to load. Asserted here
    /// rather than beside the arithmetic in `arvo-nautilus`, because this is
    /// the copy anything actually reads — keeping a second one there was how a
    /// dead `Macd::warmup()` came to exist.
    #[test]
    fn the_warmup_of_an_rsi_and_of_a_macd_account_for_what_they_are_built_from() {
        // n bars hold n-1 changes, so an index over 14 changes needs 15 bars.
        assert_eq!(ResolvedIndicator::Rsi { input: Input::Close, period: 14 }.period(), 15);

        let macd = |line| ResolvedIndicator::Macd { input: Input::Close, fast: 12, slow: 26, signal: 9, line };
        // The line is ready when its slow average is.
        assert_eq!(macd(MacdLine::Macd).period(), 26);
        // The signal averages the line, which does not begin until then, so the
        // two warmups add rather than taking the longer of the pair.
        assert_eq!(macd(MacdLine::Signal).period(), 34);
        assert_eq!(macd(MacdLine::Histogram).period(), 34);

        // And min_bars takes the longest across a rule's indicators.
        let rule = RuleDefinition {
            name: "warmup".to_owned(),
            label: "Warmup".to_owned(),
            premise: "Checks the arithmetic.".to_owned(),
            interval: arvo_data::BarInterval::DAILY,
            params: BTreeMap::new(),
            indicators: BTreeMap::from([
                ("fast".to_owned(), Indicator::Ema { input: Input::Close, period: Value::Literal(12.0) }),
                ("osc".to_owned(), Indicator::Rsi { input: Input::Close, period: Value::Literal(14.0) }),
                ("sig".to_owned(), Indicator::Macd {
                    input: Input::Close,
                    fast: Value::Literal(12.0),
                    slow: Value::Literal(26.0),
                    signal: Value::Literal(9.0),
                    line: MacdLine::Signal,
                }),
            ]),
            entry: Condition::Gt(vec![Operand::Var { var: "osc".to_owned() }, Operand::Number(70.0)]),
            exit: None,
            source: None,
        };
        let resolved = rule.resolve(&BTreeMap::new()).expect("it resolves");
        assert_eq!(resolved.min_bars(), 34, "the MACD signal is the longest of the three");
    }

    /// A fast average that is not faster makes the line zero or inverted, and a
    /// cross on it is noise. Refused with that named, rather than run.
    #[test]
    fn a_macd_whose_fast_period_is_not_below_its_slow_one_is_refused() {
        let rule = RuleDefinition {
            name: "inverted".to_owned(),
            label: "Inverted".to_owned(),
            premise: "Should not resolve.".to_owned(),
            interval: arvo_data::BarInterval::DAILY,
            params: BTreeMap::new(),
            indicators: BTreeMap::from([("m".to_owned(), Indicator::Macd {
                input: Input::Close,
                fast: Value::Literal(26.0),
                slow: Value::Literal(12.0),
                signal: Value::Literal(9.0),
                line: MacdLine::Macd,
            })]),
            entry: Condition::Gt(vec![Operand::Var { var: "m".to_owned() }, Operand::Number(0.0)]),
            exit: None,
            source: None,
        };
        let err = rule.resolve(&BTreeMap::new()).expect_err("26 is not below 12");
        let said = err.to_string();
        assert!(said.contains("not below slow"), "the reason is named: {said}");
    }

    use super::*;

    /// The control, written down: the twin of the compiled `sma_cross`.
    pub(crate) const TWIN_CROSS: &str = r#"{
      "name": "twin_cross",
      "label": "Moving-average crossover, as data",
      "premise": "The control, written down instead of compiled.",
      "interval": { "step": 1, "unit": "day" },
      "params": { "fast": 10, "slow": 30 },
      "indicators": {
        "fast": { "kind": "SMA", "period": "fast" },
        "slow": { "kind": "SMA", "period": "slow" }
      },
      "entry": { "cross_above": [ { "var": "fast" }, { "var": "slow" } ] },
      "exit":  { "cross_below": [ { "var": "fast" }, { "var": "slow" } ] }
    }"#;

    fn twin() -> RuleDefinition {
        serde_json::from_str(TWIN_CROSS).expect("the shape in the module docs parses")
    }

    #[test]
    fn the_shape_in_the_docs_parses_resolves_and_round_trips() {
        let rule = twin();
        assert_eq!(rule.interval, BarInterval::DAILY);
        let resolved = rule.resolve(&BTreeMap::new()).expect("defaults suffice");
        assert_eq!(resolved.indicators["fast"], ResolvedIndicator::Sma { input: Input::Close, period: 10 });
        assert_eq!(resolved.min_bars(), 30, "the slow average is the longest look back");
        assert_eq!(resolved.entry.describe(), "fast crossed above slow");

        // A grid's trial overrides the defaults.
        let trial = BTreeMap::from([("fast".to_owned(), 5.0), ("slow".to_owned(), 20.0)]);
        assert_eq!(rule.resolve(&trial).expect("whole numbers").min_bars(), 20);

        let text = serde_json::to_string(&rule).expect("serialises");
        assert_eq!(serde_json::from_str::<RuleDefinition>(&text).expect("parses"), rule);
        assert_eq!(rule.version(), twin().version(), "the same rule hashes the same");
    }

    #[test]
    fn a_parameter_in_a_condition_becomes_a_number_and_an_unknown_name_is_refused() {
        let mut rule = twin();
        rule.params.insert("floor".to_owned(), 100.0);
        rule.entry = Condition::And(vec![
            rule.entry.clone(),
            Condition::Gt(vec![Operand::Var { var: "close".to_owned() }, Operand::Var { var: "floor".to_owned() }]),
        ]);
        let resolved = rule.resolve(&BTreeMap::new()).expect("floor is a parameter");
        assert_eq!(
            resolved.entry,
            Condition::And(vec![
                Condition::CrossAbove(vec![Operand::Var { var: "fast".to_owned() }, Operand::Var { var: "slow".to_owned() }]),
                Condition::Gt(vec![Operand::Var { var: "close".to_owned() }, Operand::Number(100.0)]),
            ])
        );

        rule.entry = Condition::Gt(vec![Operand::Var { var: "rsi".to_owned() }, Operand::Number(70.0)]);
        let refused = rule.resolve(&BTreeMap::new()).expect_err("rsi is nothing here");
        assert_eq!(refused, RuleError::UnknownVar { rule: "twin_cross".to_owned(), var: "rsi".to_owned() });
    }

    #[test]
    fn a_period_without_a_default_and_a_fractional_period_are_refused_by_name() {
        let mut rule = twin();
        rule.params.remove("slow");
        assert_eq!(
            rule.resolve(&BTreeMap::new()).expect_err("slow has no default"),
            RuleError::MissingParam { rule: "twin_cross".to_owned(), param: "slow".to_owned() }
        );
        let trial = BTreeMap::from([("slow".to_owned(), 30.5)]);
        assert!(matches!(rule.resolve(&trial).expect_err("half a bar"), RuleError::BadPeriod { ref what, .. } if what == "slow"));

        rule.entry = Condition::CrossAbove(vec![Operand::Var { var: "fast".to_owned() }]);
        let trial = BTreeMap::from([("slow".to_owned(), 30.0)]);
        assert!(matches!(rule.resolve(&trial).expect_err("one operand"), RuleError::Arity { got: 1, .. }));
        rule.entry = Condition::And(Vec::new());
        assert!(matches!(rule.resolve(&trial).expect_err("empty and"), RuleError::Empty { op: "and", .. }));
    }
}
