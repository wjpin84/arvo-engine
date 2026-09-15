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

mod panel;
mod study;
mod walk_forward;

pub use panel::recommend_panel;
pub use study::recommend;
pub use walk_forward::recommend_walk_forward;

use serde::{Deserialize, Serialize};

use crate::TradeStats;

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

/// Below this share of a panel's members beating their own benchmark, a
/// positive pooled average is being carried by a minority of them.
///
/// Half, because that is where "it worked on these instruments" stops being a
/// fair description: the same average comes from every member edging ahead and
/// from one member carrying five, and the mean cannot tell those apart.
const PANEL_MAJORITY: f64 = 0.5;

/// Below this confidence, a positive Sharpe has not been distinguished from
/// no edge at all.
///
/// 0.95, the ordinary 5% bar, and deliberately not softer. The platform's
/// whole premise is that most results are noise; a threshold chosen to let
/// more through would be arguing with the premise rather than applying it.
const PSR_WORTH_BELIEVING: f64 = 0.95;

/// Above this share of the window spent holding, the dividend gap between a
/// strategy and buy-and-hold is too small to be worth a line.
///
/// A strategy in the market 95% of the time forgoes 95% of the dividends the
/// benchmark also forgoes, so the two are biased almost identically and the
/// comparison is very nearly fair. Below it, the gap grows with every day out.
const EXPOSURE_WORTH_SAYING: f64 = 0.9;

/// Below this modal share, a parameter axis is being chosen at random.
///
/// If the most commonly selected value on an axis wins fewer than half the
/// folds, re-selection is not converging on anything: the axis is adding
/// search width — and therefore raising the deflation bar — without adding an
/// answer.
const MODAL_SHARE_FLOOR: f64 = 0.5;

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

/// When the run trades options: the losses first, and what the history cannot
/// contain (#85).
///
/// # The sample
///
/// Option bars reach back to January 2024 and no further. That window holds
/// the August 2024 and April 2025 volatility spikes and nothing of the scale of
/// March 2020 or 2022 — so a strategy short volatility has not been tested
/// against the kind of day that ends such strategies, and its worst period here
/// is a floor on its worst period, not an estimate of it.
///
/// ponytail: the dates are the Alpaca archive's today; read them from the
/// library's first option bar if a second vendor reaches further back.
fn option_tail(
    experiment: &crate::Experiment,
    curve: &[crate::EquityPoint],
    ledger: &[crate::Trade],
    stress: Option<&crate::stress::Stress>,
) -> Vec<Recommendation> {
    // An option run is one whose instrument is a contract, or one that traded
    // contracts while reading something else — a put spread runs on SPY and
    // holds nothing but puts (#86).
    let options = arvo_data::option::OptionContract::parse(&experiment.instrument).is_some()
        || ledger
            .iter()
            .any(|trade| arvo_data::option::OptionContract::parse(&trade.instrument).is_some());
    if !options {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(worst) = stress.and_then(crate::stress::Stress::worst) {
        let share = worst.loss / experiment.starting_cash;
        out.push(Recommendation::new(
            if share >= 0.10 { Severity::Blocking } else { Severity::Warning },
            "On the market's worst day, one position held here would have lost this.",
            "Read this before any return. It is a floor: puts are priced at the VIX, not at \
             the steeper volatility a crash gives them, and the whole day lands at once.",
            format!(
                "{} ({}): SPY {:+.1}%, VIX {:.0} -> {} opened {} loses {:.0} ({:.1}% of the \
                 account){}",
                worst.day,
                worst.what,
                worst.spot_move * 100.0,
                worst.vix,
                worst.position,
                worst.opened,
                worst.loss,
                share * 100.0,
                if worst.credit > 0.0 {
                    format!(", {:.1}x the credit it took in", worst.loss / worst.credit)
                } else {
                    String::new()
                },
            ),
        ));
    }
    if let Some(tail) = crate::evaluation::tail(curve) {
        out.push(Recommendation::new(
            Severity::Warning,
            "An option strategy's risk is in its worst periods, and a Sharpe ratio averages them away.",
            "Read these before the return. Ask whether the account survives the worst period \
             happening twice in a row, not whether the average is good.",
            format!(
                "worst {} {:+.1}%, worst month {}, mean of the worst 5% of periods {:+.1}%",
                experiment.interval,
                tail.worst_period * 100.0,
                tail.worst_month
                    .map_or("n/a (under two months)".to_owned(), |m| format!("{:+.1}%", m * 100.0)),
                tail.expected_shortfall * 100.0,
            ),
        ));
    }
    out.push(Recommendation::new(
        Severity::Warning,
        "The option history this ran on holds no crash.",
        "Treat the worst period above as a floor. Before trusting a strategy that is short \
         options, find out what it would have lost on a day like 16 March 2020.",
        format!(
            "option bars from January 2024 to {}: the August 2024 and April 2025 spikes, \
             nothing of 2020 or 2022 scale",
            experiment.window.to
        ),
    ));
    out
}

/// A rule that lost money and beat a benchmark that lost more, as the blocking
/// item it is. See `evaluation::losing_reason`.
fn lost_money(strategy_return: f64, excess_return: f64) -> Option<Recommendation> {
    (strategy_return < 0.0 && excess_return >= 0.0).then(|| {
        Recommendation::new(
            Severity::Blocking,
            "It lost money, and beat holding only because holding lost more.",
            "Do not read the margin over buy-and-hold as an edge. A rule sitting mostly in cash \
             beats a falling market by doing nothing; judge it on a window where the market \
             rose as well, or on what it made rather than what it avoided.",
            format!(
                "strategy {:+.2}%, buy-and-hold {:+.2}%",
                strategy_return * 100.0,
                (strategy_return - excess_return) * 100.0
            ),
        )
    })
}

/// Orders the venue refused, as the blocking item they are.
///
/// Above the trade count on purpose. A result with too few trades says the
/// evidence is thin; one whose entries were refused says the run was not the
/// rule — it skipped signals it could not pay for, so its trades, its return
/// and its verdict all describe a different, starved strategy.
fn refusals(refused: crate::Refused) -> Option<Recommendation> {
    refused.any().then(|| {
        Recommendation::new(
            Severity::Blocking,
            "The venue refused orders this run tried to place.",
            "Do not read the trades or the return as the rule's. Every refused entry is a signal that was never acted on, so this measured a different strategy. The usual cause is an entry sized beyond the cash on hand; lower the position cap or the risk per trade, or give the account more capital, and run it again.",
            format!(
                "{} entr{} and {} exit{} refused",
                refused.entries,
                if refused.entries == 1 { "y" } else { "ies" },
                refused.exits,
                if refused.exits == 1 { "" } else { "s" },
            ),
        )
    })
}

/// A fraction of the window, phrased the way a person would say it.
fn share(fraction: f64) -> String {
    match fraction {
        f if f >= 0.66 => "most".to_owned(),
        f if f >= 0.4 => "about half".to_owned(),
        f if f >= 0.2 => "a third".to_owned(),
        f => format!("{:.0}%", f * 100.0),
    }
}

/// Warnings about the *shape* of the trades, as distinct from their total.
///
/// Only computed on enough closed trades to describe: a win rate over four
/// round trips is a statement about four round trips.
fn shape_warnings(trades: &TradeStats, stopped: bool) -> Vec<Recommendation> {
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

    // Only a rule that has a stop can be told its stop never bound. A put
    // spread has none and was told so anyway.
    if stopped && trades.closed > 0 {
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

#[cfg(test)]
mod tests;
