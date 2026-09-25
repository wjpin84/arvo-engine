//! The leaderboard (#226): every comparable finding, in one order and only
//! one.
//!
//! Supported under the conservative cost tier first, ordered by the
//! expectancy measured under that tier; then Supported under the stated
//! costs where the conservative tier was never measured (findings older than
//! #192); then everything else. Ties by drawdown, then by trades. Raw return
//! is a column to read and never the order: a ranking by raw return is the
//! overfitting machine every screener ships, and ADR 0030 says Arvo does not
//! ship one.

use std::collections::BTreeSet;

use arvo_api::research::{Ranking, RankingRowView};
use arvo_research::Record;

use super::history::{comparable, live_version};
use super::views::verdict_label;
use super::{CommandError, ResearchService};

/// Every comparable finding under the store, filtered and ordered.
///
/// `rule` matches the finding's rule name; `instrument` the finding's
/// instrument or subject. Panels are not rows: one configuration across many
/// instruments is not the same number as one instrument, and the table would
/// invite reading one against the other.
///
/// # Errors
///
/// The store cannot be listed. A finding that cannot be read is a note, not
/// a failure.
pub fn rank(service: &ResearchService, rule: Option<&str>, instrument: Option<&str>) -> Result<Ranking, CommandError> {
    let (summaries, unreadable) = service.memory.summaries().map_err(|err| CommandError::Failed(err.to_string()))?;
    let mut notes: Vec<String> = unreadable.into_iter().map(|item| format!("{}: {}", item.id, item.reason)).collect();
    let mut rows = Vec::new();
    for summary in &summaries {
        if instrument.is_some_and(|wanted| summary.instrument.as_deref() != Some(wanted) && summary.subject != wanted) {
            continue;
        }
        let stored = match service.memory.open(&summary.id) {
            Ok(stored) => stored,
            Err(err) => {
                notes.push(format!("{}: {err}", summary.id));
                continue;
            }
        };
        let Some((evaluation, rule_name)) = comparable(&stored.record) else { continue };
        if rule.is_some_and(|wanted| rule_name != wanted && !rule_name.starts_with(&format!("{wanted} "))) {
            continue;
        }
        let conservative = match &stored.record {
            Record::Study(evidence) => evidence.conservative.clone(),
            _ => None,
        };
        let (expectancy, expectancy_costs) = match &conservative {
            Some(costed) => (costed.expectancy, "conservative"),
            None => {
                // Mean profit per closed trade, under the costs the ledger was run at.
                let closed: Vec<f64> = evaluation.strategy_ledger.iter().filter(|trade| trade.closed.is_some()).map(|trade| trade.pnl).collect();
                (if closed.is_empty() { 0.0 } else { closed.iter().sum::<f64>() / closed.len() as f64 }, "stated")
            }
        };
        let regimes: BTreeSet<String> =
            evaluation.strategy_ledger.iter().filter_map(|trade| trade.journal.as_ref()?.regime.clone()).collect();
        rows.push(RankingRowView {
            id: summary.id.clone(),
            subject: summary.subject.clone(),
            kind: summary.kind.clone(),
            rule: rule_name,
            interval: summary.interval.map(|interval| interval.to_string()).unwrap_or_default(),
            verdict: verdict_label(summary.verdict).to_owned(),
            conservative_verdict: conservative.as_ref().map(|costed| verdict_label(costed.verdict).to_owned()).unwrap_or_default(),
            expectancy,
            expectancy_costs: expectancy_costs.to_owned(),
            total_return: evaluation.strategy.total_return,
            max_drawdown: evaluation.strategy.max_drawdown,
            trades: evaluation.strategy.trades,
            regimes: regimes.into_iter().collect(),
            search: summary.trials.and_then(|trials| u32::try_from(trials).ok()),
            recorded_at: summary.recorded_at.format("%Y-%m-%d %H:%M").to_string(),
            stale: live_version(service, summary).map(|live| live != summary.dataset_version),
        });
    }
    order(&mut rows);
    Ok(Ranking { rows, notes })
}

/// The one order. Public so the guard test can hold it to account without a
/// store.
pub fn order(rows: &mut [RankingRowView]) {
    rows.sort_by(|a, b| {
        tier(a)
            .cmp(&tier(b))
            .then_with(|| b.expectancy.total_cmp(&a.expectancy))
            .then_with(|| a.max_drawdown.total_cmp(&b.max_drawdown))
            .then_with(|| b.trades.cmp(&a.trades))
    });
}

/// 0: Supported under the conservative tier. 1: Supported under the stated
/// costs, conservative never measured. 2: everything else, whatever its
/// number.
fn tier(row: &RankingRowView) -> u8 {
    match (row.conservative_verdict.as_str(), row.verdict.as_str()) {
        ("Supported", _) => 0,
        ("", "Supported") => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, verdict: &str, conservative: &str, expectancy: f64, total_return: f64, max_drawdown: f64) -> RankingRowView {
        RankingRowView {
            id: id.to_owned(),
            verdict: verdict.to_owned(),
            conservative_verdict: conservative.to_owned(),
            expectancy,
            expectancy_costs: if conservative.is_empty() { "stated" } else { "conservative" }.to_owned(),
            total_return,
            max_drawdown,
            trades: 40,
            ..RankingRowView::default()
        }
    }

    #[test]
    fn a_higher_raw_return_with_a_lower_conservative_expectancy_ranks_below_its_neighbour() {
        // This test is the point of the feature.
        let mut rows = vec![
            row("raw", "Supported", "Supported", 3.0, 0.90, 0.10),
            row("costed", "Supported", "Supported", 12.0, 0.20, 0.10),
            row("old", "Supported", "", 50.0, 0.60, 0.05),
            row("loud", "NotSupported", "NotSupported", 200.0, 2.50, 0.02),
            row("quiet", "Inconclusive", "", 1.0, 0.01, 0.01),
        ];
        order(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["costed", "raw", "old", "loud", "quiet"]);
    }

    #[test]
    fn ties_break_by_drawdown_then_trades() {
        let mut a = row("deeper", "Supported", "Supported", 5.0, 0.3, 0.20);
        let mut b = row("shallower", "Supported", "Supported", 5.0, 0.3, 0.10);
        let mut c = row("busier", "Supported", "Supported", 5.0, 0.3, 0.10);
        a.trades = 40;
        b.trades = 40;
        c.trades = 60;
        let mut rows = vec![a, b, c];
        order(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["busier", "shallower", "deeper"]);
    }
}
