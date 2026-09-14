//! Checking a result against itself.
//!
//! The platform's premise is that most backtest results are noise. The failure
//! that premise cannot survive is a *plausible* result that is simply wrong —
//! and one shipped: Nautilus's `returns_series` is the day-over-day change in
//! the account's cash balance, not an equity curve, so an intraday run
//! reported +98% on two losing trades with a Sharpe of 40. Totals were right,
//! which is exactly why nobody noticed for weeks.
//!
//! It was found by one check: does the ledger's realised profit agree with
//! where the equity curve ended? That check existed only because somebody
//! wrote it. This module is the rest of them.
//!
//! # Why these run on every result rather than in a test
//!
//! A test checks a fixture. These check the run the reader is about to
//! believe. The bug above passed every test in the crate, because every test
//! asserted things about the shape of the output rather than about its
//! agreement with the other output beside it.
//!
//! Each invariant compares two quantities the engine computed *independently*.
//! That is the whole design: a number checked against a restatement of itself
//! catches nothing, and a number checked against a differently-derived version
//! of the same fact catches a broken engine.
//!
//! # Why nothing here is a bound the engine could not meet
//!
//! A check that cries wolf stops being read, and this codebase has already
//! done that once — the data-quality outlier test compared absolute price
//! ranges and reported 159 findings in 5,000 bars of a fixture with nothing
//! wrong with it. Every tolerance below is stated with the arithmetic it has
//! to absorb, and two of the four are inequalities rather than equalities
//! because that is what is actually true.

use crate::{Experiment, SimulationResult, Trade};

/// How far two independently-derived money figures may differ.
///
/// Relative, because a hundred-thousand-dollar account accumulates more
/// floating-point error than a thousand-dollar one, and a fixed tolerance
/// would either cry wolf on the large or miss real breakage on the small.
const RELATIVE: f64 = 1e-6;

/// The floor under [`RELATIVE`], in account currency.
///
/// A cent. Below this the comparison is measuring the last bits of an `f64`
/// rather than anything about the run, and a run whose books are out by less
/// than a cent has nothing wrong with it.
const ABSOLUTE: f64 = 0.01;

/// One place the engine disagreed with itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Discrepancy {
    /// Which invariant failed, named as the claim it makes.
    pub invariant: &'static str,
    /// What the check derived from one source, and from the other. Both are
    /// carried so a reader can see the size and direction of the gap rather
    /// than being told only that there was one.
    pub expected: f64,
    pub actual: f64,
    /// Enough context to start looking, without opening the ledger.
    pub detail: String,
}

impl Discrepancy {
    fn new(invariant: &'static str, expected: f64, actual: f64, detail: String) -> Self {
        Self {
            invariant,
            expected,
            actual,
            detail,
        }
    }

    /// Size of the gap, in account currency.
    #[must_use]
    pub fn gap(&self) -> f64 {
        (self.actual - self.expected).abs()
    }
}

/// Whether two money figures agree to within the stated tolerance.
fn agrees(expected: f64, actual: f64) -> bool {
    (actual - expected).abs() <= ABSOLUTE.max(expected.abs() * RELATIVE)
}

/// Every way this result disagrees with itself.
///
/// Empty is the good case and the usual one. Anything here means the engine
/// produced two figures for the same fact and they differ, so the result
/// should not be read until it is understood — no verdict drawn from it is
/// trustworthy, however good the numbers look.
#[must_use]
pub fn reconcile(experiment: &Experiment, result: &SimulationResult) -> Vec<Discrepancy> {
    reconcile_parts(experiment, &result.equity_curve, &result.ledger)
}

/// The same checks, against the curve and ledger a *stored* finding kept.
///
/// Evidence read back out of research memory is not a [`SimulationResult`] —
/// it holds the curve and the ledger without the engine wrapper around them.
/// Taking the parts is what lets a finding recorded months ago be reconciled
/// without re-running it, which matters because the invariants are newer than
/// most of the findings on disk.
#[must_use]
pub fn reconcile_parts(
    experiment: &Experiment,
    curve: &[crate::EquityPoint],
    ledger: &[Trade],
) -> Vec<Discrepancy> {
    let mut out = Vec::new();
    out.extend(curve_ends_where_the_ledger_says(experiment, curve, ledger));
    out.extend(each_trade_matches_its_own_prices(ledger));
    out.extend(fees_match_the_cost_model(experiment, ledger));
    out.extend(drawdown_covers_the_realised_losses(curve, ledger));
    out
}

