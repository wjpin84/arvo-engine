//! What to do about a finding.
//!
//! A verdict says whether a rule cleared the bar, and [`Evaluation::reasons`]
//! says why it came out that way. Neither says what to do next, and that gap
//! is where a research tool stops being useful: "NotSupported, 23 trades,
//! profit factor 0.30" is three facts a reader has to assemble themselves,
//! every time, correctly.
//!
//! # What this is not
//!
//! Not advice about markets. Nothing here says buy, sell, hold, or size a
//! position, and nothing here predicts anything. Every recommendation is an
//! instruction about the **research** — get more data, shrink the grid, test
//! the exit, check whether costs are the finding — and every one of them is a
//! mechanical consequence of a threshold stated in this file. A reader can
//! check any of them against the evidence line it carries.
//!
//! That restraint is the point. A platform whose thesis is that most backtest
//! results are noise cannot also be the thing that tells you what to buy.
//!
//! [`Evaluation::reasons`]: crate::Evaluation

use serde::{Deserialize, Serialize};

use crate::{FamilyEvidence, TradeStats, Verdict};

/// Below this many closed trades, a shape statistic is describing a handful of
/// coin flips. Separate from `EvaluationCriteria::min_trades`, which decides a
/// verdict; this decides whether it is worth *commenting* on a win rate.
const MIN_TRADES_TO_CHARACTERISE: u32 = 10;

/// A single trade producing more than this share of gross profit means the
/// average is describing that trade rather than the rule.
const CONCENTRATION: f64 = 0.5;

/// Above this share of exits being stops, the exit rule is not what is ending
/// positions.
const STOP_DOMINANCE: f64 = 0.7;

/// Fees above this share of the gross return are a material part of the
/// result rather than a rounding detail.
const FEE_SHARE: f64 = 0.25;

/// Days a position must be held for long-term US capital-gains treatment.
const LONG_TERM_DAYS: f64 = 365.0;

/// How much a recommendation should stop someone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The result cannot be read at all. Anything concluded from it is
    /// unsupported regardless of how good the numbers look.
    Blocking,
    /// The result is readable but rests on something fragile.
    Warning,
    /// Worth knowing, and not a problem.
    Note,
}

impl Severity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Warning => "warning",
            Self::Note => "note",
        }
    }
}

/// One thing to do, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub severity: Severity,
    /// What the numbers say, stated flatly and in the past tense.
    pub finding: String,
    /// What to do about it, as an instruction.
    pub action: String,
    /// The figures it was derived from, so the reader can disagree with it.
    pub evidence: String,
}

impl Recommendation {
    fn new(severity: Severity, finding: &str, action: &str, evidence: String) -> Self {
        Self {
            severity,
            finding: finding.to_owned(),
            action: action.to_owned(),
            evidence,
        }
    }
}

