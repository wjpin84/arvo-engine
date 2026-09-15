//! What to say about a walk-forward.

use super::*;
use crate::{Verdict, WalkForwardEvidence};

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

    if let Some(item) = lost_money(found.combined.total_return, found.excess_return) {
        out.push(item);
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
    // The worst shock any fold's positions took.
    let stress = found
        .folds
        .iter()
        .filter_map(|fold| fold.out_of_sample_evidence.evaluation.stress.as_ref())
        .max_by(|a, b| {
            let loss = |s: &crate::stress::Stress| s.worst().map_or(f64::NEG_INFINITY, |w| w.loss);
            loss(a).total_cmp(&loss(b))
        });
    out.extend(option_tail(&found.template, &found.combined_curve, &fold_ledgers, stress));

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

    out.extend(shape_warnings(
        &found.combined_trades,
        found.template.risk.stop_atr_multiple.is_some(),
    ));

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