/// The curve's total movement is the ledger's realised profit plus whatever
/// is still open, marked at the last price the run saw.
///
/// The check that caught the `returns_series` bug, kept as an invariant rather
/// than a test. Both sides are independently derived: one from positions, one
/// from the account balance over time.
fn curve_ends_where_the_ledger_says(
    experiment: &Experiment,
    curve: &[crate::EquityPoint],
    ledger: &[Trade],
) -> Option<Discrepancy> {
    let (first, last) = (curve.first()?, curve.last()?);
    let moved = last.equity - first.equity;

    // Realised only. An open position's mark depends on a price this module
    // does not have, so a run still holding something is checked as a bound
    // instead — see below.
    let realised: f64 = ledger
        .iter()
        .filter(|trade| trade.closed.is_some())
        .map(|trade| trade.pnl)
        .sum();
    let still_open = ledger
        .iter()
        .filter(|trade| trade.closed.is_none())
        .count();

    if still_open > 0 {
        // Nothing to assert about the mark, but the opening balance is still
        // checkable and is where a units mix-up shows up first.
        return (!agrees(experiment.starting_cash, first.equity)).then(|| {
            Discrepancy::new(
                "the curve opens at the starting balance",
                experiment.starting_cash,
                first.equity,
                format!("{still_open} position(s) still open, so only the opening is checked"),
            )
        });
    }

    (!agrees(realised, moved)).then(|| {
        Discrepancy::new(
            "the curve ends where the ledger says it should",
            realised,
            moved,
            format!(
                "{} closed round trips realising {realised:.2}, against a curve that moved \
                 {moved:.2} from {:.2} to {:.2}",
                ledger.len(),
                first.equity,
                last.equity,
            ),
        )
    })
}

/// A closed trade's realised profit is what its own entry, exit, size and
/// commission say it is.
///
/// Nautilus reports the P&L and the prices separately. If they disagree, one
/// of the two is wrong and every statistic drawn from either is suspect — the
/// win rate and the profit factor come from P&L, the charts come from prices.
fn each_trade_matches_its_own_prices(ledger: &[Trade]) -> Vec<Discrepancy> {
    ledger
        .iter()
        .enumerate()
        .filter_map(|(index, trade)| {
            let exit = trade.exit?;
            trade.closed?;
            let direction = match trade.direction {
                crate::Direction::Long => 1.0,
                crate::Direction::Short => -1.0,
            };
            // Net of commission, because that is what `Trade::pnl` is: the
            // number that actually landed in the account.
            let from_prices = direction * (exit - trade.entry) * trade.quantity - trade.commission;
            (!agrees(from_prices, trade.pnl)).then(|| {
                Discrepancy::new(
                    "a trade's profit is what its own prices say",
                    from_prices,
                    trade.pnl,
                    format!(
                        "trade {index} in {}: {:.4} in, {exit:.4} out, {} units, \
                         {:.2} commission",
                        if trade.instrument.is_empty() {
                            "the run's instrument"
                        } else {
                            &trade.instrument
                        },
                        trade.entry,
                        trade.quantity,
                        trade.commission,
                    ),
                )
            })
        })
        .collect()
}

