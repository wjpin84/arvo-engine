//! Whether a rule is still the rule its finding described, while it trades.
//!
//! A finding says whether a rule worked on its out-of-sample window. The
//! gate's drawdown halt and daily loss limit say whether the *account* has
//! lost too much. Nothing in between said whether the rule was still
//! behaving as the finding said it would — so a session could only be
//! judged once the money ran out. This is the judgement in between (#221):
//! the session's own trades against the finding's out-of-sample
//! expectation, in the research vocabulary, and never a risk limit.
//!
//! Every reason here is a comparison of something observed to something the
//! finding recorded. None of them touches the gate: a Diverging session
//! keeps trading until a person or the agent decides otherwise, because a
//! verdict is a judgement and a halt is a limit, and conflating the two is
//! how a limit ends up lifted by a restart.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Live closed trades before the expectancy comparison is allowed to speak.
///
/// Lower than the research criteria's thirty on purpose: the question is not
/// "is there an edge" (the finding answered that on many more trades) but
/// "does this look like the same distribution", and ten draws against a
/// known mean and deviation is enough to notice a different one while it
/// can still be acted on. Below it the verdict is Inconclusive, which is
/// where every session starts.
pub const MIN_TRADES: usize = 10;
/// Live drawdown beyond this multiple of the finding's out-of-sample maximum
/// is Diverging, at any trade count: a drawdown is money already lost.
pub const DRAWDOWN_FACTOR: f64 = 1.5;
/// The rule firing this many times less, or more, often than in the window
/// is a regime change before it is a loss.
pub const FREQUENCY_FACTOR: f64 = 4.0;
/// Entries the frequency comparison waits for, in either direction, so a
/// quiet first week is not a verdict.
pub const MIN_ENTRIES: f64 = 4.0;
/// Fills before measured slippage is compared to what the finding assumed.
pub const MIN_FILLS: usize = 5;
/// Entries in a regime the finding never traded in, before that is a reason.
pub const MIN_UNSEEN_ENTRIES: usize = 3;

/// What the finding's out-of-sample ledger leads a session to expect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    /// Closed round trips the expectation was drawn from.
    pub trades: usize,
    /// Mean realised profit per closed trade, in account currency.
    pub expectancy: f64,
    /// Sample standard deviation of that profit. Zero when one trade, or
    /// when every trade made the same.
    pub deviation: f64,
    /// The out-of-sample maximum drawdown, as a fraction.
    pub max_drawdown: f64,
    /// Entries per bar of the out-of-sample window, still-open ones included:
    /// how often the rule fires.
    pub entries_per_bar: f64,
    /// The regimes the finding's trades were opened in. Empty when the
    /// ledger has no journal, and then the regime comparison says nothing.
    pub regimes: BTreeSet<String>,
    /// The slippage the finding's cost model assumed, in basis points.
    pub slippage_bps: f64,
}

impl Expectation {
    /// Drawn from a finding's out-of-sample ledger.
    ///
    /// `bars` is how many bars the window held (the equity curve's length),
    /// so the firing rate has a denominator. `None` when nothing closed:
    /// a finding with no completed trade expects nothing a session could be
    /// compared to.
    #[must_use]
    pub fn of(ledger: &[arvo_risk::Trade], max_drawdown: f64, bars: usize, slippage_bps: f64) -> Option<Self> {
        let pnls: Vec<f64> = ledger.iter().filter(|trade| trade.closed.is_some()).map(|trade| trade.pnl).collect();
        if pnls.is_empty() {
            return None;
        }
        let (expectancy, deviation) = mean_and_deviation(&pnls);
        Some(Self {
            trades: pnls.len(),
            expectancy,
            deviation,
            max_drawdown,
            entries_per_bar: if bars == 0 { 0.0 } else { ledger.len() as f64 / bars as f64 },
            regimes: ledger.iter().filter_map(|trade| trade.journal.as_ref()?.regime.clone()).collect(),
            slippage_bps,
        })
    }
}