/// Everything worth saying about a finding, most stopping first.
///
/// Derived on read rather than stored: these are a function of the evidence
/// and the rules in this file, and a stored recommendation would go quietly
/// stale the first time a threshold changed. The evidence is the record; this
/// is a reading of it.
#[must_use]
pub fn recommend(found: &FamilyEvidence) -> Vec<Recommendation> {
    let evaluation = &found.out_of_sample_evidence.evaluation;
    let criteria = &found.out_of_sample_evidence.criteria;
    let trades = &evaluation.strategy_trades;
    let mut out = Vec::new();

    // ---- blocking: the result cannot be read -----------------------------

    if evaluation.strategy.trades < criteria.min_trades {
        out.push(Recommendation::new(
            Severity::Blocking,
            "Too few round trips to tell skill from luck.",
            "Widen the out-of-sample window, or loosen the entry so the rule \
             fires more often. Do not read the return until it does.",
            format!(
                "{} trades against a {} minimum",
                evaluation.strategy.trades, criteria.min_trades
            ),
        ));
    }

    if !found.selection.survived_deflation {
        out.push(Recommendation::new(
            Severity::Blocking,
            "The search explains the winner.",
            "Shrink the grid or lengthen the in-sample window, then re-run. \
             Picking the best of a large search is not evidence that the best \
             one works.",
            match found.selection.expected_best_under_null {
                Some(expected) => format!(
                    "best in-sample Sharpe {:.2} across {} configurations, against {expected:.2} \
                     expected from a no-skill search of that size",
                    found.selection.best_sharpe, found.selection.trials
                ),
                None => format!(
                    "{} configurations, too few to say what a no-skill search would produce",
                    found.selection.trials
                ),
            },
        ));
    }

    if !found.failures.is_empty() {
        out.push(Recommendation::new(
            Severity::Blocking,
            "Part of the grid never ran.",
            "Fix the failing configurations before reading the winner. A grid \
             where some trials errored is a different search from the one the \
             deflation check was computed against.",
            format!("{} configurations failed to run", found.failures.len()),
        ));
    }

    // ---- warning: readable, but resting on something fragile -------------

    out.extend(shape_warnings(trades));

    if trades.halted {
        out.push(Recommendation::new(
            Severity::Blocking,
            "The run stopped early: the account hit its drawdown limit.",
            "Read every number here as covering a window that was not              finished. The return is what it was at the halt, not what the              rule would have made — and the rule was stopped precisely where              it was going worst, so what came after is unmeasured.",
            format!(
                "{} round trips before the limit was reached",
                trades.closed
            ),
        ));
    }

    if trades.still_open > 0 {
        out.push(Recommendation::new(
            Severity::Warning,
            "The run ended holding a position.",
            "Treat the tail of the curve as provisional: that part of the \
             return is marked to market, not realised, and a stop was never \
             tested against it.",
            format!(
                "{} of {} positions still open at the end of the window",
                trades.still_open,
                trades.closed + trades.still_open
            ),
        ));
    }

    if let Some(fees) = fee_share(evaluation.strategy.total_return, trades, found) {
        out.push(Recommendation::new(
            Severity::Warning,
            "Costs are a material part of this result.",
            "Check the cost assumptions against a real broker schedule before \
             concluding anything. At this share, a wrong fee or slippage \
             figure changes the verdict rather than the third decimal.",
            fees,
        ));
    }

    if evaluation.strategy.max_drawdown > criteria.max_drawdown {
        out.push(Recommendation::new(
            Severity::Warning,
            "The drawdown exceeded what the criteria call holdable.",
            "Judge the return against whether it was actually sittable \
             through. Add a drawdown halt, or accept the ceiling was the \
             wrong one and say so.",
            format!(
                "worst drawdown {:.1}% against a {:.1}% ceiling",
                evaluation.strategy.max_drawdown * 100.0,
                criteria.max_drawdown * 100.0
            ),
        ));
    }

    // ---- notes -----------------------------------------------------------

    let long_enough_to_characterise = trades.closed >= MIN_TRADES_TO_CHARACTERISE;
    if let Some(days) = trades
        .average_holding_secs
        .map(|secs| secs / 86_400.0)
        .filter(|_| long_enough_to_characterise)
    {
        let (finding, action) = if days < LONG_TERM_DAYS {
            (
                "Positions were held for less than a year on average.",
                "In a taxable account these gains fall under short-term \
                 treatment. The backtest does not model that, so the \
                 after-tax result is worse than the figure shown — by how \
                 much depends on your bracket and account type.",
            )
        } else {
            (
                "Positions were held for more than a year on average.",
                "In a taxable account this is long-term treatment, which is \
                 the favourable case. Still not modelled here, so read the \
                 return as pre-tax.",
            )
        };
        out.push(Recommendation::new(
            Severity::Note,
            finding,
            action,
            format!("{days:.0} days on average across {} trades", trades.closed),
        ));
    }

    if out.is_empty() && found.verdict == Verdict::Supported {
        out.push(Recommendation::new(
            Severity::Note,
            "Nothing in the evidence undercuts this one.",
            "Re-run it on a different instrument or a later window before \
             believing it. One surviving finding is where the work starts, \
             not where it ends.",
            format!(
                "{} trades, {:.1}% excess over buy-and-hold",
                evaluation.strategy.trades,
                evaluation.excess_return * 100.0
            ),
        ));
    }

    out.sort_by_key(|item| item.severity);
    out
}

