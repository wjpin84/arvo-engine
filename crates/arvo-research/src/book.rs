//! What holding all of them at once would have done.
//!
//! A panel reports its members one by one and then averages them. An average
//! of returns is not a portfolio: it says what the typical instrument did, not
//! what an account holding all of them would have experienced. Those differ in
//! exactly the place that decides whether a rule is usable — the **drawdown**.
//! Three instruments that each fell 20% at different times combine into a book
//! that fell rather less than 20%, and a mean of drawdowns cannot express that
//! because it averages three numbers that never happened together.
//!
//! So this combines the curves instead of the summaries, and evaluates the
//! result as one account.
//!
//! # What it assumes, all of it stated
//!
//! **Equal weight, set once, never rebalanced.** Capital is divided evenly at
//! the start and each sleeve then runs on its own. Rebalancing back to equal
//! weight would be a different and more flattering strategy — it sells what
//! rose and buys what fell — and pretending it happened for free is how
//! backtested portfolios acquire returns nobody could have taken.
//!
//! **Rescaled, not re-simulated.** Each member ran with the whole account
//! behind it; here each gets a share. Returns are fractional so the *shape*
//! carries over exactly, but this cannot model the one thing a real portfolio
//! run would: capital contention. A rule that wanted to be in all three at
//! once and could only afford two would show up in a single-account backtest
//! and cannot show up here. That is the honest limit of this method, and the
//! reason a true multi-instrument engine run is still worth building.
//!
//! **A member with no bar at some instant is held, not sold.** Its last known
//! value carries forward, which is what actually happens to a position on a
//! day its instrument does not trade.

use std::collections::{BTreeMap, BTreeSet};

use crate::EquityPoint;

/// Combines several equity curves into the one account that held them all.
///
/// Returns `None` for fewer than two curves — one instrument is not a book —
/// or when no instant is covered by anything.
#[must_use]
pub fn combine(starting_cash: f64, curves: &[(String, Vec<EquityPoint>)]) -> Option<Vec<EquityPoint>> {
    let usable: Vec<&Vec<EquityPoint>> = curves
        .iter()
        .map(|(_, curve)| curve)
        .filter(|curve| curve.first().is_some_and(|point| point.equity > 0.0))
        .collect();
    if usable.len() < 2 {
        return None;
    }

    #[expect(clippy::cast_precision_loss, reason = "panel sizes are small")]
    let sleeve = starting_cash / usable.len() as f64;

    // Every instant any member reported, so a book of instruments on different
    // calendars is measured on all of their days rather than only the days
    // they happen to share.
    let instants: BTreeSet<chrono::NaiveDateTime> = usable
        .iter()
        .flat_map(|curve| curve.iter().map(|point| point.at))
        .collect();
    if instants.is_empty() {
        return None;
    }

    // Each member as a lookup, so a missing instant can carry the last known
    // value forward rather than reading as a fall to zero.
    let series: Vec<(f64, BTreeMap<chrono::NaiveDateTime, f64>)> = usable
        .iter()
        .map(|curve| {
            let opening = curve.first().map_or(1.0, |point| point.equity);
            let points = curve
                .iter()
                .map(|point| (point.at, point.equity))
                .collect::<BTreeMap<_, _>>();
            (opening, points)
        })
        .collect();

    let mut combined = Vec::with_capacity(instants.len());
    let mut last: Vec<f64> = series.iter().map(|_| 1.0).collect();

    for at in instants {
        let mut equity = 0.0;
        for (index, (opening, points)) in series.iter().enumerate() {
            if let Some(value) = points.get(&at) {
                last[index] = value / opening;
            }
            // Before a member's first point its sleeve sits in cash, which is
            // the ratio of 1.0 it starts at.
            equity += sleeve * last[index];
        }
        combined.push(EquityPoint { at, equity });
    }
    Some(combined)
}

