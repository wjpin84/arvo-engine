//! A rule written as data (#225), run as a strategy.
//!
//! The same shape as every compiled rule in [`super::rules`]: the stop is
//! checked before the signal, sizing and closing are [`super::Managed`]'s, and
//! the bar's indicators are fed only after the stop and the halt have had
//! their say — which is the order [`super::SmaCross`] uses, and the reason a
//! data twin of it books the same trades to the cent.
//!
//! The conditions are evaluated as JSON Logic with two stateful operators.
//! `cross_above` and `cross_below` remember which side they were on and fire
//! on the bar that changes, never while a value is still unknown, and forget
//! after a stop or a halt so the next entry needs a fresh crossing rather than
//! one still technically in force.

use std::collections::{BTreeMap, VecDeque};

use arvo_research::rule::{Condition, Input, Operand, Resolved, ResolvedIndicator};
use nautilus_common::actor::DataActor;
use nautilus_model::{
    data::{Bar, BarType},
    identifiers::InstrumentId,
    types::Quantity,
};
use nautilus_trading::strategy::StrategyCore;

use super::{
    indicator::{Atr, Ema, Macd, MacdLine, Rsi, Sma},
    managed_strategy, Managed, Position, Risk, Trigger, EXIT_SIGNAL,
};

pub(crate) struct DataRule {
    core: StrategyCore,
    bar_type: BarType,
    instrument_id: InstrumentId,
    position: Position,
    /// The risk model's, for sizing the stop, as every rule carries.
    atr: Atr,
    indicators: Vec<(String, Series)>,
    entry: Node,
    exit: Option<Node>,
    label: String,
}

impl DataRule {
    pub(crate) fn new(
        core: StrategyCore,
        bar_type: BarType,
        trade_size: Quantity,
        rule: Resolved,
        risk: Risk,
        correlations: std::sync::Arc<arvo_research::RollingCorrelations>,
    ) -> Self {
        let indicators = rule
            .indicators
            .iter()
            .map(|(name, indicator)| (name.clone(), Series::new(*indicator)))
            .collect();
        Self {
            core,
            bar_type,
            instrument_id: bar_type.instrument_id(),
            atr: Atr::new(risk.model.atr_period),
            position: Position::new(risk, trade_size, correlations),
            indicators,
            label: rule.entry.describe(),
            entry: Node::compile(&rule.entry),
            exit: rule.exit.as_ref().map(Node::compile),
        }
    }

    fn forget_crossings(&mut self) {
        self.entry.reset();
        if let Some(exit) = &mut self.exit {
            exit.reset();
        }
    }
}

managed_strategy!(DataRule);

impl DataActor for DataRule {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_bars(self.bar_type, None, None);
        Ok(())
    }

    fn on_bar(&mut self, bar: &Bar) -> anyhow::Result<()> {
        let fields = Fields::of(bar);
        let atr = self.atr.update(fields.high, fields.low, fields.close);

        self.observe_bar(bar);
        if self.exit_on_levels(fields.high, fields.low)?.is_some() || self.halt_if_drawn_down()? {
            self.forget_crossings();
            return Ok(());
        }
        if self.position.is_halted() {
            return Ok(());
        }

        let mut values: BTreeMap<&str, f64> = BTreeMap::new();
        for input in [Input::Open, Input::High, Input::Low, Input::Close, Input::Volume] {
            values.insert(input.as_str(), fields.get(input));
        }
        for (name, series) in &mut self.indicators {
            if let Some(value) = series.update(&fields) {
                values.insert(name.as_str(), value);
            }
        }

        // Both conditions see every bar, so their memory of which side they
        // were on stays true whether or not the rule could act on them.
        let entered = self.entry.eval(&values);
        let exited = self.exit.as_mut().and_then(|exit| exit.eval(&values));
        if self.position.is_open() {
            if exited == Some(true) {
                return self.close(EXIT_SIGNAL);
            }
        } else if entered == Some(true) {
            let strength = self.entry.strength(&values);
            let trigger = Trigger::named(self.label.clone(), strength);
            self.enter_long(fields.close, atr, None, trigger)?;
        }
        Ok(())
    }
}

/// One bar's fields as numbers.
struct Fields {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl Fields {
    fn of(bar: &Bar) -> Self {
        Self {
            open: bar.open.as_f64(),
            high: bar.high.as_f64(),
            low: bar.low.as_f64(),
            close: bar.close.as_f64(),
            volume: bar.volume.as_f64(),
        }
    }

