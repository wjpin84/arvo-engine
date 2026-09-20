//! Every option rule the workbench offers must run, reconcile and trade under
//! the template it ships, on a chain.
//!
//! # Why this exists
//!
//! `shipped_risk.rs` holds the stock rules to this and skips the option ones:
//! they trade a chain its stock fixture does not have, and ship no stop. Their
//! pieces were each tested — the template's settings in `research::study`, the
//! rules on a chain in `arvo-nautilus` — but nothing ran the shipped template's
//! grid through the engine. A grid value the engine refuses, or a template
//! setting that starves a rule of every trade, would surface only as a study
//! that fails or reports nothing after a user has waited for it.
//!
//! # What it proves
//!
//! For each option plan on the menu, every configuration in its search runs to
//! the end of the window and its ledger reconciles with its curve; and across
//! the search, the rule trades. Not every configuration has to trade: a
//! five-delta short put can fairly find no credit worth selling on a synthetic
//! chain, and a test that demanded one would be testing the fixture.
//!
//! A separate process for the reason `shipped_risk.rs` gives: a backtest
//! installs Nautilus's process-wide logger.

use arvo_data::option::{OptionContract, Right};
use arvo_data::{Bar, BarInterval, InMemoryBars};
use arvo_research::greeks::{greeks, years_to_expiry, Market};
use arvo_research::{DateRange, SimulationProvider as _};
use arvo_service::research::{list_strategies, study_for, StrategyPlan};
use chrono::{Duration, NaiveDate, NaiveTime};

const UNDERLYING: &str = "SPY.AIEX";

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("a real date")
}

fn market(spot: f64) -> Market {
    Market {
        spot,
        rate: 0.04,
        dividend_yield: 0.013,
    }
}

/// A price as a chain quotes it: to the cent, and never nothing.
fn quoted(price: f64) -> f64 {
    ((price * 100.0).round() / 100.0).max(0.01)
}

fn flat(at: chrono::NaiveDateTime, price: f64) -> Bar {
    Bar {
        at,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 100.0,
    }
}

/// Daily SPY drifting up through a cycle, and its puts on four monthly
/// expirations, every strike from 70 to 110 priced each day at 20% volatility
/// from that day's close. The engine's own put-spread fixture.
fn monthly_chain() -> (InMemoryBars, DateRange) {
    let mut day = date(2024, 1, 1);
    let mut underlying = Vec::new();
    for index in 0..200 {
        let phase = (index % 40) as f64;
        let cycle = if phase < 20.0 { phase } else { 40.0 - phase };
        let close = 100.0 + index as f64 * 0.05 + cycle * 0.5;
        underlying.push(Bar {
            at: day.and_time(NaiveTime::MIN),
            open: close,
            high: close + 0.5,
            low: close - 0.5,
            close,
            volume: 10_000.0,
        });
        day = day.succ_opt().expect("in range");
    }
    let window = DateRange::new(date(2024, 1, 1), underlying[199].at.date()).expect("ordered");

    let mut library = InMemoryBars::new().with_instrument(UNDERLYING, underlying.clone());
    for expiration in [date(2024, 3, 15), date(2024, 4, 19), date(2024, 5, 17), date(2024, 6, 21)] {
        for strike in 70..=110 {
            let contract = OptionContract {
                underlying: "SPY".to_owned(),
                expiration,
                right: Right::Put,
                strike: f64::from(strike),
            };
            let bars = underlying
                .iter()
                .filter(|bar| bar.at.date() <= expiration)
                .map(|bar| {
                    let years = years_to_expiry(&contract, arvo_data::session::regular_close(bar.at.date()));
                    flat(bar.at, quoted(greeks(&contract, market(bar.close), years, 0.20).price))
                })
                .collect();
            library = library.with_instrument(&format!("{}.AOPT", contract.symbol()), bars);
        }
    }
    (library, window)
}

/// Ten sessions of five-minute SPY that each break a quiet opening half hour,
/// alternately up and down, with a same-day chain of calls and puts on every
/// strike from 90 to 110 priced at every bar. At 150% volatility, because the
/// fixture moves ten points a session on a hundred.
fn same_day_chain() -> (InMemoryBars, DateRange) {
    let five = BarInterval::new(5, arvo_data::IntervalUnit::Minute);
    let first = date(2024, 1, 2);
    let mut underlying = Vec::new();
    for session in 0..10 {
        let open = (first + Duration::days(session)).and_hms_opt(14, 30, 0).expect("valid");
        let sign = if session % 2 == 0 { 1.0 } else { -1.0 };
        for index in 0..78 {
            let phase = f64::from(index) / 78.0;
            let close = 100.0
                + sign
                    * if phase < 0.2 {
                        0.0
                    } else if phase < 0.6 {
                        (phase - 0.2) * 25.0
                    } else {
                        10.0 - (phase - 0.6) * 15.0
                    };
            underlying.push(Bar {
                at: open + Duration::minutes(5 * i64::from(index)),
                open: close,
                high: close + 0.2,
                low: close - 0.2,
                close,
                volume: 10_000.0,
            });
        }
    }
    let window = DateRange::new(first, first + Duration::days(9)).expect("ordered");

    let mut library = InMemoryBars::new().with_interval(UNDERLYING, five, underlying.clone());
    for session in 0..10 {
        let day = first + Duration::days(session);
        for strike in 90..=110 {
            for right in [Right::Put, Right::Call] {
                let contract = OptionContract {
                    underlying: "SPY".to_owned(),
                    expiration: day,
                    right,
                    strike: f64::from(strike),
                };
                let bars = underlying
                    .iter()
                    .filter(|bar| bar.at.date() == day)
                    .map(|bar| {
                        let years = years_to_expiry(&contract, bar.at + Duration::minutes(5));
                        flat(bar.at, quoted(greeks(&contract, market(bar.close), years, 1.50).price))
                    })
                    .collect();
                library = library.with_interval(&format!("{}.AOPT", contract.symbol()), five, bars);
            }
        }
    }
    (library, window)
}

#[test]
fn every_shipped_option_rule_runs_reconciles_and_trades_on_a_chain() {
    let options: Vec<(String, &StrategyPlan)> = list_strategies(std::path::Path::new(""))
        .expect("listing the menu never fails")
        .into_iter()
        .map(|offered| {
            let plan = StrategyPlan::find(&offered.name).expect("the menu names real plans");
            (offered.name, plan)
        })
        .filter(|(_, plan)| plan.trades_options())
        .collect();
    assert!(!options.is_empty(), "this test is worthless if the menu offers no option rule");

    for (name, plan) in options {
        let (library, window) = if plan.interval().is_intraday() {
            same_day_chain()
        } else {
            monthly_chain()
        };
        let family = study_for(UNDERLYING, plan, window, "chain:fixture");
        let engine = arvo_nautilus::NautilusSimulation::new(library);

        let mut traded = 0;
        for configuration in family.grid.combinations() {
            let mut experiment = family.template.clone();
            experiment.strategy.params.extend(configuration.clone());

            let result = engine.run(&experiment).unwrap_or_else(|error| {
                panic!("{} at {configuration:?} should run under its shipped template: {error:?}", name)
            });
            assert_eq!(
                arvo_research::reconcile::reconcile(&experiment, &result),
                Vec::new(),
                "{} at {configuration:?}: the ledger and the curve disagree",
                name
            );
            traded += result.ledger.len();
        }
        assert!(
            traded > 0,
            "{} traded nothing at any configuration of its shipped search on a chain its signal fires on",
            name
        );
    }
}
