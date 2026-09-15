//! What to say about a panel.

use super::*;
use crate::{EvaluationCriteria, PanelEvidence, Verdict};

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

    if !found.ended_early.is_empty() {
        out.push(Recommendation::new(
            Severity::Warning,
            "Part of the panel stopped trading before the window ended, and is valued at its last close.",
            "Do not read those members' returns as what holding them paid. A delisted stock's last \
             exchange price is rarely what its holders received; find what they did get, or run \
             the panel without them and compare.",
            format!(
                "{} of {} members ended early: {}",
                found.ended_early.len(),
                pooled.instruments,
                found.ended_early.join(", ")
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

    // After the note on purpose: this is about how the members were chosen,
    // not about what they did, so it must not stop a clean panel being told
    // it is clean. It fires on every panel because every panel earns it —
    // members come from the library, the library holds what trades today,
    // and nothing records what was in an index or delisted (#9). A company
    // that went bankrupt mid-window is not a loser here; it is absent.
    let securities = if pooled.distinct == 0 { pooled.instruments } else { pooled.distinct };
    if securities > 1 {
        out.push(Recommendation::new(
            Severity::Warning,
            "Every member was chosen from companies that still trade, so the panel holds no failures.",
            "Read the pooled return as an upper bound. Companies that were delisted or dropped from \
             an index during the window would have been in a universe picked on its first day, \
             and they are the ones a long rule loses most on.",
            format!(
                "{securities} securities judged over {} to {}, {} of them a casualty of that window",
                found.in_sample.from,
                found.out_of_sample.to,
                match found.ended_early.len() {
                    0 => "none".to_owned(),
                    n => n.to_string(),
                },
            ),
        ));
    }

    out.sort_by_key(|item| item.severity);
    out
}