    const fn get(&self, input: Input) -> f64 {
        match input {
            Input::Open => self.open,
            Input::High => self.high,
            Input::Low => self.low,
            Input::Close => self.close,
            Input::Volume => self.volume,
        }
    }
}

/// A live indicator.
enum Series {
    Sma(Input, Sma),
    Ema(Input, Ema),
    Atr(Atr),
    Rsi(Input, Rsi),
    Macd(Input, Macd),
    Extreme(Input, Rolling),
}

/// The rule language's choice of MACD series, as the indicator names it.
///
/// Two enums rather than one because the rule language is a persisted format
/// and the indicator is an implementation detail; this is the one place they
/// meet, so a new line has to be added here to compile.
const fn macd_line(line: arvo_research::rule::MacdLine) -> MacdLine {
    match line {
        arvo_research::rule::MacdLine::Macd => MacdLine::Macd,
        arvo_research::rule::MacdLine::Signal => MacdLine::Signal,
        arvo_research::rule::MacdLine::Histogram => MacdLine::Histogram,
    }
}

impl Series {
    fn new(indicator: ResolvedIndicator) -> Self {
        match indicator {
            ResolvedIndicator::Sma { input, period } => Self::Sma(input, Sma::new(period)),
            ResolvedIndicator::Ema { input, period } => Self::Ema(input, Ema::new(period)),
            ResolvedIndicator::Atr { period } => Self::Atr(Atr::new(period)),
            ResolvedIndicator::Rsi { input, period } => Self::Rsi(input, Rsi::new(period)),
            ResolvedIndicator::Macd { input, fast, slow, signal, line } => {
                Self::Macd(input, Macd::new(fast, slow, signal, macd_line(line)))
            }
            ResolvedIndicator::Max { input, period } => Self::Extreme(input, Rolling::new(period, true)),
            ResolvedIndicator::Min { input, period } => Self::Extreme(input, Rolling::new(period, false)),
        }
    }

    fn update(&mut self, fields: &Fields) -> Option<f64> {
        match self {
            Self::Sma(input, sma) => sma.update(fields.get(*input)),
            Self::Ema(input, ema) => ema.update(fields.get(*input)),
            Self::Atr(atr) => atr.update(fields.high, fields.low, fields.close),
            Self::Rsi(input, rsi) => rsi.update(fields.get(*input)),
            Self::Macd(input, macd) => macd.update(fields.get(*input)),
            Self::Extreme(input, rolling) => rolling.update(fields.get(*input)),
        }
    }
}

/// The highest or lowest of the last `period` values.
struct Rolling {
    period: usize,
    max: bool,
    window: VecDeque<f64>,
}

impl Rolling {
    fn new(period: usize, max: bool) -> Self {
        Self { period, max, window: VecDeque::with_capacity(period) }
    }

    fn update(&mut self, value: f64) -> Option<f64> {
        self.window.push_back(value);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        if self.window.len() < self.period {
            return None;
        }
        // ponytail: a scan per bar over at most 10000 values; a monotonic
        // deque if a rule with a long window ever shows up in a profile.
        let fold = if self.max { f64::max } else { f64::min };
        self.window.iter().copied().reduce(fold)
    }
}

/// A condition compiled for evaluation, with the crossings' memory.
enum Node {
    Cross { above: bool, a: Operand, b: Operand, previous: Option<bool> },
    Compare { greater: bool, a: Operand, b: Operand },
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
}

impl Node {
    fn compile(condition: &Condition) -> Self {
        let pair = |operands: &[Operand]| -> (Operand, Operand) {
            // Arity was checked when the definition resolved.
            (operands.first().cloned().unwrap_or(Operand::Number(0.0)), operands.get(1).cloned().unwrap_or(Operand::Number(0.0)))
        };
        match condition {
            Condition::CrossAbove(operands) => {
                let (a, b) = pair(operands);
                Self::Cross { above: true, a, b, previous: None }
            }
            Condition::CrossBelow(operands) => {
                let (a, b) = pair(operands);
                Self::Cross { above: false, a, b, previous: None }
            }
            Condition::Gt(operands) => {
                let (a, b) = pair(operands);
                Self::Compare { greater: true, a, b }
            }
            Condition::Lt(operands) => {
                let (a, b) = pair(operands);
                Self::Compare { greater: false, a, b }
            }
            Condition::And(conditions) => Self::All(conditions.iter().map(Self::compile).collect()),
            Condition::Or(conditions) => Self::Any(conditions.iter().map(Self::compile).collect()),
            Condition::Not(condition) => Self::Not(Box::new(Self::compile(condition))),
        }
    }