/// What a session has seen so far.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Observed {
    /// Realised profit of each closed round trip, in order.
    pub pnls: Vec<f64>,
    /// The regime each entry was opened in, still-open ones included.
    /// `None` where the rule had not seen enough bars to say.
    pub entries: Vec<Option<String>>,
    /// The deepest fall from the session's equity peak so far, as a fraction
    /// of starting cash.
    pub drawdown: f64,
    /// Bars pushed through the rule since the session started.
    pub bars: usize,
    pub fills: usize,
    /// Mean adverse slippage across those fills, in basis points.
    pub mean_slippage_bps: f64,
}

/// Why a session is Diverging. Each carries the two numbers compared, so
/// the record and the row say what was seen and what was expected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    /// Live expectancy per trade below the out-of-sample interval.
    ///
    /// One-sided: a rule doing better than its finding is not a reason to
    /// stop it, and if it is doing better because the market changed, the
    /// regime reason is the one that says so.
    Expectancy { live: f64, floor: f64, expected: f64, trades: usize },
    Drawdown { live: f64, expected: f64, factor: f64 },
    /// Entries per bar, live against the window.
    Frequency { live: f64, expected: f64 },
    /// Most entries are landing in a regime the finding never traded in.
    Regime { regime: String, entries: usize },
    Execution { live_bps: f64, assumed_bps: f64 },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expectancy { live, floor, expected, trades } => write!(
                f,
                "expectancy {live:.2} per trade over {trades} trades, below the floor {floor:.2} of the finding's {expected:.2}"
            ),
            Self::Drawdown { live, expected, factor } => write!(
                f,
                "drawdown {:.1}% against the finding's {:.1}% (limit {factor}x)",
                live * 100.0,
                expected * 100.0
            ),
            Self::Frequency { live, expected } => {
                write!(f, "firing {live:.4} entries per bar against the finding's {expected:.4}")
            }
            Self::Regime { regime, entries } => {
                write!(f, "{entries} entries in a {regime} regime the finding never traded in")
            }
            Self::Execution { live_bps, assumed_bps } => {
                write!(f, "slippage {live_bps:.1} bps against the {assumed_bps:.1} bps the finding assumed")
            }
        }
    }
}

/// A session's verdict, in the research vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Live {
    /// The live trades sit inside what the out-of-sample distribution would
    /// produce.
    Holding,
    /// Something has left that range; the reason says what.
    Diverging(Reason),
    /// Too few live trades to say. Where every session starts.
    Inconclusive,
}

impl Live {
    /// `holding`, `diverging` or `inconclusive`, as a status field says it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Holding => "holding",
            Self::Diverging(_) => "diverging",
            Self::Inconclusive => "inconclusive",
        }
    }

    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Diverging(reason) => Some(reason.to_string()),
            Self::Holding | Self::Inconclusive => None,
        }
    }
}

/// The comparison, in the order a person should care: money already lost,
/// then the edge, then the shape of the firing, then where the entries are
/// landing, then what the venue is charging.
#[must_use]
pub fn judge(expected: &Expectation, seen: &Observed) -> Live {
    let ceiling = expected.max_drawdown.max(0.01) * DRAWDOWN_FACTOR;
    if seen.drawdown > ceiling {
        return Live::Diverging(Reason::Drawdown { live: seen.drawdown, expected: expected.max_drawdown, factor: DRAWDOWN_FACTOR });
    }

    let closed = seen.pnls.len();
    if closed >= MIN_TRADES {
        let live = seen.pnls.iter().sum::<f64>() / closed as f64;
        let floor = expected.expectancy - 2.0 * expected.deviation / (closed as f64).sqrt();
        if live < floor {
            return Live::Diverging(Reason::Expectancy { live, floor, expected: expected.expectancy, trades: closed });
        }
    }

    let entries = seen.entries.len() as f64;
    let due = expected.entries_per_bar * seen.bars as f64;
    let live_rate = if seen.bars == 0 { 0.0 } else { entries / seen.bars as f64 };
    let too_quiet = due >= MIN_ENTRIES && entries < due / FREQUENCY_FACTOR;
    let too_busy = entries >= MIN_ENTRIES && entries > due * FREQUENCY_FACTOR;
    if too_quiet || too_busy {
        return Live::Diverging(Reason::Frequency { live: live_rate, expected: expected.entries_per_bar });
    }

    if !expected.regimes.is_empty() {
        let labelled: Vec<&String> = seen.entries.iter().flatten().collect();
        let mut unseen: BTreeMap<&String, usize> = BTreeMap::new();
        for regime in &labelled {
            if !expected.regimes.contains(regime.as_str()) {
                *unseen.entry(regime).or_default() += 1;
            }
        }
        let strangers: usize = unseen.values().sum();
        if let Some((regime, count)) = unseen.into_iter().max_by_key(|(_, count)| *count) {
            if count >= MIN_UNSEEN_ENTRIES && strangers * 2 > labelled.len() {
                return Live::Diverging(Reason::Regime { regime: regime.clone(), entries: count });
            }
        }
    }

    if seen.fills >= MIN_FILLS {
        let limit = (2.0 * expected.slippage_bps).max(expected.slippage_bps + 5.0);
        if seen.mean_slippage_bps > limit {
            return Live::Diverging(Reason::Execution { live_bps: seen.mean_slippage_bps, assumed_bps: expected.slippage_bps });
        }
    }

    if closed >= MIN_TRADES {
        Live::Holding
    } else {
        Live::Inconclusive
    }
}

