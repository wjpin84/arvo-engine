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

use crate::{
    EvaluationCriteria, FamilyEvidence, PanelEvidence, TradeStats, Verdict,
    WalkForwardEvidence,
};

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

/// Orders the venue refused, as the blocking item they are.
///
/// Above the trade count on purpose. A result with too few trades says the
/// evidence is thin; one whose entries were refused says the run was not the
/// rule — it skipped signals it could not pay for, so its trades, its return
/// and its verdict all describe a different, starved strategy.
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

    // First, and above the trade-count bar, because this is a different kind
    // of objection. Everything else here says the evidence is too thin to
    // conclude from; this says the evidence contradicts itself, and no amount
    // of extra data fixes a result whose own two halves disagree.
    for discrepancy in crate::reconcile_parts(
        &found.out_of_sample_evidence.experiment,
        &evaluation.strategy_curve,
        &evaluation.strategy_ledger,
    ) {
        out.push(Recommendation::new(
            Severity::Blocking,
            "The engine disagrees with itself about this run.",
            "Do not read any number here. Two figures for the same fact came out differently, so at least one of them is wrong and nothing downstream can be trusted until it is known which.",
            format!(
                "{}: expected {:.2}, got {:.2} — {}",
                discrepancy.invariant,
                discrepancy.expected,
                discrepancy.actual,
                discrepancy.detail,
            ),
        ));
    }

    if let Some(item) = refusals(evaluation.refused_orders) {
        out.push(item);
    }

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

    // Before any warning about the return, because for an option the return is
    // the part least worth reading first.
    out.extend(option_tail(
        &found.out_of_sample_evidence.experiment,
        &evaluation.strategy_curve,
        &evaluation.strategy_ledger,
    ));

    // The benchmark starves the same way the strategy does, out of the same
    // account. When it holds fewer members than the book names, the comparison
    // stays fair — both sides are short of capital identically — but it stops
    // being a comparison about the instruments in the title.
    let benchmark_short = benchmark_held_fewer(found);
    if let Some((held, asked)) = benchmark_short {
        out.push(Recommendation::new(
            Severity::Warning,
            "The benchmark could not hold the whole book either.",
            "Read the excess return as being about the instruments both sides \
             actually held, not the ones named above. Buy-and-hold of this book \
             pays for the same account, so it is starved by the same shortage — \
             which keeps the comparison fair and makes it narrower than its \
             title.",
            format!("buy-and-hold held {held} of {asked} instruments"),
        ));
    }

    let silent = never_traded(found);
    if !silent.is_empty() {
        // Found by running two identical instruments against an account with
        // room for one: the second was denied every fill, and nothing in the
        // result said so. The return looked like a single-instrument run, the
        // trade count looked like a single-instrument run, and the finding
        // claimed to be about two instruments.
        out.push(Recommendation::new(
            Severity::Blocking,
            "Some instruments in this run never traded.",
            "Read this as a result about the instruments that did. The usual \
             cause is an account too small to fund every member at once: the \
             first to signal takes the capital and the rest are denied, so the \
             ones missing here are not instruments the rule declined — they \
             are instruments it could not afford.",
            format!("{} never opened a position", silent.join(", ")),
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
            "Read every number here as covering a window that was not finished. The return is \
             what it was at the halt, not what the rule would have made — and the rule was \
             stopped precisely where it was going worst, so what came after is unmeasured.",
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

    if let Some(psr) = evaluation.strategy.psr {
        // Only where there is a positive Sharpe to be sceptical about. A
        // negative one has already lost on the return, and saying its
        // confidence is low would be piling a weak objection on a decided
        // question.
        let positive = evaluation.strategy.sharpe.is_some_and(|sharpe| sharpe > 0.0);
        if positive && psr < PSR_WORTH_BELIEVING {
            out.push(Recommendation::new(
                Severity::Warning,
                "The Sharpe ratio is not distinguishable from no edge at all.",
                "Lengthen the window or loosen the entry so more returns are observed, \
                 and do not compare this Sharpe against another until it is. A point \
                 estimate says nothing about its own error, and this one is small enough, \
                 short enough or skewed enough that zero is still plausible.",
                format!(
                    "{:.0}% confidence the true Sharpe is above zero, against a {:.0}% bar",
                    psr * 100.0,
                    PSR_WORTH_BELIEVING * 100.0,
                ),
            ));
        }
    }

    // The bias nothing else here can see, and it always points the same way:
    // in the strategy's favour. Reported as a measurement when a distribution
    // series is on disk, and as the old estimate when there is none — the bias
    // is real either way, and the difference is whether its size is known.
    match evaluation.dividend_gap {
        Some(gap) if gap.worth_saying() => {
            let coverage = if gap.complete() {
                String::new()
            } else {
                format!(
                    "; a floor, not the answer — {} of {} instruments hold no distribution \
                     series, so their bias is unknown rather than zero",
                    gap.instruments - gap.covered,
                    gap.instruments,
                )
            };
            let evidence = format!(
                "{} distributions in the window: buy-and-hold would have collected {:.2} \
                 and this rule {:.2}, a gap of {:.2}% of starting capital{coverage}",
                gap.events,
                gap.benchmark_income,
                gap.strategy_income,
                gap.overstatement * 100.0,
            );

            match gap.corrected_excess(evaluation.excess_return) {
                // Split-adjusted: the distribution is missing from both sides,
                // so the gap is money neither received and the margin has to be
                // read smaller than it prints.
                Some(corrected) => {
                    // Whether the correction takes the result back across the
                    // line it was judged against. That is the version of this
                    // finding that changes a decision, so it changes severity.
                    let survives = corrected > 0.0;
                    out.push(Recommendation::new(
                        if survives {
                            Severity::Warning
                        } else {
                            Severity::Blocking
                        },
                        if survives {
                            "The excess return is smaller than it reads, by a measured amount."
                        } else {
                            "The excess return is entirely dividends the benchmark forgoes \
                             and this rule does not."
                        },
                        &format!(
                            "Read the margin over buy-and-hold as {:.2}%, not {:.2}%. Prices \
                             here are split-adjusted but not total-return adjusted, so no \
                             dividend is paid to anything — and the benchmark held through \
                             every ex-date while this rule held through only some.{}",
                            corrected * 100.0,
                            evaluation.excess_return * 100.0,
                            if survives {
                                ""
                            } else {
                                " Corrected, this rule did not beat holding, and the verdict \
                                 above was reached on the uncorrected figure."
                            },
                        ),
                        evidence,
                    ));
                }
                // Total-return adjusted: the distribution is already in the
                // returns, so the margin above is right and subtracting the gap
                // again would double-count it. What is left to say is what the
                // margin is *made of*, which is a different and quieter claim.
                None => out.push(Recommendation::new(
                    Severity::Note,
                    "Much of the margin over buy-and-hold is dividends rather than timing.",
                    "Do not subtract this from the excess return — on a total-return series \
                     the distributions are already in it, and taking them out again would \
                     report a margin smaller than the account earned. Read it as \
                     composition: this much of the edge came from holding through ex-dates \
                     rather than from choosing when to be in.",
                    evidence,
                )),
            }
        }
        // Measured, and too small to change how anything is read. Deliberately
        // silent: a line saying "this bias is negligible" on every result is a
        // line that teaches a reader to skip the section the day it is not.
        Some(_) => {}
        // No series for any instrument the run held. The bias is still real and
        // still points the same way; only its size is unknown, which is what
        // this said in words before it could be measured at all.
        None => {
            if let Some(exposure) = time_in_market(found) {
                if exposure < EXPOSURE_WORTH_SAYING {
                    out.push(Recommendation::new(
                        Severity::Warning,
                        "The excess return is flattered by dividends neither side received.",
                        "Re-fetch these instruments from a source that serves distributions \
                         and run this again: the size of this is then measured rather than \
                         guessed at. Until then, treat the margin over buy-and-hold as \
                         smaller than it reads, by roughly the instrument's dividend yield \
                         times the share of the window this rule sat out. Prices here are \
                         split-adjusted but not total-return adjusted, so no dividend is \
                         paid to anything — and the benchmark holds through every ex-date \
                         while this rule holds through only some.",
                        format!(
                            "in the market {:.0}% of the window against the benchmark's \
                             100%, so about {:.0}% of the period's dividends are missing \
                             from the benchmark and not from the strategy — and no \
                             distribution series is held here, so the size is unknown",
                            exposure * 100.0,
                            (1.0 - exposure) * 100.0,
                        ),
                    ));
                }
            }
        }
    }

    // What kind of market produced this, when the answer is not one kind.
    // A rule judged across a trend and a range gets one number describing
    // neither, and the verdict above is that number.
    if let Some(breakdown) =
        crate::regime::attribute(&evaluation.strategy_curve, &evaluation.benchmark_curve)
    {
        if let Some((best, worst)) = breakdown.split() {
            let action = format!(
                "Read the verdict as an average of two different answers, not as one. This rule \
                 beat its benchmark by {:+.1}% while the market was {} and lost {:.1}% to it \
                 while {} — so whichever verdict it received describes a blend the market never \
                 produced in one piece.\n\nThis is a caveat, not a filter. Trading only the \
                 regime it worked in means knowing the regime *before* the bar, and these labels \
                 were computed after the run; selecting on them would be look-ahead of the most \
                 flattering kind.",
                best.excess_return * 100.0,
                best.regime.label(),
                worst.excess_return.abs() * 100.0,
                worst.regime.label(),
            );
            out.push(Recommendation::new(
                Severity::Warning,
                "The result is two different results averaged together.",
                &action,
                format!(
                    "{} of the window was {} ({:+.1}% excess) and {} was {} ({:+.1}% excess)",
                    share(best.share),
                    best.regime.label(),
                    best.excess_return * 100.0,
                    share(worst.share),
                    worst.regime.label(),
                    worst.excess_return * 100.0,
                ),
            ));
        }
    }

    // An intraday rule that made its money in the gaps between sessions. The
    // verdict reads as a statement about intraday timing and is not one.
    let intraday = found.out_of_sample_evidence.experiment.interval.is_intraday();
    if let Some(split) = crate::overnight::split(&evaluation.strategy_curve)
        .filter(|split| intraday && split.earned_only_overnight())
    {
        out.push(Recommendation::new(
            Severity::Warning,
            "The profit was made while the market was closed.",
            "Do not read this as evidence for the intraday signal: within sessions it did not \
             make money, and everything it earned came from positions it happened to carry \
             through the close. That is exposure to overnight news and the overnight drift, \
             which no rule here chose. Flatten at the close and run it again to see what the \
             signal itself is worth.",
            format!(
                "held through {} nights: {:+.2}% between sessions, {:+.2}% within them",
                split.nights_held,
                split.overnight * 100.0,
                split.session * 100.0,
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

/// Everything worth saying about a panel, most stopping first.
///
/// A panel's findings are not a study's. Whether its members were independent,
/// whether a positive average is carried by a minority of them, whether
/// holding all of them would have been holdable — none of these arise for a
/// single instrument, and none have an equivalent in [`recommend`].
///
/// `criteria` is passed rather than read off the evidence because a panel does
/// not store the bar it was judged against.
#[must_use]
pub fn recommend_panel(
    found: &PanelEvidence,
    criteria: &EvaluationCriteria,
) -> Vec<Recommendation> {
    let pooled = &found.pooled;
    let mut out = Vec::new();

    // ---- blocking: the result cannot be read -----------------------------

    if pooled.total_trades < criteria.min_trades {
        out.push(Recommendation::new(
            Severity::Blocking,
            "Too few round trips across the whole panel to tell skill from luck.",
            "Add instruments or lengthen the window. Pooling is what a panel is for, and this \
             one has not pooled enough to read.",
            format!(
                "{} trades across {} instruments, against a {} minimum",
                pooled.total_trades, pooled.instruments, criteria.min_trades
            ),
        ));
    }

    if !found.selection.survived_deflation {
        out.push(Recommendation::new(
            Severity::Blocking,
            "The search explains the winning configuration.",
            "Shrink the grid or lengthen the in-sample window, then re-run. One configuration \
             chosen across every instrument is still one configuration chosen out of many.",
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
        // Worse here than in a study: an instrument that failed is one the
        // conclusion quietly excludes, and the ones that fail are rarely a
        // random sample of the panel.
        out.push(Recommendation::new(
            Severity::Blocking,
            "Part of the panel never ran.",
            "Fix the failing members before reading the pooled numbers. A panel that silently \
             dropped instruments is a panel of the instruments that happened to work.",
            format!(
                "{} instrument/configuration runs failed",
                found.failures.len()
            ),
        ));
    }

    // ---- warning: readable, but resting on something fragile -------------

    if pooled.distinct > 0 && pooled.distinct < pooled.instruments {
        out.push(Recommendation::new(
            Severity::Warning,
            "This panel holds the same security more than once.",
            "Read every pooled figure as covering the distinct securities rather \
             than the rows. Two copies of one stock are not two pieces of \
             evidence about anything, however they were filed, and the easiest \
             way to have one is to fetch the same ticker from two sources.",
            format!(
                "{} rows covering {} securities",
                pooled.instruments, pooled.distinct
            ),
        ));
    }

    if let Some(breadth) = &found.breadth {
        if let (Some(effective), Some(overstatement)) =
            (breadth.effective, breadth.overstatement())
        {
            if overstatement > crate::panel::OVERSTATEMENT_WORTH_SAYING {
                out.push(Recommendation::new(
                    Severity::Warning,
                    "These instruments are not as independent as their count suggests.",
                    "Read the pooled average as resting on fewer observations than it appears \
                     to. Adding more instruments that move like these will not fix it; adding \
                     ones that do not move like them will.",
                    format!(
                        "{} instruments behaving like {effective:.1} independent ones, average \
                         correlation {:.2}",
                        breadth.instruments.len(),
                        breadth.mean_correlation.unwrap_or_default(),
                    ),
                ));
            }
        }
    }

    #[expect(clippy::cast_precision_loss, reason = "panel sizes are small")]
    let beat_share = if pooled.instruments == 0 {
        0.0
    } else {
        pooled.beat_benchmark as f64 / pooled.instruments as f64
    };
    if pooled.mean_excess_return > 0.0 && beat_share < PANEL_MAJORITY {
        out.push(Recommendation::new(
            Severity::Warning,
            "The positive average is carried by a minority of the instruments.",
            "Look at which members produced it before calling this something that works across \
             instruments. A rule that wins on one and loses on the rest is a finding about \
             that one.",
            format!(
                "{} of {} instruments beat their own benchmark, mean excess {:.1}%",
                pooled.beat_benchmark,
                pooled.instruments,
                pooled.mean_excess_return * 100.0,
            ),
        ));
    }

    if let Some(book) = &found.book {
        let diversification = pooled.mean_max_drawdown - book.max_drawdown;
        if diversification <= 0.0 && pooled.instruments > 1 {
            out.push(Recommendation::new(
                Severity::Warning,
                "Holding all of them would have fallen as hard as holding the average one.",
                "Treat this panel as one bet rather than several. The instruments went down \
                 together, so spreading capital across them bought no protection.",
                format!(
                    "book drawdown {:.1}% against a mean member drawdown of {:.1}%",
                    book.max_drawdown * 100.0,
                    pooled.mean_max_drawdown * 100.0,
                ),
            ));
        }
    }

    if pooled.worst_max_drawdown > criteria.max_drawdown
        && pooled.mean_max_drawdown <= criteria.max_drawdown
    {
        // The mean passed and a member did not. Averaging is what hid it, so
        // the average is the wrong place to go looking.
        out.push(Recommendation::new(
            Severity::Warning,
            "One instrument breached the drawdown ceiling even though the average did not.",
            "Decide whether the panel is judged on its average member or its worst one. \
             Capital is committed per instrument, and nobody holds the average.",
            format!(
                "worst member {:.1}% against a {:.1}% ceiling, mean {:.1}%",
                pooled.worst_max_drawdown * 100.0,
                criteria.max_drawdown * 100.0,
                pooled.mean_max_drawdown * 100.0,
            ),
        ));
    }

    // ---- notes -----------------------------------------------------------

    if out.is_empty() && found.verdict == Verdict::Supported {
        out.push(Recommendation::new(
            Severity::Note,
            "Nothing in the evidence undercuts this panel.",
            "Re-run it on a later window, or on instruments that move differently from these. \
             A panel that survives is where the work starts, not where it ends.",
            format!(
                "{} trades across {} instruments, mean excess {:.1}%",
                pooled.total_trades,
                pooled.instruments,
                pooled.mean_excess_return * 100.0,
            ),
        ));
    }

    out.sort_by_key(|item| item.severity);
    out
}

/// Everything worth saying about a walk-forward run, most stopping first.
///
/// A walk-forward tests a *procedure* — re-select on recent data, trade the
/// next stretch, repeat — so its findings are about the procedure. Whether the
/// search selected anything real, whether it kept selecting the same thing,
/// and whether the folds were long enough for the rule to start are questions
/// a single study cannot ask, and they decide more than the combined return
/// does.
#[must_use]
pub fn recommend_walk_forward(found: &WalkForwardEvidence) -> Vec<Recommendation> {
    let folds = found.folds.len();
    let mut out = Vec::new();

    // ---- blocking: the result cannot be read -----------------------------

    let refused = found.folds.iter().fold(crate::Refused::default(), |sum, fold| {
        let each = fold.out_of_sample_evidence.evaluation.refused_orders;
        crate::Refused {
            entries: sum.entries + each.entries,
            exits: sum.exits + each.exits,
        }
    });
    if let Some(item) = refusals(refused) {
        out.push(item);
    }

    if folds == 0 {
        out.push(Recommendation::new(
            Severity::Blocking,
            "The window produced no folds.",
            "Shorten the in-sample length or the step, or fetch more history. There is nothing \
             here to read.",
            "0 folds".to_owned(),
        ));
        return out;
    }

    // The same test the verdict applies, called rather than restated. Two
    // thresholds for one question is how a report ends up recommending against
    // a result it also calls supported, which is exactly what happened here.
    if !crate::walk_forward::selection_beat_chance(&found.folds) {
        // The finding the combined curve cannot show: a procedure whose
        // selections are noise still produces a curve, and the curve looks
        // exactly the same either way.
        out.push(Recommendation::new(
            Severity::Blocking,
            "The re-selection is picking noise in most folds.",
            "Shrink the grid or lengthen the in-sample window before reading the combined \
             return. What is under test here is the procedure, and a procedure that selects \
             noise most of the time has not been shown to select.",
            format!(
                "{} of {folds} folds chose a configuration beating what a no-skill search of \
                 that size would produce",
                found.folds_surviving_deflation,
            ),
        ));
    }

    if found.folds_without_trades > 0 {
        out.push(Recommendation::new(
            Severity::Blocking,
            "Some folds never opened a position.",
            "Lengthen the step so each fold outlasts the rule's warm-up. An empty fold is not \
             evidence the rule does nothing — it is evidence the fold was too short to let it \
             start — and it enters the combined curve as a flat stretch either way.",
            format!(
                "{} of {folds} folds traded not at all",
                found.folds_without_trades
            ),
        ));
    }

    // ---- warning: readable, but resting on something fragile -------------

    let fold_ledgers: Vec<crate::Trade> = found
        .folds
        .iter()
        .flat_map(|fold| fold.out_of_sample_evidence.evaluation.strategy_ledger.iter().cloned())
        .collect();
    out.extend(option_tail(&found.template, &found.combined_curve, &fold_ledgers));

    for axis in &found.stability {
        if axis.distinct > 1 && axis.modal_share < MODAL_SHARE_FLOOR {
            out.push(Recommendation::new(
                Severity::Warning,
                &format!("The search never settled on a value for `{}`.", axis.axis),
                "Consider fixing this axis or dropping it. One re-chosen differently every fold \
                 is widening the search — and so raising the bar the result has to clear — \
                 without converging on an answer.",
                format!(
                    "{} distinct values across {folds} folds; the most common won {:.0}% of them",
                    axis.distinct,
                    axis.modal_share * 100.0,
                ),
            ));
        }
    }

    out.extend(shape_warnings(&found.combined_trades));

    if found.excess_return <= 0.0 && found.verdict != Verdict::Inconclusive {
        out.push(Recommendation::new(
            Severity::Warning,
            "Re-selecting did not beat holding the instrument.",
            "Compare against the single-window study before concluding the procedure adds \
             anything. Re-selection pays a warm-up at every fold boundary, and that cost is \
             real whether or not it buys something.",
            format!(
                "{:.1}% combined against {:.1}% buy-and-hold over the same stitched period",
                found.combined.total_return * 100.0,
                found.benchmark.total_return * 100.0,
            ),
        ));
    }

    // ---- notes -----------------------------------------------------------

    if out.is_empty() && found.verdict == Verdict::Supported {
        out.push(Recommendation::new(
            Severity::Note,
            "Nothing in the evidence undercuts this procedure.",
            "Re-run it on another instrument before believing it. Surviving a walk-forward is a \
             stronger claim than surviving one study, and it is still one claim.",
            format!(
                "{folds} folds, {} of them selecting above the no-skill bar, {:.1}% excess",
                found.folds_surviving_deflation,
                found.excess_return * 100.0,
            ),
        ));
    }

    out.sort_by_key(|item| item.severity);
    out
}

/// What share of the window the rule actually held something.
///
/// The denominator is the window times the number of instruments, so a book
/// whose members can hold at once is measured as average exposure per member
/// rather than being allowed to exceed one.
///
/// `None` when nothing closed, or the window has no length — there is no
/// exposure to speak of, and inventing a zero would fire this on every run
/// that did not trade.
fn time_in_market(found: &FamilyEvidence) -> Option<f64> {
    let experiment = &found.out_of_sample_evidence.experiment;
    let trades = &found.out_of_sample_evidence.evaluation.strategy_trades;

    let held = trades.average_holding_secs? * f64::from(trades.closed);
    if trades.closed == 0 {
        return None;
    }

    let days = (experiment.window.to - experiment.window.from).num_days();
    if days <= 0 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "windows are years, not eons")]
    let window = days as f64 * 86_400.0 * experiment.instruments().len() as f64;

    // Clamped rather than trusted. Holding periods are measured to the bar and
    // the window to the day, so a rule that is in the market continuously can
    // round to just over one.
    Some((held / window).clamp(0.0, 1.0))
}

/// How many instruments the benchmark held, when that is fewer than the book
/// asked for.
///
/// `None` for a single-instrument study, for a benchmark that held everything,
/// and for a finding recorded before the benchmark's instruments were kept —
/// an empty list there means "not recorded", not "held nothing", and reading
/// it the other way would accuse every stored book of a shortage it may not
/// have had.
fn benchmark_held_fewer(found: &FamilyEvidence) -> Option<(usize, usize)> {
    let asked = found.out_of_sample_evidence.experiment.instruments().len();
    if asked < 2 {
        return None;
    }
    let held = found
        .out_of_sample_evidence
        .evaluation
        .benchmark_instruments
        .len();
    (held > 0 && held < asked).then_some((held, asked))
}

/// Instruments the run held but never opened a position in.
///
/// Only meaningful for a book: a single-instrument run that never traded is
/// already caught by the trade-count bar, and a ledger recorded before trades
/// named their instrument cannot answer the question at all — so an empty name
/// in the ledger means "cannot tell", and this says nothing rather than
/// accusing every stored finding of crowding out.
fn never_traded(found: &FamilyEvidence) -> Vec<String> {
    let experiment = &found.out_of_sample_evidence.experiment;
    if experiment.alongside.is_empty() {
        return Vec::new();
    }
    let ledger = &found.out_of_sample_evidence.evaluation.strategy_ledger;
    if ledger.iter().any(|trade| trade.instrument.is_empty()) {
        return Vec::new();
    }
    experiment
        .instruments()
        .into_iter()
        .filter(|name| !ledger.iter().any(|trade| trade.instrument == *name))
        .collect()
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
            instrument: String::new(),
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
    fn an_option_run_leads_with_its_losses_and_what_its_history_lacks() {
        // Collects 0.2% on 59 days and loses 15% on one: a Sharpe-friendly
        // shape until that one day.
        let start = chrono::NaiveDate::from_ymd_opt(2024, 3, 1).expect("valid");
        let mut equity = 100_000.0;
        let curve: Vec<crate::EquityPoint> = (0..61)
            .map(|day| {
                if day > 0 {
                    equity *= if day == 45 { 0.85 } else { 1.002 };
                }
                crate::EquityPoint {
                    at: (start + chrono::Duration::days(day)).and_time(chrono::NaiveTime::MIN),
                    equity,
                }
            })
            .collect();

        let mut option = experiment();
        option.instrument = "SPY240621P00500000.AOPT".to_owned();
        let advice = option_tail(&option, &curve, &[]);
        assert_eq!(advice.len(), 2);
        assert!(advice[0].evidence.contains("worst 1day -15.0%"), "{}", advice[0].evidence);
        assert!(advice[0].evidence.contains("worst month"), "{}", advice[0].evidence);
        assert!(advice[1].finding.contains("no crash"));

        assert!(option_tail(&experiment(), &curve, &[]).is_empty(), "a stock run is unaffected");
        // A put spread reads SPY and holds puts: its ledger makes it an option run.
        let mut put = trade(0, 3, -50.0, ExitReason::Signal);
        put.instrument = "SPY240621P00500000.AOPT".to_owned();
        assert_eq!(option_tail(&experiment(), &curve, &[put]).len(), 2);
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

    // ---- panels ----------------------------------------------------------

    /// A panel that clears every bar, so each test can break exactly one thing.
    fn clean_panel() -> PanelEvidence {
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
        PanelEvidence {
            hypothesis: crate::HypothesisId("h".to_owned()),
            dataset: crate::DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
            in_sample: crate::DateRange {
                from: day(1),
                to: day(4),
            },
            out_of_sample: crate::DateRange {
                from: day(5),
                to: day(9),
            },
            selected_params: std::collections::BTreeMap::new(),
            selection: crate::Selection {
                trials: 9,
                best_sharpe: 1.5,
                expected_best_under_null: Some(0.5),
                survived_deflation: true,
                prior_trials: 0,
                scored: Vec::new(),
            },
            per_instrument: Vec::new(),
            pooled: crate::PooledOutcome {
                instruments: 4,
                total_trades: 60,
                mean_excess_return: 0.1,
                beat_benchmark: 4,
            distinct: 4,
            distinct_beat: 4,
                mean_max_drawdown: 0.1,
                worst_max_drawdown: 0.12,
            },
            breadth: None,
            book: None,
            study: None,
            criteria: None,
            failures: Vec::new(),
            verdict: Verdict::Supported,
            reasons: Vec::new(),
        }
    }

    fn findings(items: &[Recommendation]) -> String {
        items
            .iter()
            .map(|item| format!("{}: {}", item.severity.label(), item.finding))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_panel_holding_one_security_twice_is_told_so() {
        // The easiest way to have one, and the way this was found: fetch the
        // same ticker from two sources and run a panel over the pair. It read
        // "beat benchmark on 4 of 6" for what was two of three.
        let mut panel = clean_panel();
        panel.pooled.instruments = 6;
        panel.pooled.distinct = 3;

        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        let item = out
            .iter()
            .find(|item| item.finding.contains("same security more than once"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Warning);
        assert!(item.evidence.contains("6 rows covering 3"), "{}", item.evidence);
    }

    #[test]
    fn a_panel_of_distinct_securities_is_not_warned() {
        let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
        assert!(
            !out.iter().any(|item| item.finding.contains("same security")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_panel_recorded_before_distinct_was_measured_makes_no_accusation() {
        // Zero means not measured, not "no distinct securities".
        let mut panel = clean_panel();
        panel.pooled.distinct = 0;
        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        assert!(
            !out.iter().any(|item| item.finding.contains("same security")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_panel_average_carried_by_a_minority_is_named_as_such() {
        // The finding a mean cannot express. One member carrying five and
        // every member edging ahead produce the same positive average, and
        // only one of them is a result about instruments in general.
        let mut panel = clean_panel();
        panel.pooled.beat_benchmark = 1;

        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        assert!(
            out.iter().any(|item| item.finding.contains("minority")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_panel_where_most_members_won_says_nothing_about_minorities() {
        let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
        assert!(
            !out.iter().any(|item| item.finding.contains("minority")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_worst_member_through_the_ceiling_is_reported_even_when_the_mean_passed() {
        // Averaging is exactly what hides this, so the average is the wrong
        // place to notice it.
        let mut panel = clean_panel();
        panel.pooled.worst_max_drawdown = 0.55;

        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        let item = out
            .iter()
            .find(|item| item.finding.contains("breached the drawdown ceiling"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Warning);
    }

    #[test]
    fn a_panel_whose_members_fell_together_is_called_one_bet() {
        let mut panel = clean_panel();
        // A book that fell as hard as the average member: no diversification.
        panel.book = Some(crate::Metrics {
            total_return: 0.1,
            cagr: 0.1,
            max_drawdown: 0.1,
            volatility: 0.2,
            sharpe: Some(1.0),
            sortino: Some(1.2),
            calmar: Some(0.9),
            psr: None,
            trades: 60,
        });

        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        assert!(
            out.iter().any(|item| item.action.contains("one bet")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_panel_that_diversified_is_not_warned_about_it() {
        let mut panel = clean_panel();
        // Fell less than the average member did: the point of the panel.
        panel.book = Some(crate::Metrics {
            total_return: 0.1,
            cagr: 0.1,
            max_drawdown: 0.04,
            volatility: 0.2,
            sharpe: Some(1.0),
            sortino: Some(1.2),
            calmar: Some(0.9),
            psr: None,
            trades: 60,
        });

        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        assert!(
            !out.iter().any(|item| item.action.contains("one bet")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_panel_that_dropped_instruments_blocks_rather_than_warns() {
        // The ones that fail are rarely a random sample of the panel.
        let mut panel = clean_panel();
        panel.failures = vec!["MSFT.NASDAQ out-of-sample: no bars".to_owned()];

        let out = recommend_panel(&panel, &EvaluationCriteria::default());
        let item = out
            .iter()
            .find(|item| item.finding.contains("never ran"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Blocking);
    }

    #[test]
    fn a_clean_panel_still_says_one_finding_is_where_the_work_starts() {
        let out = recommend_panel(&clean_panel(), &EvaluationCriteria::default());
        assert_eq!(out.len(), 1, "{}", findings(&out));
        assert_eq!(out[0].severity, Severity::Note);
    }

    // ---- walk-forward ----------------------------------------------------

    fn metrics(total_return: f64) -> crate::Metrics {
        crate::Metrics {
            total_return,
            cagr: total_return,
            max_drawdown: 0.1,
            volatility: 0.2,
            sharpe: Some(1.0),
            sortino: Some(1.2),
            calmar: Some(0.9),
            psr: None,
            trades: 40,
        }
    }

    /// A walk-forward that clears every bar.
    fn clean_walk() -> WalkForwardEvidence {
        WalkForwardEvidence {
            hypothesis: crate::HypothesisId("h".to_owned()),
            template: experiment(),
            in_sample_days: 365,
            step_days: 90,
            anchored: false,
            folds: vec![],
            combined: metrics(0.3),
            benchmark: metrics(0.1),
            excess_return: 0.2,
            combined_curve: Vec::new(),
            benchmark_curve: Vec::new(),
            combined_trades: TradeStats::default(),
            stability: Vec::new(),
            folds_surviving_deflation: 5,
            grid: None,
            criteria: None,
            folds_without_trades: 0,
            verdict: Verdict::Supported,
            reasons: Vec::new(),
        }
    }

    fn experiment() -> crate::Experiment {
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
        crate::Experiment {
            id: crate::ExperimentId("x".to_owned()),
            hypothesis: crate::HypothesisId("h".to_owned()),
            instrument: "AAPL.NASDAQ".to_owned(),
            alongside: Vec::new(),
            underlying: None,
            window: crate::DateRange {
                from: day(1),
                to: day(9),
            },
            interval: arvo_data::BarInterval::DAILY,
            dataset: crate::DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
            strategy: crate::StrategySpec {
                name: "sma_cross".to_owned(),
                params: std::collections::BTreeMap::new(),
            },
            costs: crate::CostModel::proportional(0.0, 0.0),
            risk: crate::RiskModel::default(),
            starting_cash: 100_000.0,
            seed: 7,
        }
    }

    /// Five folds, because the gate is a binomial tail: all of four is one in
    /// sixteen under the null and does not clear 5%, all of five is one in
    /// thirty-two and does.
    fn with_folds(mut walk: WalkForwardEvidence) -> WalkForwardEvidence {
        // Winners well clear of their own no-skill bars, which is what the
        // selection test reads. `folds_surviving_deflation` is carried beside
        // it for the reasons that quote it, and the two are set to agree.
        walk.folds = (0..5)
            .map(|_| crate::FamilyEvidence {
                hypothesis: crate::HypothesisId("h".to_owned()),
                in_sample: experiment().window,
                out_of_sample: experiment().window,
                selection: crate::Selection {
                    trials: 9,
                    best_sharpe: 1.5,
                    expected_best_under_null: Some(1.0),
                    survived_deflation: true,
                    prior_trials: 0,
                    scored: Vec::new(),
                },
                selected: experiment(),
                out_of_sample_evidence: crate::Evidence {
                    hypothesis: crate::HypothesisId("h".to_owned()),
                    experiment: experiment(),
                    benchmark: crate::ExperimentId("b".to_owned()),
                    engine: "test 1".to_owned(),
                    criteria: EvaluationCriteria::default(),
                    evaluation: crate::Evaluation {
                        strategy: metrics(0.3),
                        benchmark: metrics(0.1),
                        strategy_curve: Vec::new(),
                        benchmark_curve: Vec::new(),
                        strategy_trades: TradeStats::default(),
                        strategy_ledger: Vec::new(),
                dividend_gap: None,
                refused_orders: crate::Refused::default(),
                        benchmark_instruments: Vec::new(),
                        excess_return: 0.2,
                        verdict: Verdict::Supported,
                        reasons: Vec::new(),
                    },
                },
                failures: Vec::new(),
                verdict: Verdict::Supported,
                reasons: Vec::new(),
            })
            .collect();
        walk
    }

    #[test]
    fn a_procedure_that_selects_noise_in_most_folds_blocks_the_combined_return() {
        // The finding the combined curve cannot show: a procedure whose
        // selections are noise still draws a curve, and it looks the same.
        //
        // Expressed as margins rather than a count, because that is what the
        // test reads now. Each fold's winner landed near its own no-skill bar,
        // some a little above and some a little below, which is what a search
        // with nothing to find produces.
        let mut walk = with_folds(clean_walk());
        walk.folds_surviving_deflation = 1;
        for (index, fold) in walk.folds.iter_mut().enumerate() {
            fold.selection.expected_best_under_null = Some(1.0);
            fold.selection.best_sharpe = if index % 2 == 0 { 1.02 } else { 0.97 };
        }

        let out = recommend_walk_forward(&walk);
        let item = out
            .iter()
            .find(|item| item.finding.contains("picking noise"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Blocking);
    }

    #[test]
    fn an_empty_fold_is_reported_as_too_short_rather_than_as_a_rule_doing_nothing() {
        let mut walk = with_folds(clean_walk());
        walk.folds_without_trades = 2;

        let out = recommend_walk_forward(&walk);
        let item = out
            .iter()
            .find(|item| item.finding.contains("never opened a position"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert!(item.action.contains("warm-up"), "{}", item.action);
    }

    #[test]
    fn an_axis_the_search_never_settled_on_is_named_by_name() {
        let mut walk = with_folds(clean_walk());
        walk.stability = vec![crate::AxisStability {
            axis: "fast".to_owned(),
            distinct: 4,
            modal: 10.0,
            modal_share: 0.25,
        }];

        let out = recommend_walk_forward(&walk);
        assert!(
            out.iter().any(|item| item.finding.contains("`fast`")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn an_axis_the_search_agreed_on_is_left_alone() {
        let mut walk = with_folds(clean_walk());
        walk.stability = vec![crate::AxisStability {
            axis: "fast".to_owned(),
            distinct: 2,
            modal: 10.0,
            modal_share: 0.75,
        }];

        let out = recommend_walk_forward(&walk);
        assert!(
            !out.iter().any(|item| item.finding.contains("never settled")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_procedure_that_did_not_beat_holding_is_told_to_compare_against_one_window() {
        let mut walk = with_folds(clean_walk());
        walk.excess_return = -0.05;
        walk.verdict = Verdict::NotSupported;

        let out = recommend_walk_forward(&walk);
        assert!(
            out.iter()
                .any(|item| item.finding.contains("did not beat holding")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_walk_forward_with_no_folds_says_so_and_stops() {
        let out = recommend_walk_forward(&clean_walk());
        assert_eq!(out.len(), 1, "{}", findings(&out));
        assert_eq!(out[0].severity, Severity::Blocking);
        assert!(out[0].finding.contains("no folds"), "{}", out[0].finding);
    }

    #[test]
    fn a_clean_walk_forward_is_still_told_it_is_one_claim() {
        let out = recommend_walk_forward(&with_folds(clean_walk()));
        assert_eq!(out.len(), 1, "{}", findings(&out));
        assert_eq!(out[0].severity, Severity::Note);
        assert!(out[0].action.contains("one claim"), "{}", out[0].action);
    }

    // ---- books -----------------------------------------------------------

    /// A study of a two-instrument book whose ledger holds `traded`.
    fn book_study(traded: &[&str]) -> FamilyEvidence {
        let mut experiment = experiment();
        experiment.alongside = vec!["MSFT.NASDAQ".to_owned()];
        let ledger: Vec<crate::Trade> = traded
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let mut round_trip = trade(
                    u32::try_from(index).expect("small"),
                    3,
                    10.0,
                    crate::ExitReason::Signal,
                );
                round_trip.instrument = (*name).to_owned();
                round_trip
            })
            .collect();

        let mut study = with_folds(clean_walk()).folds.remove(0);
        study.out_of_sample_evidence.experiment = experiment.clone();
        study.selected = experiment;
        study.out_of_sample_evidence.evaluation.strategy_ledger = ledger;
        study
    }

    #[test]
    fn a_run_the_venue_refused_orders_in_blocks() {
        // A refused entry is a signal never acted on: the run measured a
        // starved strategy, not the rule.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.evaluation.refused_orders = crate::Refused {
            entries: 69,
            exits: 0,
        };
        let out = recommend(&study);
        let item = out
            .iter()
            .find(|item| item.finding.contains("refused orders"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Blocking);
        assert!(item.evidence.contains("69 entries"), "{}", item.evidence);

        study.out_of_sample_evidence.evaluation.refused_orders = crate::Refused::default();
        assert!(!recommend(&study).iter().any(|item| item.finding.contains("refused orders")));
    }

    #[test]
    fn a_run_whose_curve_contradicts_its_ledger_blocks_before_anything_else() {
        let day = |d: u32| {
            chrono::NaiveDate::from_ymd_opt(2024, 1, d)
                .expect("valid")
                .and_time(chrono::NaiveTime::MIN)
        };
        // The category the gap analysis had no room for: not "too little
        // evidence" but "the evidence contradicts itself". More data does not
        // fix it, so it outranks every other objection.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        let ledger = &mut study.out_of_sample_evidence.evaluation.strategy_ledger;
        ledger[0].pnl = 10.0;
        ledger[0].commission = 0.0;
        study.out_of_sample_evidence.evaluation.strategy_curve = vec![
            crate::EquityPoint {
                at: day(1),
                equity: 100_000.0,
            },
            crate::EquityPoint {
                at: day(2),
                // Nowhere near the 10.0 the ledger realised.
                equity: 190_000.0,
            },
        ];

        let out = recommend(&study);
        assert_eq!(
            out.first().map(|item| item.severity),
            Some(Severity::Blocking),
            "{}",
            findings(&out)
        );
        assert!(
            out.iter().any(|item| item.finding.contains("disagrees with itself")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_benchmark_that_could_not_hold_the_whole_book_is_reported() {
        // Both sides starve out of the same account, which keeps the
        // comparison fair and makes it about fewer instruments than the title
        // claims. Measured against the engine: three instruments and room for
        // one left the strategy holding one and buy-and-hold holding one, with
        // an excess return that reads as a statement about three.
        let mut study = book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]);
        study.out_of_sample_evidence.evaluation.benchmark_instruments =
            vec!["AAPL.NASDAQ".to_owned()];

        let out = recommend(&study);
        let item = out
            .iter()
            .find(|item| item.finding.contains("benchmark could not hold"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Warning);
        assert!(item.evidence.contains("1 of 2"), "{}", item.evidence);
    }

    #[test]
    fn a_benchmark_that_held_everything_is_not_reported() {
        let mut study = book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]);
        study.out_of_sample_evidence.evaluation.benchmark_instruments =
            vec!["AAPL.NASDAQ".to_owned(), "MSFT.NASDAQ".to_owned()];

        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("benchmark could not hold")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_finding_recorded_before_benchmark_instruments_were_kept_makes_no_accusation() {
        // An empty list means "not recorded", not "held nothing". Reading it
        // the other way accuses every stored book of a shortage.
        let study = book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]);
        assert!(
            study
                .out_of_sample_evidence
                .evaluation
                .benchmark_instruments
                .is_empty(),
            "the fixture stands in for an older record"
        );
        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("benchmark could not hold")),
            "{}",
            findings(&out)
        );
    }



    /// A study whose window trends for its first half and ranges for its
    /// second, with a rule that beats the benchmark in one and loses in the
    /// other.
    fn two_regimes() -> FamilyEvidence {
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();

        let mut benchmark = Vec::new();
        let mut strategy = Vec::new();
        let (mut market, mut equity) = (100.0, 100.0);
        for index in 0..120 {
            if index < 60 {
                market += 1.0; // goes somewhere
                equity += 2.0; // and this outruns it
            } else {
                market += if index % 2 == 0 { 3.0 } else { -3.0 }; // arrives nowhere
                equity -= 0.4; // and this bleeds
            }
            let at = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
                .expect("valid")
                .and_time(chrono::NaiveTime::MIN)
                + chrono::Duration::days(index);
            benchmark.push(crate::EquityPoint { at, equity: market });
            strategy.push(crate::EquityPoint { at, equity });
        }

        let evaluation = &mut study.out_of_sample_evidence.evaluation;
        evaluation.strategy_curve = strategy;
        evaluation.benchmark_curve = benchmark;
        study
    }

    #[test]
    fn a_result_earned_in_one_regime_and_lost_in_another_says_so() {
        // The finding this exists for: a rule judged across a trend and a range
        // gets one number describing neither, and the verdict above it is that
        // number.
        let out = recommend(&two_regimes());
        let item = out
            .iter()
            .find(|item| item.finding.contains("two different results"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));

        assert_eq!(item.severity, Severity::Warning);
        assert!(
            item.evidence.contains("trending up") && item.evidence.contains("ranging"),
            "it should name both regimes: {}",
            item.evidence
        );
    }

    #[test]
    fn the_regime_split_is_a_caveat_and_never_a_filter() {
        // The property that keeps this from becoming a look-ahead machine.
        // These labels are computed after the run; selecting on them would be
        // the most flattering bias available, and the action text has to say so
        // rather than leaving a reader to infer it.
        let out = recommend(&two_regimes());
        let item = out
            .iter()
            .find(|item| item.finding.contains("two different results"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));

        assert!(
            item.action.contains("caveat, not a filter"),
            "{}",
            item.action
        );
        assert!(
            item.action.contains("look-ahead"),
            "it must name what selecting on these would be: {}",
            item.action
        );
    }

    #[test]
    fn a_result_that_behaved_the_same_way_throughout_is_not_split() {
        // Silence is the right answer. A line on every result saying "this
        // behaved consistently" is a line that teaches a reader to skip the
        // section the day it does not.
        let out = recommend(&book_study(&["AAPL.NASDAQ"]));
        assert!(
            !out.iter().any(|item| item.finding.contains("two different results")),
            "{}",
            findings(&out)
        );
    }

    /// A study at `interval` whose curve lost within both sessions and gained
    /// across the night between them.
    fn gapped(interval: arvo_data::BarInterval) -> FamilyEvidence {
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.interval = interval;
        let point = |day: u32, hour: u32, equity: f64| crate::EquityPoint {
            at: chrono::NaiveDate::from_ymd_opt(2024, 7, day)
                .expect("valid")
                .and_hms_opt(hour, 0, 0)
                .expect("valid"),
            equity,
        };
        study.out_of_sample_evidence.evaluation.strategy_curve = vec![
            point(1, 14, 100.0),
            point(1, 19, 98.0),
            point(2, 14, 106.0),
            point(2, 19, 104.0),
        ];
        study
    }

    #[test]
    fn an_intraday_rule_that_only_made_money_overnight_is_told_so() {
        let out = recommend(&gapped(arvo_data::BarInterval::new(
            5,
            arvo_data::IntervalUnit::Minute,
        )));
        let item = out
            .iter()
            .find(|item| item.finding.contains("while the market was closed"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Warning);
        assert!(item.evidence.contains("1 nights"), "{}", item.evidence);
    }

    #[test]
    fn a_daily_rule_is_not_accused_of_holding_overnight() {
        // Every daily bar is a night; holding through them is the rule.
        let out = recommend(&gapped(arvo_data::BarInterval::DAILY));
        assert!(
            !out.iter().any(|item| item.finding.contains("while the market was closed")),
            "{}",
            findings(&out)
        );
    }

    /// A study with a measured gap of `overstatement`, against `excess`.
    fn measured(excess: f64, gap: crate::DividendGap) -> FamilyEvidence {
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        let evaluation = &mut study.out_of_sample_evidence.evaluation;
        evaluation.excess_return = excess;
        evaluation.dividend_gap = Some(gap);
        study
    }

    fn gap(overstatement: f64, covered: usize, instruments: usize) -> crate::DividendGap {
        crate::DividendGap {
            events: 4,
            strategy_income: 120.0,
            benchmark_income: 120.0 + overstatement * 100_000.0,
            overstatement,
            adjustment: arvo_data::source::Adjustment::Split,
            covered,
            instruments,
        }
    }

    #[test]
    fn a_measured_gap_states_the_corrected_margin_instead_of_a_rule_of_thumb() {
        // The whole point of fetching distributions. It used to say "smaller by
        // roughly the yield times the time out of the market", which nobody can
        // act on without going and finding the dividend history themselves.
        let out = recommend(&measured(0.08, gap(0.02, 1, 1)));
        let item = out
            .iter()
            .find(|item| item.finding.contains("smaller than it reads"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));

        assert_eq!(item.severity, Severity::Warning);
        assert!(item.action.contains("6.00%"), "corrected: {}", item.action);
        assert!(item.action.contains("8.00%"), "reported: {}", item.action);
        assert!(
            item.evidence.contains("4 distributions"),
            "{}",
            item.evidence
        );
        assert!(
            !out.iter().any(|item| item.finding.contains("flattered by dividends")),
            "the estimate must not run beside the measurement: {}",
            findings(&out)
        );
    }

    #[test]
    fn a_margin_that_is_entirely_dividends_is_blocking_rather_than_a_warning() {
        // The case that changes a decision: the rule did not beat holding, and
        // the verdict above it was reached on the uncorrected figure. A warning
        // beside a passing verdict would be read as a caveat on a win.
        let out = recommend(&measured(0.01, gap(0.03, 1, 1)));
        let item = out
            .iter()
            .find(|item| item.finding.contains("entirely dividends"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));

        assert_eq!(item.severity, Severity::Blocking);
        assert!(
            item.action.contains("did not beat holding"),
            "{}",
            item.action
        );
        assert!(item.action.contains("-2.00%"), "{}", item.action);
    }

    #[test]
    fn on_a_total_return_series_the_gap_describes_the_margin_instead_of_correcting_it() {
        // Same measurement, different claim. Subtracting here would take the
        // distributions out of a figure that already contains them and report
        // a margin smaller than the account earned — so the recommendation must
        // not quote a corrected number, and must not be Blocking on a rule that
        // did beat holding.
        let mut gap = gap(0.03, 1, 1);
        gap.adjustment = arvo_data::source::Adjustment::TotalReturn;
        let out = recommend(&measured(0.01, gap));

        let item = out
            .iter()
            .find(|item| item.finding.contains("dividends rather than timing"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));

        assert_eq!(item.severity, Severity::Note);
        assert!(item.action.contains("Do not subtract"), "{}", item.action);
        assert!(
            item.evidence.contains("4 distributions"),
            "the measurement still stands: {}",
            item.evidence
        );
        assert!(
            !out.iter().any(|item| {
                item.finding.contains("entirely dividends")
                    || item.finding.contains("smaller than it reads")
            }),
            "no correction may be offered on this basis: {}",
            findings(&out)
        );
        // And the -2.00% the split-adjusted case reports must appear nowhere.
        assert!(
            !out.iter().any(|item| item.action.contains("-2.00%")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_measured_gap_too_small_to_matter_is_not_mentioned_at_all() {
        // Deliberately silent. A line saying "this bias is negligible" on every
        // result is a line that teaches a reader to skip the section the day it
        // is not — and this one is measured, so silence is a real answer rather
        // than an omission.
        let out = recommend(&measured(0.08, gap(0.0001, 1, 1)));
        assert!(
            !out.iter().any(|item| {
                item.finding.contains("dividend") || item.finding.contains("smaller than it reads")
            }),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_partly_covered_book_says_the_figure_is_a_floor() {
        // A precise, understated number presented as the answer is worse than
        // no number, because it invites belief.
        let out = recommend(&measured(0.08, gap(0.02, 1, 3)));
        let item = out
            .iter()
            .find(|item| item.finding.contains("smaller than it reads"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));

        assert!(item.evidence.contains("floor"), "{}", item.evidence);
        assert!(
            item.evidence.contains("2 of 3"),
            "names what is missing: {}",
            item.evidence
        );
    }

    #[test]
    fn measuring_no_distributions_at_all_is_better_news_than_not_measuring() {
        // `Some(events: 0)` means the series was there and these instruments
        // pay nothing, so the excess return needs no correction. That must not
        // fall back to the estimate, which would claim an unknown bias where
        // one has been shown not to exist.
        let mut study = measured(
            0.08,
            crate::DividendGap {
                events: 0,
                strategy_income: 0.0,
                benchmark_income: 0.0,
                overstatement: 0.0,
                adjustment: arvo_data::source::Adjustment::Split,
                covered: 1,
                instruments: 1,
            },
        );
        let trades = &mut study.out_of_sample_evidence.evaluation.strategy_trades;
        trades.closed = 10;
        trades.average_holding_secs = Some(3.0 * 86_400.0);

        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("dividend")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_rule_that_sits_out_most_of_the_window_is_told_the_margin_is_flattered() {
        // The bias nothing else here can see, and it always favours the
        // strategy: prices are split-adjusted but not total-return adjusted,
        // so no dividend is paid to anything — and the benchmark holds through
        // every ex-date while the rule holds through only some.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        study.out_of_sample_evidence.experiment.window = crate::DateRange::new(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
            chrono::NaiveDate::from_ymd_opt(2024, 12, 31).expect("valid"),
        )
        .expect("ordered");
        let trades = &mut study.out_of_sample_evidence.evaluation.strategy_trades;
        trades.closed = 10;
        // Ten trades of three days each across a year: about 8% of it.
        trades.average_holding_secs = Some(3.0 * 86_400.0);

        let out = recommend(&study);
        let item = out
            .iter()
            .find(|item| item.finding.contains("flattered by dividends"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Warning);
        assert!(item.evidence.contains("8%"), "{}", item.evidence);
    }

    #[test]
    fn a_rule_that_is_always_in_the_market_is_not_warned() {
        // It forgoes the same dividends the benchmark does, so the comparison
        // is very nearly fair and the line would be noise.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        study.out_of_sample_evidence.experiment.window = crate::DateRange::new(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
            chrono::NaiveDate::from_ymd_opt(2024, 12, 31).expect("valid"),
        )
        .expect("ordered");
        let trades = &mut study.out_of_sample_evidence.evaluation.strategy_trades;
        trades.closed = 1;
        trades.average_holding_secs = Some(365.0 * 86_400.0);

        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("flattered by dividends")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_run_that_never_closed_a_trade_makes_no_exposure_claim() {
        // Zero exposure would fire this on every run that did not trade, where
        // the trade-count bar has already said the only useful thing.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.evaluation.strategy_trades = TradeStats::default();
        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("flattered by dividends")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_sharpe_that_could_still_be_zero_is_flagged() {
        // The objection deflation cannot make: this run had no grid behind it
        // at all, and the Sharpe is still an estimate with a standard error.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(1.2);
        study.out_of_sample_evidence.evaluation.strategy.psr = Some(0.62);

        let out = recommend(&study);
        let item = out
            .iter()
            .find(|item| item.finding.contains("not distinguishable"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Warning);
        assert!(item.evidence.contains("62%"), "{}", item.evidence);
    }

    #[test]
    fn a_well_evidenced_sharpe_is_left_alone() {
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(1.2);
        study.out_of_sample_evidence.evaluation.strategy.psr = Some(0.99);

        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("not distinguishable")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_losing_sharpe_is_not_also_told_it_is_uncertain() {
        // It has already lost on the return. Piling a weak objection onto a
        // decided question is how a list of recommendations stops being read.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(-0.4);
        study.out_of_sample_evidence.evaluation.strategy.psr = Some(0.10);

        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("not distinguishable")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_finding_recorded_before_psr_existed_makes_no_claim() {
        let mut study = book_study(&["AAPL.NASDAQ"]);
        study.out_of_sample_evidence.experiment.alongside.clear();
        study.out_of_sample_evidence.evaluation.strategy.sharpe = Some(1.2);
        assert!(study.out_of_sample_evidence.evaluation.strategy.psr.is_none());

        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("not distinguishable")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_book_member_that_never_traded_is_named_and_blocks() {
        // Found by running two identical instruments against an account with
        // room for one: the second was denied every fill, the return looked
        // like a single-instrument run, and the finding claimed to be about
        // two instruments.
        let out = recommend(&book_study(&["AAPL.NASDAQ"]));
        let item = out
            .iter()
            .find(|item| item.finding.contains("never traded"))
            .unwrap_or_else(|| panic!("{}", findings(&out)));
        assert_eq!(item.severity, Severity::Blocking);
        assert!(
            item.evidence.contains("MSFT.NASDAQ"),
            "the silent member should be named: {}",
            item.evidence
        );
    }

    #[test]
    fn a_book_where_everyone_traded_says_nothing_about_it() {
        let out = recommend(&book_study(&["AAPL.NASDAQ", "MSFT.NASDAQ"]));
        assert!(
            !out.iter().any(|item| item.finding.contains("never traded")),
            "{}",
            findings(&out)
        );
    }

    #[test]
    fn a_ledger_from_before_trades_named_instruments_makes_no_accusation() {
        // Every stored finding has an unnamed ledger. Reading that as "no
        // instrument traded" would accuse all of them of crowding out.
        let mut study = book_study(&["AAPL.NASDAQ"]);
        for round_trip in &mut study.out_of_sample_evidence.evaluation.strategy_ledger {
            round_trip.instrument = String::new();
        }
        let out = recommend(&study);
        assert!(
            !out.iter().any(|item| item.finding.contains("never traded")),
            "{}",
            findings(&out)
        );
    }
}