    /// Whether the condition holds on this bar; `None` while something it
    /// reads is not yet known, in which case no memory is touched.
    fn eval(&mut self, values: &BTreeMap<&str, f64>) -> Option<bool> {
        match self {
            Self::Cross { above, a, b, previous } => {
                let now = read(a, values)? > read(b, values)?;
                let before = previous.replace(now);
                Some(if *above { before == Some(false) && now } else { before == Some(true) && !now })
            }
            Self::Compare { greater, a, b } => {
                let (a, b) = (read(a, values)?, read(b, values)?);
                Some(if *greater { a > b } else { a < b })
            }
            // Every child is evaluated, so each keeps its memory current.
            Self::All(children) => children.iter_mut().map(|child| child.eval(values)).collect::<Option<Vec<_>>>().map(|held| held.iter().all(|held| *held)),
            Self::Any(children) => children.iter_mut().map(|child| child.eval(values)).collect::<Option<Vec<_>>>().map(|held| held.iter().any(|held| *held)),
            Self::Not(child) => child.eval(values).map(|held| !held),
        }
    }

    /// How strongly the condition holds: the gap between the first
    /// comparison's operands, which is what the journal records as the
    /// signal, as the compiled crossover records `fast - slow`.
    fn strength(&self, values: &BTreeMap<&str, f64>) -> f64 {
        match self {
            Self::Cross { a, b, .. } | Self::Compare { a, b, .. } => {
                read(a, values).zip(read(b, values)).map_or(0.0, |(a, b)| a - b)
            }
            Self::All(children) | Self::Any(children) => children.first().map_or(0.0, |child| child.strength(values)),
            Self::Not(child) => -child.strength(values),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Cross { previous, .. } => *previous = None,
            Self::Compare { .. } => {}
            Self::All(children) | Self::Any(children) => children.iter_mut().for_each(Self::reset),
            Self::Not(child) => child.reset(),
        }
    }
}

fn read(operand: &Operand, values: &BTreeMap<&str, f64>) -> Option<f64> {
    match operand {
        Operand::Number(number) => Some(*number),
        Operand::Var { var } => values.get(var.as_str()).copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str) -> Operand {
        Operand::Var { var: name.to_owned() }
    }

    #[test]
    fn a_crossing_fires_once_on_the_bar_it_happens_and_not_before_both_sides_are_known() {
        let mut node = Node::compile(&Condition::CrossAbove(vec![var("fast"), var("slow")]));
        let at = |fast: Option<f64>, slow: f64| {
            let mut values = BTreeMap::from([("slow", slow)]);
            if let Some(fast) = fast {
                values.insert("fast", fast);
            }
            values
        };
        assert_eq!(node.eval(&at(None, 10.0)), None, "the fast average has not filled");
        assert_eq!(node.eval(&at(Some(9.0), 10.0)), Some(false), "below: remembered, nothing fires");
        assert_eq!(node.eval(&at(Some(11.0), 10.0)), Some(true), "the bar it crosses");
        assert_eq!(node.eval(&at(Some(12.0), 10.0)), Some(false), "still above is not a crossing");
        node.reset();
        assert_eq!(node.eval(&at(Some(12.0), 10.0)), Some(false), "after a stop the first look only remembers");
        assert_eq!(node.eval(&at(Some(9.0), 10.0)), Some(false));
        assert_eq!(node.eval(&at(Some(11.0), 10.0)), Some(true), "a fresh crossing");
        assert!((node.strength(&at(Some(11.0), 10.0)) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn and_or_not_combine_and_a_rolling_extreme_looks_back_exactly_its_period() {
        let mut node = Node::compile(&Condition::And(vec![
            Condition::Gt(vec![var("close"), Operand::Number(100.0)]),
            Condition::Not(Box::new(Condition::Lt(vec![var("close"), var("floor")]))),
        ]));
        assert_eq!(node.eval(&BTreeMap::from([("close", 101.0), ("floor", 90.0)])), Some(true));
        assert_eq!(node.eval(&BTreeMap::from([("close", 101.0), ("floor", 105.0)])), Some(false));
        assert_eq!(node.eval(&BTreeMap::from([("close", 101.0)])), None, "floor unknown");

        let mut high = Rolling::new(3, true);
        assert_eq!(high.update(1.0), None);
        assert_eq!(high.update(5.0), None);
        assert_eq!(high.update(2.0), Some(5.0));
        assert_eq!(high.update(3.0), Some(5.0));
        assert_eq!(high.update(4.0), Some(4.0), "the 5 has left the window");
    }
}