fn mean_and_deviation(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    if values.len() < 2 {
        return (mean, 0.0);
    }
    let variance = values.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (mean, variance.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finding whose trades made 10 each, give or take 4, one entry every
    /// twenty bars, all in a ranging market, with a 5% drawdown.
    fn expected() -> Expectation {
        Expectation {
            trades: 40,
            expectancy: 10.0,
            deviation: 4.0,
            max_drawdown: 0.05,
            entries_per_bar: 0.05,
            regimes: ["ranging".to_owned()].into_iter().collect(),
            slippage_bps: 5.0,
        }
    }

    fn seen(pnls: &[f64]) -> Observed {
        Observed {
            pnls: pnls.to_vec(),
            entries: pnls.iter().map(|_| Some("ranging".to_owned())).collect(),
            drawdown: 0.0,
            bars: pnls.len() * 20,
            fills: pnls.len() * 2,
            mean_slippage_bps: 4.0,
        }
    }

    #[test]
    fn a_session_starts_inconclusive_and_stays_so_below_the_minimum() {
        assert_eq!(judge(&expected(), &Observed::default()), Live::Inconclusive);
        // Nine losses in a row are not yet a verdict: nine draws is what the
        // minimum exists to wait past.
        let losses = vec![-20.0; MIN_TRADES - 1];
        assert_eq!(judge(&expected(), &seen(&losses)), Live::Inconclusive);
    }

    #[test]
    fn a_steady_loser_diverges_on_expectancy_at_the_minimum_and_not_before() {
        let losses = vec![-20.0; MIN_TRADES];
        match judge(&expected(), &seen(&losses)) {
            Live::Diverging(Reason::Expectancy { live, floor, trades, .. }) => {
                assert!((live + 20.0).abs() < 1e-9);
                assert!(floor > 0.0, "the floor is two standard errors under 10: {floor}");
                assert_eq!(trades, MIN_TRADES);
            }
            other => panic!("expected an expectancy divergence, got {other:?}"),
        }
    }

    #[test]
    fn trades_inside_the_interval_are_holding() {
        let ordinary: Vec<f64> = (0..MIN_TRADES).map(|i| if i % 2 == 0 { 14.0 } else { 7.0 }).collect();
        assert_eq!(judge(&expected(), &seen(&ordinary)), Live::Holding);
    }

    #[test]
    fn doing_better_than_the_finding_is_not_diverging() {
        let better = vec![40.0; MIN_TRADES];
        assert_eq!(judge(&expected(), &seen(&better)), Live::Holding);
    }

    #[test]
    fn a_drawdown_past_the_factor_diverges_at_any_trade_count() {
        let mut one = seen(&[5.0]);
        one.drawdown = 0.08;
        assert!(matches!(judge(&expected(), &one), Live::Diverging(Reason::Drawdown { .. })));
        one.drawdown = 0.07;
        assert_eq!(judge(&expected(), &one), Live::Inconclusive);
    }

    #[test]
    fn a_rule_gone_quiet_diverges_once_enough_entries_were_due() {
        // 80 bars at one entry per twenty: four were due, none came.
        let quiet = Observed { bars: 80, ..Observed::default() };
        assert!(matches!(judge(&expected(), &quiet), Live::Diverging(Reason::Frequency { live, .. }) if live == 0.0));
        // At 60 bars only three were due, so silence is not yet a verdict.
        let early = Observed { bars: 60, ..Observed::default() };
        assert_eq!(judge(&expected(), &early), Live::Inconclusive);
    }

    #[test]
    fn a_rule_firing_far_too_often_diverges() {
        // Five entries in five bars, against one every twenty.
        let busy = Observed { bars: 5, entries: vec![Some("ranging".to_owned()); 5], ..Observed::default() };
        assert!(matches!(judge(&expected(), &busy), Live::Diverging(Reason::Frequency { .. })));
    }

    #[test]
    fn entries_mostly_in_a_regime_the_finding_never_saw_diverge() {
        let mut strange = seen(&[5.0, 5.0, 5.0]);
        strange.entries = vec![Some("trending up".to_owned()); 3];
        strange.bars = 60;
        match judge(&expected(), &strange) {
            Live::Diverging(Reason::Regime { regime, entries }) => {
                assert_eq!(regime, "trending up");
                assert_eq!(entries, 3);
            }
            other => panic!("expected a regime divergence, got {other:?}"),
        }
        // One stranger among familiar entries is not a majority.
        strange.entries = vec![Some("ranging".to_owned()), Some("ranging".to_owned()), Some("trending up".to_owned())];
        assert_eq!(judge(&expected(), &strange), Live::Inconclusive);
    }

    #[test]
    fn a_finding_without_a_journal_says_nothing_about_regimes() {
        let mut blind = expected();
        blind.regimes.clear();
        let mut strange = seen(&[5.0, 5.0, 5.0]);
        strange.entries = vec![Some("trending up".to_owned()); 3];
        strange.bars = 60;
        assert_eq!(judge(&blind, &strange), Live::Inconclusive);
    }

    #[test]
    fn slippage_well_past_the_assumption_diverges_after_enough_fills() {
        let mut costly = seen(&[5.0, 5.0, 5.0]);
        costly.mean_slippage_bps = 12.0;
        assert!(matches!(judge(&expected(), &costly), Live::Diverging(Reason::Execution { .. })));
        costly.fills = MIN_FILLS - 1;
        assert_eq!(judge(&expected(), &costly), Live::Inconclusive);
    }

    #[test]
    fn an_expectation_is_drawn_from_closed_trades_only() {
        use arvo_risk::{Direction, ExitReason, Journal, Trade};
        let at = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let trade = |pnl: f64, closed: bool, regime: &str| Trade {
            instrument: String::new(),
            opened: at,
            closed: closed.then_some(at),
            direction: Direction::Long,
            quantity: 1.0,
            entry: 100.0,
            exit: closed.then_some(100.0 + pnl),
            pnl,
            commission: 0.0,
            exit_reason: ExitReason::Signal,
            journal: Some(Journal { rule: String::new(), signal: 0.0, regime: Some(regime.to_owned()), asked: 1.0 }),
        };
        let ledger = [trade(8.0, true, "ranging"), trade(12.0, true, "trending up"), trade(99.0, false, "ranging")];
        let drawn = Expectation::of(&ledger, 0.1, 30, 5.0).unwrap();
        assert_eq!(drawn.trades, 2);
        assert!((drawn.expectancy - 10.0).abs() < 1e-9);
        assert!((drawn.deviation - 8.0_f64.sqrt()).abs() < 1e-9);
        assert!((drawn.entries_per_bar - 0.1).abs() < 1e-9, "the open trade still counts as an entry");
        assert_eq!(drawn.regimes.len(), 2);
        assert!(Expectation::of(&[trade(1.0, false, "ranging")], 0.1, 30, 5.0).is_none());
    }

    #[test]
    fn verdicts_serialise_with_their_reason_flat() {
        let diverging = Live::Diverging(Reason::Drawdown { live: 0.1, expected: 0.05, factor: 1.5 });
        let json = serde_json::to_value(&diverging).unwrap();
        assert_eq!(json["verdict"], "diverging");
        assert_eq!(json["reason"], "drawdown");
        assert_eq!(serde_json::to_value(Live::Holding).unwrap()["verdict"], "holding");
    }
}