/// Warnings about the *shape* of the trades, as distinct from their total.
///
/// Only computed on enough closed trades to describe: a win rate over four
/// round trips is a statement about four round trips.
fn shape_warnings(trades: &TradeStats) -> Vec<Recommendation> {
    let mut out = Vec::new();
    if trades.closed < MIN_TRADES_TO_CHARACTERISE {
        return out;
    }

    if let (Some(rate), Some(expectancy)) = (trades.win_rate, trades.expectancy()) {
        if expectancy < 0.0 && rate >= 0.5 {
        out.push(Recommendation::new(
            Severity::Warning,
            "It wins more often than it loses and still loses money.",
            "The exit is the problem, not the entry. Losers are running \
             further than winners — look at the stop distance and at what \
             ends a winning trade.",
            format!(
                "{:.0}% win rate, expectancy {expectancy:+.0} per trade",
                rate * 100.0
            ),
        ));
        }
    }

    // One trade carrying the result. Gross profit is reconstructed from the
    // mean rather than stored, which is exact: mean times count.
    if let (Some(average_win), Some(largest)) = (trades.average_win, trades.largest_win) {
        let gross = average_win * f64::from(trades.wins);
        if trades.wins > 1
            && gross > 0.0
            && largest / gross > CONCENTRATION
        {
            out.push(Recommendation::new(
                Severity::Warning,
                "One trade produced most of the profit.",
                "Re-run without that trade's window before believing the \
                 average. An edge that survives only because of a single \
                 outcome has not been shown to repeat.",
                format!(
                    "largest win {largest:+.0} of {gross:+.0} gross profit ({:.0}%) across {} \
                     winners",
                    largest / gross * 100.0,
                    trades.wins
                ),
            ));
        }
    }

    if trades.closed > 0 {
        let stop_share = f64::from(trades.stop_exits) / f64::from(trades.closed);
        if stop_share > STOP_DOMINANCE {
            out.push(Recommendation::new(
                Severity::Warning,
                "Almost every position ended at the stop, not on a signal.",
                "The exit rule is barely firing, so what is being tested is \
                 the stop. Widen the stop or fix the exit — until then the \
                 result says little about the idea.",
                format!(
                    "{} of {} exits were stops ({:.0}%)",
                    trades.stop_exits,
                    trades.closed,
                    stop_share * 100.0
                ),
            ));
        } else if trades.stop_exits == 0 {
            out.push(Recommendation::new(
                Severity::Note,
                "The stop never bound.",
                "The left tail is untested: this curve is what the rule does \
                 when nothing goes badly wrong. Do not read the drawdown as \
                 evidence the stop works.",
                format!("0 of {} exits were stops", trades.closed),
            ));
        }
    }

    out
}