/// What the venue charged is what the cost model says it should have.
///
/// The engine applies the cost model; this reapplies it to the fills the
/// ledger reports. A mismatch means the run did not pay what the experiment
/// says it paid — and since the cost model is pinned into the reproducibility
/// record, a stored finding would then be unreproducible in a way nothing else
/// would notice.
///
/// Slippage is deliberately absent. It is charged inside the fill prices, so
/// it is already in `entry` and `exit` and never appears as a fee. Adding it
/// here would report every correct run as wrong by the spread.
fn fees_match_the_cost_model(experiment: &Experiment, ledger: &[Trade]) -> Option<Discrepancy> {
    let costs = &experiment.costs;
    let charged: f64 = ledger.iter().map(|trade| trade.commission).sum();

    // Every fill's commission is rounded to the account currency's smallest
    // unit before it is charged, so a reconstruction from average prices can
    // be out by up to half a cent per fill.
    //
    // Measured against the real engine rather than guessed: four round trips
    // at 2bps came out 0.024 below a reconstruction of 17.704, against a bound
    // of eight fills times half a cent. Derived from the mechanism, not
    // widened until the test passed — a tolerance chosen to silence a failure
    // silences the next real one too.
    const ROUNDING_PER_FILL: f64 = 0.005;

    let mut fills = 0.0;
    let mut expected = 0.0;
    for trade in ledger {
        // Settlement at expiry is not a trade on the venue and charges nothing
        // (#84), so an expired contract's close is not a fill to price.
        let settled = trade.exit_reason == crate::ExitReason::Expired;
        let entry_notional = trade.entry * trade.quantity;
        // One fill in, and one out only if it actually closed.
        let exit_notional = match trade.exit {
            Some(exit) if !settled => exit * trade.quantity,
            _ => 0.0,
        };
        let sides = if trade.exit.is_some() && !settled { 2.0 } else { 1.0 };
        fills += sides;

        expected += (entry_notional + exit_notional) * costs.commission_bps / 10_000.0;
        expected += costs.per_fill * sides;
        // Sell-side charges fall on whichever fill sold: a long's close, or a
        // short's open (#84). A settled close is not a fill at all.
        let sold = match trade.direction {
            crate::Direction::Long => (trade.exit.is_some() && !settled).then_some(exit_notional),
            crate::Direction::Short => Some(entry_notional),
        };
        if let Some(notional) = sold {
            expected += costs.per_unit_sold * trade.quantity;
            expected += notional * costs.sell_notional_bps / 10_000.0;
        }
    }

    let allowed = ABSOLUTE.max(expected.abs() * RELATIVE) + fills * ROUNDING_PER_FILL;
    ((charged - expected).abs() > allowed).then(|| {
        Discrepancy::new(
            "fees charged match the cost model",
            expected,
            charged,
            format!(
                "{} round trips over {fills:.0} fills at {:.1}bps commission, {:.2} per fill, \
                 {:.4} per unit sold, {:.1}bps on sells; allowed {allowed:.3}",
                ledger.len(),
                costs.commission_bps,
                costs.per_fill,
                costs.per_unit_sold,
                costs.sell_notional_bps,
            ),
        )
    })
}

