//! Every rule the workbench offers must trade under the risk model it ships.
//!
//! # Why this is not a unit test
//!
//! It runs real backtests, and a backtest installs Nautilus's process-wide
//! logger. `arvo-runtime` also carries a guard test asserting that
//! `init_tracing` leaves the `log` global free, and that guard proves its
//! point by *taking* the global. Whichever runs first wins. See
//! `chart_alignment.rs`, which is here for the same reason: a separate
//! process is the fix, rather than weakening either check.
//!
//! # What it proves
//!
//! The engine's own tests build their experiments from `RiskModel::default`,
//! which configures no stop. Everything a user runs goes through the runtime's
//! template, which configures a 2xATR stop and sizes against it. Those are
//! different enough to decide whether a rule trades at all: `Position::plan`
//! refuses to size when a stop is asked for and no ATR is supplied, so a rule
//! that forgets to feed its indicator buys nothing — forever, silently, and
//! reports it as a family of failed configurations rather than as one unfed
//! argument. The cross-sectional rule did exactly that, through a full test
//! suite that never once ran it this way.
//!
//! So the property is deliberately not about returns. It is that a rule on the
//! menu, given data its signal fires on, under the risk model it will actually
//! be run with, does something. Any rule added to the menu is held to it
//! without anyone remembering to.
//!
//! The menu is read through `list_strategies` — the same call the UI makes —
//! so "every rule offered" means what a user can actually pick, not a second
//! list that could drift from it.

use arvo_research::{DateRange, SimulationProvider as _};
use arvo_service::research::{list_strategies, study_for, StrategyPlan};

/// Rises overall, oscillates enough to cross, break out and pull back.
///
/// One series rather than one per rule: a fixture tuned per strategy stops
/// being evidence that the strategies share a world.
///
/// Each bar's range brackets its own move rather than being a fixed percentage
/// of the close. That distinction decides whether this fixture can test
/// anything — a constant wide range makes the ATR larger than any day-over-day
/// move, and every volatility-scaled threshold in the library becomes
/// unreachable by construction. The first draft of this did that, and
/// `volatility_breakout` "failed" against a fixture that could never have
/// triggered it. A fixture that silently cannot fire a rule tests that the
/// engine runs, not that the rule works.
fn daily(days: usize, phase: f64) -> Vec<arvo_data::Bar> {
    let start = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).expect("a real date");
    let level =
        |t: f64| 100.0 * (0.004f64).mul_add(t, 1.0) * (0.06f64).mul_add((t / 2.0 + phase).sin(), 1.0);
    (0..days)
        .map(|i| {
            let t = i as f64;
            let (open, close) = (level((t - 1.0).max(0.0)), level(t));
            arvo_data::Bar {
                at: start
                    .checked_add_days(chrono::Days::new(i as u64))
                    .expect("in range")
                    .and_time(chrono::NaiveTime::MIN),
                open,
                high: open.max(close) * 1.004,
                low: open.min(close) * 0.996,
                close,
                volume: 1_000_000.0,
            }
        })
        .collect()
}

/// Five-minute bars over several sessions, each with a quiet open, a break
/// upward, a fall back through the session's average and a recovery — the
/// shape the session-anchored rules are defined in terms of.
fn intraday(sessions: usize, per_session: usize) -> Vec<arvo_data::Bar> {
    let start = chrono::NaiveDate::from_ymd_opt(2020, 1, 6).expect("a real date");
    let level = |session: usize, slot: f64| {
        let base = 100.0 + session as f64;
        if slot < 4.0 {
            base
        } else {
            base * (0.03f64).mul_add(((slot - 4.0) / 6.0).sin(), 1.0)
        }
    };
    let mut bars = Vec::with_capacity(sessions * per_session);
    for session in 0..sessions {
        let day = start
            .checked_add_days(chrono::Days::new(session as u64))
            .expect("in range");
        for slot in 0..per_session {
            let t = slot as f64;
            let close = level(session, t);
            let open = level(session, (t - 1.0).max(0.0));
            bars.push(arvo_data::Bar {
                at: day.and_hms_opt(9, 30, 0).expect("market open exists")
                    + chrono::Duration::minutes(i64::try_from(5 * slot).expect("small")),
                open,
                high: open.max(close) * 1.001,
                low: open.min(close) * 0.999,
                close,
                volume: 100_000.0,
            });
        }
    }
    bars
}

#[test]
fn every_shipped_rule_trades_under_the_shipped_risk_model() {
    // Wide enough to cover both fixtures. `bars` filters by date, so one
    // window serves whichever resolution the plan turns out to want and the
    // test need not know the resolution before it can ask.
    let window = DateRange::new(
        chrono::NaiveDate::from_ymd_opt(2019, 12, 1).expect("a real date"),
        chrono::NaiveDate::from_ymd_opt(2022, 12, 31).expect("a real date"),
    )
    .expect("ordered");

    for offered in list_strategies(std::path::Path::new("")).expect("listing the menu never fails") {
        let plan = StrategyPlan::find(&offered.name).expect("the menu names real plans");
        // An option plan trades a chain this stock fixture does not have, and
        // ships no stop by design: the engine refuses one for a chain rule.
        // `shipped_option_templates.rs` holds them to the same property on a
        // chain.
        if plan.trades_options() {
            continue;
        }
        let family = study_for("A.SIM", plan, window, "fixture");

        assert!(
            family.template.risk.stop_atr_multiple.is_some(),
            "this test is worthless if the shipped template stops configuring a stop"
        );

        let series = if family.template.interval == arvo_data::BarInterval::DAILY {
            daily(400, 0.0)
        } else {
            intraday(12, 78)
        };
        // At the resolution the plan asks for. `with_instrument` means daily,
        // and an intraday plan handed daily bars gets `NoData` rather than the
        // wrong series, which is the library being right.
        let mut library =
            arvo_data::InMemoryBars::new().with_interval("A.SIM", family.template.interval, series);

        let mut experiment = family.template.clone();
        // A ranking rule needs a field to rank. The others ignore the rest.
        if plan.ranks_a_set() {
            for (name, phase) in [("B.SIM", 0.7), ("C.SIM", 1.4), ("D.SIM", 2.1)] {
                library = library.with_interval(name, family.template.interval, daily(400, phase));
                experiment.alongside.push(name.to_owned());
            }
        }

        // The cheapest corner of the grid, so warm-up never eats the fixture.
        // `combinations` is ordered, so this is the same corner every run.
        let corner = family
            .grid
            .combinations()
            .into_iter()
            .next()
            .expect("every plan searches something");
        experiment.strategy.params.extend(corner.clone());

        let result = arvo_nautilus::NautilusSimulation::new(library)
            .run(&experiment)
            .unwrap_or_else(|error| {
                panic!(
                    "{} should run under the shipped risk model: {error:?}",
                    offered.name
                )
            });

        assert!(
            !result.ledger.is_empty(),
            "{} traded nothing under the shipped risk model at {corner:?} — check it feeds an \
             ATR to `Position::plan`",
            offered.name
        );
    }
}