/// Fees as a share of the gross return, when that share is material.
///
/// Gross rather than net: the reported return already has fees taken out, so
/// comparing fees to it would understate them — most severely in the case
/// that matters, where fees are what turned a positive result negative.
fn fee_share(net_return: f64, trades: &TradeStats, found: &FamilyEvidence) -> Option<String> {
    let capital = found.selected.starting_cash;
    if capital <= 0.0 || trades.total_commission <= 0.0 {
        return None;
    }
    let fee_fraction = trades.total_commission / capital;
    let gross = net_return + fee_fraction;
    if gross.abs() < f64::EPSILON || fee_fraction / gross.abs() < FEE_SHARE {
        return None;
    }
    Some(format!(
        "{:.0} in fees is {:.0}% of the {:.1}% gross return, leaving {:.1}% net \
         — and slippage is on top, inside the fill prices",
        trades.total_commission,
        fee_fraction / gross.abs() * 100.0,
        gross * 100.0,
        net_return * 100.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, ExitReason, Trade};

    fn trade(day: u32, held: i64, pnl: f64, reason: ExitReason) -> Trade {
        let opened = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
            + chrono::Duration::days(i64::from(day));
        Trade {
            opened,
            closed: Some(opened + chrono::Duration::days(held)),
            direction: Direction::Long,
            quantity: 100.0,
            entry: 10.0,
            exit: Some(11.0),
            pnl,
            commission: 1.0,
            exit_reason: reason,
        }
    }

    #[test]
    fn a_high_win_rate_that_loses_money_is_named_as_an_exit_problem() {
        // Six small wins, five large losses: wins more often than it loses,
        // loses money. The one shape a win rate alone always misreads.
        let mut ledger: Vec<Trade> = (0..6)
            .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
            .collect();
        ledger.extend((6..11).map(|day| trade(day, 3, -100.0, ExitReason::Stop)));

        let warnings = shape_warnings(&TradeStats::from_ledger(&ledger));
        assert!(
            warnings.iter().any(|r| r.action.contains("exit is the problem")),
            "{warnings:#?}"
        );
    }

    #[test]
    fn one_trade_carrying_the_profit_is_called_out() {
        let mut ledger: Vec<Trade> = (0..10)
            .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
            .collect();
        ledger.push(trade(11, 3, 5_000.0, ExitReason::Signal));

        let warnings = shape_warnings(&TradeStats::from_ledger(&ledger));
        let concentration = warnings
            .iter()
            .find(|r| r.finding.contains("One trade"))
            .expect("a single trade produced 98% of profit");
        assert_eq!(concentration.severity, Severity::Warning);
        assert!(concentration.evidence.contains('%'), "cite the share");
    }

    #[test]
    fn shape_is_not_characterised_on_too_few_trades() {
        // Four round trips, one of which is 90% of the profit. True, and not
        // worth saying: it is a statement about four round trips.
        let mut ledger: Vec<Trade> = (0..3)
            .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
            .collect();
        ledger.push(trade(4, 3, 5_000.0, ExitReason::Signal));

        assert!(
            shape_warnings(&TradeStats::from_ledger(&ledger)).is_empty(),
            "a handful of trades has no shape to describe"
        );
    }

    #[test]
    fn a_stop_that_never_bound_is_a_note_not_a_warning() {
        let ledger: Vec<Trade> = (0..12)
            .map(|day| trade(day, 3, 10.0, ExitReason::Signal))
            .collect();
        let notes = shape_warnings(&TradeStats::from_ledger(&ledger));
        let untested = notes
            .iter()
            .find(|r| r.finding.contains("stop never bound"))
            .expect("no exit was a stop");
        assert_eq!(untested.severity, Severity::Note);
    }

    #[test]
    fn stops_ending_almost_everything_is_a_warning() {
        let mut ledger: Vec<Trade> = (0..10)
            .map(|day| trade(day, 3, -10.0, ExitReason::Stop))
            .collect();
        ledger.push(trade(11, 3, 10.0, ExitReason::Signal));

        let warnings = shape_warnings(&TradeStats::from_ledger(&ledger));
        assert!(
            warnings.iter().any(|r| r.finding.contains("ended at the stop")),
            "{warnings:#?}"
        );
    }

    #[test]
    fn severity_orders_blocking_before_warning_before_note() {
        // The sort in `recommend` relies on this, and a derived `Ord` follows
        // declaration order — which is easy to break by tidying the enum.
        let mut severities = [Severity::Note, Severity::Blocking, Severity::Warning];
        severities.sort_unstable();
        assert_eq!(
            severities,
            [Severity::Blocking, Severity::Warning, Severity::Note]
        );
    }
}