/// The curve's worst fall is at least as deep as the worst run of realised
/// losses.
///
/// An inequality, not an equality, and deliberately so: the curve marks open
/// positions to market, so it sees drawdowns the closed-trade sequence cannot
/// — a position that halved and recovered never appears in realised P&L at
/// all. The curve can therefore be worse. It cannot be *better*, and a curve
/// showing a gentler fall than the trades actually took is a curve that is not
/// describing this run.
fn drawdown_covers_the_realised_losses(
    curve: &[crate::EquityPoint],
    ledger: &[Trade],
) -> Option<Discrepancy> {
    let opening = curve.first()?.equity;
    if opening <= 0.0 {
        return None;
    }

    // Worst peak-to-trough of the running realised total, as a fraction of the
    // opening balance — the same denominator the curve's drawdown uses.
    let mut running = 0.0;
    let mut peak: f64 = 0.0;
    let mut worst_realised: f64 = 0.0;
    for trade in ledger.iter().filter(|trade| trade.closed.is_some()) {
        running += trade.pnl;
        peak = peak.max(running);
        worst_realised = worst_realised.max((peak - running) / (opening + peak));
    }

    let mut curve_peak = f64::NEG_INFINITY;
    let mut curve_worst: f64 = 0.0;
    for point in curve {
        curve_peak = curve_peak.max(point.equity);
        if curve_peak > 0.0 {
            curve_worst = curve_worst.max((curve_peak - point.equity) / curve_peak);
        }
    }

    // Tolerant by a whole percentage point of drawdown: the two are measured
    // against denominators that drift apart as the account grows, and this is
    // a sanity bound rather than a reconciliation to the cent.
    (curve_worst + 0.01 < worst_realised).then(|| {
        Discrepancy::new(
            "the curve's drawdown is at least the realised one",
            worst_realised,
            curve_worst,
            format!(
                "closed trades ran down {:.1}% from their peak while the curve's worst fall \
                 was {:.1}%",
                worst_realised * 100.0,
                curve_worst * 100.0,
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CostModel, DatasetRef, DateRange, Direction, EquityPoint, ExitReason, ExperimentId,
                HypothesisId, StrategySpec};

    fn at(day: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN)
    }

    fn experiment(costs: CostModel) -> Experiment {
        Experiment {
            id: ExperimentId("e".to_owned()),
            hypothesis: HypothesisId("h".to_owned()),
            instrument: "AAPL.NASDAQ".to_owned(),
            alongside: Vec::new(),
            underlying: None,
            window: DateRange::new(at(1).date(), at(9).date()).expect("ordered"),
            interval: arvo_data::BarInterval::DAILY,
            dataset: DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
            strategy: StrategySpec {
                name: "sma_cross".to_owned(),
                params: std::collections::BTreeMap::new(),
            },
            costs,
            risk: crate::RiskModel::default(),
            starting_cash: 1_000.0,
            seed: 7,
        }
    }

    /// A closed round trip whose P&L is consistent with its own prices.
    fn trade(entry: f64, exit: f64, quantity: f64, commission: f64) -> Trade {
        Trade {
            instrument: "AAPL.NASDAQ".to_owned(),
            opened: at(1),
            closed: Some(at(3)),
            direction: Direction::Long,
            quantity,
            entry,
            exit: Some(exit),
            pnl: (exit - entry) * quantity - commission,
            commission,
            exit_reason: ExitReason::Signal,
        }
    }

    fn result(ledger: Vec<Trade>, curve: &[f64]) -> SimulationResult {
        SimulationResult {
            experiment: ExperimentId("e".to_owned()),
            engine: "test 1".to_owned(),
            trades: u32::try_from(ledger.len()).expect("small"),
            equity_curve: curve
                .iter()
                .enumerate()
                .map(|(index, equity)| EquityPoint {
                    at: at(u32::try_from(index).expect("small") + 1),
                    equity: *equity,
                })
                .collect(),
            ledger,
            refused: crate::Refused::default(),
        }
    }

    fn names(found: &[Discrepancy]) -> Vec<&str> {
        found.iter().map(|item| item.invariant).collect()
    }

    #[test]
    fn a_consistent_run_reports_nothing() {
        // The case that must stay silent. A check that fires on a correct run
        // stops being read, and this codebase has already done that once.
        let ledger = vec![trade(10.0, 11.0, 10.0, 0.0)];
        let found = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(ledger, &[1_000.0, 1_010.0]),
        );
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_curve_that_disagrees_with_the_ledger_is_caught() {
        // The bug that shipped: totals plausible, curve derived from something
        // that was not the account's equity.
        let ledger = vec![trade(10.0, 11.0, 10.0, 0.0)];
        let found = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(ledger, &[1_000.0, 1_980.0]),
        );
        assert!(
            names(&found).contains(&"the curve ends where the ledger says it should"),
            "{found:#?}"
        );
    }

    #[test]
    fn a_trade_whose_profit_contradicts_its_prices_is_caught() {
        // The win rate comes from P&L and the charts come from prices. If
        // these two disagree, one half of the report is describing a different
        // run from the other half.
        let mut wrong = trade(10.0, 11.0, 10.0, 0.0);
        wrong.pnl = 500.0;
        let found = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(vec![wrong], &[1_000.0, 1_500.0]),
        );
        assert!(
            names(&found).contains(&"a trade's profit is what its own prices say"),
            "{found:#?}"
        );
    }

    #[test]
    fn fees_are_reconstructed_from_the_cost_model_including_the_one_sided_ones() {
        // Per-unit and sell-notional charges fall on the closing fill only.
        // Applying them to both sides would double them and report every
        // correct run as wrong, which is the failure mode this whole module
        // has to avoid.
        let costs = CostModel {
            commission_bps: 10.0,
            slippage_bps: 0.0,
            per_fill: 0.50,
            per_unit_sold: 0.01,
            sell_notional_bps: 2.0,
            option_spread: None,
        };
        // 100 in at 10, out at 11: notional 1000 + 1100.
        let expected = (1_000.0 + 1_100.0) * 10.0 / 10_000.0  // commission
            + 0.50 * 2.0                                       // two fills
            + 0.01 * 100.0                                     // per unit sold
            + 1_100.0 * 2.0 / 10_000.0; // sell notional
        let ledger = vec![trade(10.0, 11.0, 100.0, expected)];
        let moved = ledger[0].pnl;

        let found = reconcile(
            &experiment(costs),
            &result(ledger, &[1_000.0, 1_000.0 + moved]),
        );
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn fees_that_do_not_match_the_cost_model_are_caught() {
        // A run that did not pay what the experiment says it paid is a run
        // whose stored record cannot be reproduced, and nothing else would
        // notice: the cost model is pinned but never checked against.
        let ledger = vec![trade(10.0, 11.0, 100.0, 50.0)];
        let moved = ledger[0].pnl;
        let found = reconcile(
            &experiment(CostModel::proportional(1.0, 0.0)),
            &result(ledger, &[1_000.0, 1_000.0 + moved]),
        );
        assert!(
            names(&found).contains(&"fees charged match the cost model"),
            "{found:#?}"
        );
    }

    #[test]
    fn slippage_is_not_counted_as_a_fee() {
        // It is charged inside the fill prices, so it is already in the entry
        // and the exit. Counting it here would report every run with slippage
        // as having underpaid its fees by the spread.
        let ledger = vec![trade(10.0, 11.0, 100.0, 0.0)];
        let moved = ledger[0].pnl;
        let found = reconcile(
            // 50bps of slippage, no commission at all.
            &experiment(CostModel::proportional(0.0, 50.0)),
            &result(ledger, &[1_000.0, 1_000.0 + moved]),
        );
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_curve_gentler_than_the_trades_it_came_from_is_caught() {
        // The curve can be *worse* than realised P&L — it marks open positions
        // to market, so it sees falls the closed trades never recorded. It
        // cannot be better.
        let ledger = vec![
            trade(10.0, 11.0, 100.0, 0.0),  // +100
            trade(11.0, 7.0, 100.0, 0.0),   // -400
        ];
        // A curve that rises the whole way while the trades lost 400 from
        // their peak.
        let found = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(ledger, &[1_000.0, 1_100.0, 1_000.0 - 300.0 + 300.0]),
        );
        assert!(
            names(&found).contains(&"the curve's drawdown is at least the realised one"),
            "{found:#?}"
        );
    }

    #[test]
    fn a_curve_deeper_than_the_realised_losses_is_not_a_discrepancy() {
        // A position that halved and recovered never appears in realised P&L,
        // so the curve legitimately shows a fall the ledger does not.
        let ledger = vec![trade(10.0, 11.0, 10.0, 0.0)];
        let found = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(ledger, &[1_000.0, 500.0, 1_010.0]),
        );
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn a_run_still_holding_is_checked_at_its_opening_rather_than_not_at_all() {
        // The mark depends on a price this module does not have, so the
        // realised comparison cannot be made — but the opening balance is
        // still checkable, and it is where a units mix-up shows up first.
        let mut open = trade(10.0, 11.0, 10.0, 0.0);
        open.closed = None;
        open.exit = None;
        open.pnl = 0.0;
        open.exit_reason = ExitReason::StillOpen;

        let found = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(vec![open.clone()], &[999.0, 1_100.0]),
        );
        assert!(
            names(&found).contains(&"the curve opens at the starting balance"),
            "{found:#?}"
        );

        let clean = reconcile(
            &experiment(CostModel::proportional(0.0, 0.0)),
            &result(vec![open], &[1_000.0, 1_100.0]),
        );
        assert!(clean.is_empty(), "{clean:#?}");
    }

    #[test]
    fn an_empty_run_reconciles_rather_than_panicking() {
        let found = reconcile(
            &experiment(CostModel::proportional(1.0, 0.0)),
            &result(Vec::new(), &[1_000.0, 1_000.0]),
        );
        assert!(found.is_empty(), "{found:#?}");
    }
}