/// How much less the book fell than its members did on average.
///
/// The whole reason to combine curves rather than average summaries. A
/// positive number is diversification: drawdowns that did not coincide. Zero
/// or negative means the members fell together, and the panel was one bet.
#[must_use]
pub fn diversification(book_drawdown: f64, mean_member_drawdown: f64) -> f64 {
    mean_member_drawdown - book_drawdown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(values: &[f64]) -> (String, Vec<EquityPoint>) {
        let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN);
        (
            "X".to_owned(),
            values
                .iter()
                .enumerate()
                .map(|(index, equity)| EquityPoint {
                    at: start + chrono::Duration::days(index as i64),
                    equity: *equity,
                })
                .collect(),
        )
    }

    fn at(day: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn drawdown(curve: &[EquityPoint]) -> f64 {
        let mut peak = f64::NEG_INFINITY;
        let mut worst: f64 = 0.0;
        for point in curve {
            peak = peak.max(point.equity);
            worst = worst.max((peak - point.equity) / peak);
        }
        worst
    }

    #[test]
    fn a_book_of_two_equal_sleeves_starts_at_the_whole_account() {
        let book = combine(1_000.0, &[curve(&[100.0, 110.0]), curve(&[50.0, 55.0])])
            .expect("two curves");
        assert!((book[0].equity - 1_000.0).abs() < 1e-9);
        // Both rose 10%, so the book rose 10%.
        assert!((book[1].equity - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn falls_that_do_not_coincide_hurt_the_book_less_than_they_hurt_its_members() {
        // The number a mean of drawdowns cannot express, because it averages
        // three figures that never happened at the same time.
        let early = curve(&[100.0, 60.0, 100.0, 100.0, 100.0]);
        let late = curve(&[100.0, 100.0, 100.0, 60.0, 100.0]);
        let mean_member = (drawdown(&early.1) + drawdown(&late.1)) / 2.0;

        let book = combine(1_000.0, &[early, late]).expect("two curves");
        let book_drawdown = drawdown(&book);

        assert!(
            book_drawdown < mean_member,
            "book {book_drawdown} should be gentler than the members' {mean_member}"
        );
        assert!(diversification(book_drawdown, mean_member) > 0.0);
    }

    #[test]
    fn falls_that_do_coincide_give_the_book_no_relief() {
        // Two instruments that move together are one bet, and the book has to
        // say so rather than averaging its way to a softer number.
        let together = |()| curve(&[100.0, 60.0, 100.0]);
        let members = [together(()), together(())];
        let mean_member = drawdown(&members[0].1);

        let book = combine(1_000.0, &members).expect("two curves");
        assert!(
            (drawdown(&book) - mean_member).abs() < 1e-9,
            "identical members give no diversification at all"
        );
    }

    #[test]
    fn a_member_with_no_point_at_an_instant_is_held_rather_than_sold() {
        // Which is what happens to a position on a day its instrument does not
        // trade. Reading the gap as zero would show the book collapsing on
        // every holiday one member observed and the other did not.
        let dense = curve(&[100.0, 100.0, 100.0]);
        let sparse = (
            "SPARSE".to_owned(),
            vec![
                EquityPoint { at: at(1), equity: 100.0 },
                EquityPoint { at: at(3), equity: 100.0 },
            ],
        );

        let book = combine(1_000.0, &[dense, sparse]).expect("two curves");
        assert_eq!(book.len(), 3, "measured on every day either member reported");
        assert!(
            book.iter().all(|point| (point.equity - 1_000.0).abs() < 1e-9),
            "nothing moved, so the book did not: {book:?}"
        );
    }

    #[test]
    fn one_instrument_is_not_a_book() {
        assert!(combine(1_000.0, &[curve(&[100.0, 110.0])]).is_none());
    }

    #[test]
    fn a_member_that_opened_at_zero_is_left_out_rather_than_dividing_by_it() {
        let good = curve(&[100.0, 110.0]);
        let broken = curve(&[0.0, 0.0]);
        assert!(
            combine(1_000.0, &[good, broken]).is_none(),
            "one usable curve is not a book either"
        );
    }

    #[test]
    fn the_book_is_not_a_rebalanced_one() {
        // Equal weight set once, never restored. A rebalanced book sells what
        // rose and buys what fell, which is a different and more flattering
        // strategy — and pretending it happened for free is how backtested
        // portfolios acquire returns nobody could have taken.
        let winner = curve(&[100.0, 200.0, 200.0]);
        let loser = curve(&[100.0, 50.0, 50.0]);
        let book = combine(1_000.0, &[winner, loser]).expect("two curves");

        // 500 doubled plus 500 halved is 1,250 and stays there. A rebalanced
        // book would have sold the winner down at day two and would sit
        // somewhere else entirely.
        assert!((book[1].equity - 1_250.0).abs() < 1e-9, "{:?}", book[1]);
        assert!((book[2].equity - 1_250.0).abs() < 1e-9);
    }
}
