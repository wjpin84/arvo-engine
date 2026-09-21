//! What to say about a single study.

use super::*;
use crate::{Evaluation, FamilyEvidence, Verdict};

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

    if let Some(costly) = found.under_conservative_costs.filter(|costly| *costly != Verdict::Supported) {
        let stated = &found.selected.costs;
        let worse = stated.at(crate::risk::CostTier::Conservative);
        out.push(Recommendation::new(
            Severity::Blocking,
            "Supported only under the stated costs.",
            "Do not promote this. Widen the edge or trade less often until the \
             rule survives fills that cost twice what was assumed; a result \
             that holds only when fills are cheap is a result about the cost \
             assumption.",
            format!(
                "Supported at {:.1} bps commission and {:.1} bps slippage; {costly:?} at {:.1} and {:.1}",
                stated.commission_bps, stated.slippage_bps, worse.commission_bps, worse.slippage_bps
            ),
        ));
    }

    if let Some(costly) = found.under_conservative_costs.filter(|costly| *costly != Verdict::Supported) {
        let stated = &found.selected.costs;
        let worse = stated.at(crate::risk::CostTier::Conservative);
        out.push(Recommendation::new(
            Severity::Blocking,
            "Supported only under the stated costs.",
            "Do not promote this. Widen the edge or trade less often until the \
             rule survives fills that cost twice what was assumed; a result \
             that holds only when fills are cheap is a result about the cost \
             assumption.",
            format!(
                "Supported at {:.1} bps commission and {:.1} bps slippage; {costly:?} at {:.1} and {:.1}",
                stated.commission_bps, stated.slippage_bps, worse.commission_bps, worse.slippage_bps
            ),
        ));
    }

    if let Some(item) = refusals(evaluation.refused_orders) {
        out.push(item);
    }

    if let Some(item) = lost_money(evaluation.strategy.total_return, evaluation.excess_return) {
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
        evaluation.stress.as_ref(),
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

    out.extend(shape_warnings(
        trades,
        found.out_of_sample_evidence.experiment.risk.stop_atr_multiple.is_some(),
    ));

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
    out.extend(dividend_advice(evaluation, found));

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
                 treatment. The verdict is judged pre-tax; read the after-tax \
                 rows under the metrics, and redo them at your own bracket \
                 before comparing this with holding.",
            )
        } else {
            (
                "Positions were held for more than a year on average.",
                "In a taxable account this is long-term treatment, which is \
                 the favourable case. The verdict is still judged pre-tax; \
                 the after-tax rows under the metrics show the difference.",
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

/// The dividend bias in the margin over buy-and-hold: measured when a
/// distribution series is on disk, estimated from time in the market when not.
fn dividend_advice(evaluation: &Evaluation, found: &FamilyEvidence) -> Option<Recommendation> {
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
                    Some(Recommendation::new(
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
                    ))
                }
                // Total-return adjusted: the distribution is already in the
                // returns, so the margin above is right and subtracting the gap
                // again would double-count it. What is left to say is what the
                // margin is *made of*, which is a different and quieter claim.
                None => Some(Recommendation::new(
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
        Some(_) => None,
        // No series for any instrument the run held. The bias is still real and
        // still points the same way; only its size is unknown, which is what
        // this said in words before it could be measured at all.
        None => {
            let exposure = time_in_market(found)?;
            (exposure < EXPOSURE_WORTH_SAYING).then(|| {
                Recommendation::new(
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
                )
            })
        }
    }
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
