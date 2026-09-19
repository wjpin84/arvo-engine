//! Pulling data in: search, fetch, and a second source's opinion.

use super::*;

/// A two-source comparison in words, with the verdict separated from the count.
///
/// Two vendors disagreeing is the normal case, and most of the ways they
/// disagree are not faults — a different adjustment basis, a different session,
/// a different volume basis. Reporting "4,812 bars disagree" across any of those
/// is true, useless, and the kind of thing that gets a check switched off. So
/// the disagreement is classified first and counted second, and only a genuine
/// divergence is flagged.
pub fn source_comparison_view(outcome: &source::Comparison) -> SourceComparisonView {
    use arvo_data::agreement::Agreement;

    let (summary, diverged) = match &outcome.agreement {
        Agreement::NoOverlap => (
            "no bar instant appears in both series, so there is nothing to compare"
                .to_owned(),
            false,
        ),
        Agreement::Aligned { compared } => (
            format!("all {compared} shared bars agree within tolerance"),
            false,
        ),
        Agreement::Rescaled { factor, compared } => (
            format!(
                "the {compared} shared bars differ by a near-constant factor of {factor:.4} — almost certainly a different adjustment basis rather than bad data on either side, and they cannot be used together until one is restated"
            ),
            false,
        ),
        // Declared different adjustments: the series are meant to differ, by a
        // factor that steps at each distribution, so the gap is the adjustment
        // accumulating and not a price anyone got wrong.
        Agreement::Diverged {
            disagreeing,
            compared,
            worst,
            ..
        } if outcome.adjustments_differ => (
            format!(
                "{disagreeing} of {compared} shared bars differ, by up to {:.2}% — what a different adjustment basis accumulates to over this window, not bad data on either side",
                worst * 100.0,
            ),
            false,
        ),
        Agreement::Diverged {
            disagreeing,
            compared,
            worst,
            at,
        } => (
            format!(
                "{disagreeing} of {compared} shared bars genuinely disagree; the worst is {:.2}% on {} — at least one of these sources has prices nobody traded at",
                worst * 100.0,
                at.format("%Y-%m-%d")
            ),
            true,
        ),
    };

    SourceComparisonView {
        symbol: outcome.symbol.clone(),
        interval: outcome.interval.to_string(),
        first: outcome.first.to_owned(),
        second: outcome.second.to_owned(),
        first_bars: outcome.first_bars,
        second_bars: outcome.second_bars,
        shared: outcome.coverage.shared,
        only_first: outcome.coverage.only_first,
        only_second: outcome.coverage.only_second,
        summary,
        diverged,
        basis_mismatch: outcome.basis_mismatch.clone(),
    }
}

/// What a re-fetch changed, in words.
///
/// The distinction that matters is between a *rescaling* and a *revision*. A
/// rescaling is what a corporate action does to a whole series at once: every
/// price moves by the same factor, nothing that happened has been contradicted,
/// and a stored finding is stale only in the sense that its numbers are now
/// quoted in different units. A revision is a source changing its mind about
/// individual prices, and a finding drawn from the old ones rested on
/// something that source no longer stands behind.
pub fn describe_revision(agreement: &arvo_data::agreement::Agreement) -> String {
    use arvo_data::agreement::Agreement;
    match agreement {
        Agreement::NoOverlap => {
            "covers a different period from the copy already held".to_owned()
        }
        Agreement::Aligned { compared } => {
            format!("matches the {compared} bars already held")
        }
        Agreement::Rescaled { factor, compared } => format!(
            "every one of {compared} bars moved by the same factor of {factor:.4} \u{2014} a \
             corporate action re-adjustment, not a change of mind about any price"
        ),
        Agreement::Diverged {
            disagreeing,
            compared,
            worst,
            at,
        } => format!(
            "{disagreeing} of {compared} bars now hold different prices, the worst by \
             {:.2}% on {at} \u{2014} the source has revised history rather than re-adjusted it",
            worst * 100.0,
        ),
    }
}

#[cfg(test)]
mod comparison_view_tests {
    use super::source_comparison_view;
    use arvo_data::agreement::{Agreement, Coverage};
    use arvo_data::source::Comparison;

    fn diverged(adjustments_differ: bool) -> Comparison {
        Comparison {
            symbol: "KO".to_owned(),
            interval: arvo_data::BarInterval::DAILY,
            first: "yahoo-tr",
            second: "yahoo",
            first_bars: 6284,
            second_bars: 6284,
            basis_mismatch: adjustments_differ.then(|| "different adjustment".to_owned()),
            adjustments_differ,
            agreement: Agreement::Diverged {
                disagreeing: 6222,
                compared: 6284,
                worst: 0.5082,
                at: chrono::NaiveDate::from_ymd_opt(2001, 10, 31)
                    .expect("valid")
                    .and_time(chrono::NaiveTime::MIN),
            },
            coverage: Coverage {
                shared: 6284,
                only_first: 0,
                only_second: 0,
            },
        }
    }

    #[test]
    fn a_total_return_series_against_a_split_one_is_not_called_bad_data() {
        // What the live cross-check said on 2026-09-14, about two correct
        // series: "6222 of 6284 genuinely disagree — at least one of these has
        // prices nobody traded at", in red.
        let view = source_comparison_view(&diverged(true));
        assert!(!view.diverged, "declared different adjustments are expected to differ");
        assert!(view.summary.contains("adjustment"), "{}", view.summary);
        assert!(!view.summary.contains("nobody traded"), "{}", view.summary);
    }

    #[test]
    fn the_same_divergence_on_the_same_basis_is_still_flagged() {
        let view = source_comparison_view(&diverged(false));
        assert!(view.diverged);
        assert!(view.summary.contains("genuinely disagree"), "{}", view.summary);
    }
}
