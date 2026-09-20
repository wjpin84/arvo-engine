//! What an option run's positions would have lost on the market's worst days
//! (#85).
//!
//! Option history reaches back to 2024. It holds the August 2024 and April 2025
//! spikes and nothing of 2008, 2020, or the February 2018 day short-volatility
//! funds were wiped out — so a rule that sells premium has never been shown
//! the kind of day that ends such rules, and its worst period is a floor. No
//! option prices exist for those days to backtest against. What does exist is
//! how far SPY fell and where the VIX closed, and a model that prices a
//! position from those.
//!
//! # The replay
//!
//! Every option position the run opened is repriced at the instant it opened,
//! as if that instant were one of [`SCENARIOS`]: the underlying moved by that
//! day's close-to-close return, and every leg's implied volatility rose to at
//! least that day's VIX close. The loss is what the position would then have
//! cost against what it was worth, and the worst position per scenario is
//! kept.
//!
//! # Why it is a floor
//!
//! - **Skew.** A crash lifts out-of-the-money put volatility well above the
//!   VIX, which is an at-the-money index. Pricing every leg at the VIX
//!   understates a short put's loss.
//! - **Instantaneous.** The whole day's move lands at once, with no time
//!   passing and no chance to exit. For a spread that caps its own loss this
//!   is close to the truth; for anything that relies on a stop it is not.
//! - **Levels, not paths.** 16 March 2020 followed three weeks of falls. A
//!   position opened into that tape was opened at different prices, which no
//!   single-day shock can reproduce.

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use arvo_data::option::OptionContract;

use crate::greeks::{greeks, implied_volatility, years_to_expiry, Market};
use crate::{Direction, Trade};

/// A historical day, as SPY and the VIX closed on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scenario {
    pub day: NaiveDate,
    pub what: &'static str,
    /// SPY's close-to-close price return that day, as a fraction.
    pub spot_move: f64,
    /// The VIX close that day, in points.
    pub vix: f64,
}

const fn day(year: i32, month: u32, date: u32) -> NaiveDate {
    match NaiveDate::from_ymd_opt(year, month, date) {
        Some(day) => day,
        None => panic!("a scenario date is not a date"),
    }
}

/// SPY's worst days, and one from each kind of stress, with the VIX close.
///
/// Read from Yahoo's daily SPY and ^VIX history, 1993-01-29 to 2026-09-14, on
/// 2026-09-14: the price returns are split-adjusted closes, not total return.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        day: day(2020, 3, 16),
        what: "covid crash, SPY's worst day",
        spot_move: -0.1094,
        vix: 82.69,
    },
    Scenario {
        day: day(2008, 10, 15),
        what: "financial crisis",
        spot_move: -0.0984,
        vix: 69.25,
    },
    Scenario {
        day: day(2011, 8, 8),
        what: "US downgrade",
        spot_move: -0.0651,
        vix: 48.00,
    },
    Scenario {
        day: day(2025, 4, 4),
        what: "tariff shock, inside the option history",
        spot_move: -0.0585,
        vix: 45.31,
    },
    Scenario {
        day: day(2022, 9, 13),
        what: "bear-market CPI day",
        spot_move: -0.0435,
        vix: 27.27,
    },
    Scenario {
        day: day(2018, 2, 5),
        what: "volatility doubled in a day and short-vol funds closed",
        spot_move: -0.0418,
        vix: 37.32,
    },
];

/// One leg as priced at its entry: signed size, price, implied volatility.
struct Leg<'a> {
    held: f64,
    price: f64,
    vol: f64,
    contract: &'a OptionContract,
    years: f64,
}

/// One scenario's worst position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shock {
    pub day: NaiveDate,
    pub what: String,
    pub spot_move: f64,
    pub vix: f64,
    /// The position that lost most, by its legs' instruments.
    pub position: String,
    pub opened: NaiveDateTime,
    /// In account currency. Positive is a loss.
    pub loss: f64,
    /// What the position took in when it opened, in account currency; zero for
    /// one that paid to open.
    pub credit: f64,
}

/// The replay's result: each scenario's worst position, worst first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stress {
    pub shocks: Vec<Shock>,
    /// Positions that could not be priced — no underlying price at the time,
    /// or a leg whose price no volatility reaches — and are not in the result.
    pub unpriced: usize,
}

impl Stress {
    /// The worst loss across every scenario.
    #[must_use]
    pub fn worst(&self) -> Option<&Shock> {
        self.shocks.first()
    }
}

/// Replays every option position in `ledger` through [`SCENARIOS`].
///
/// `spot_at` is the underlying's price at an instant — the last one known then.
/// `None` when the ledger holds no option position at all.
#[must_use]
pub fn replay(
    ledger: &[Trade],
    spot_at: impl Fn(NaiveDateTime) -> Option<f64>,
    rate: f64,
    dividend_yield: f64,
) -> Option<Stress> {
    // Legs opened together on one underlying and expiration are one position.
    let mut positions: Vec<Vec<(&Trade, OptionContract)>> = Vec::new();
    for trade in ledger {
        let Some(contract) = OptionContract::parse(&trade.instrument) else {
            continue;
        };
        match positions.iter_mut().find(|legs| {
            let (first, other) = &legs[0];
            first.opened == trade.opened
                && other.underlying == contract.underlying
                && other.expiration == contract.expiration
        }) {
            Some(legs) => legs.push((trade, contract)),
            None => positions.push(vec![(trade, contract)]),
        }
    }
    if positions.is_empty() {
        return None;
    }

    let mut stress = Stress::default();
    let mut worst: Vec<Option<Shock>> = vec![None; SCENARIOS.len()];
    for legs in &positions {
        let opened = legs[0].0.opened;
        let Some(spot) = spot_at(opened) else {
            stress.unpriced += 1;
            continue;
        };
        let market = Market {
            spot,
            rate,
            dividend_yield,
        };
        let priced: Option<Vec<Leg<'_>>> = legs
            .iter()
            .map(|(trade, contract)| {
                let years = years_to_expiry(contract, opened);
                let vol = implied_volatility(contract, trade.entry, market, years)?;
                let sign = match trade.direction {
                    Direction::Long => 1.0,
                    Direction::Short => -1.0,
                };
                Some(Leg {
                    held: sign * trade.quantity,
                    price: trade.entry,
                    vol,
                    contract,
                    years,
                })
            })
            .collect();
        let Some(priced) = priced else {
            stress.unpriced += 1;
            continue;
        };
        let now: f64 = priced.iter().map(|leg| leg.held * leg.price).sum();
        let name = legs
            .iter()
            .map(|(t, _)| t.instrument.as_str())
            .collect::<Vec<_>>()
            .join("+");

        for (index, scenario) in SCENARIOS.iter().enumerate() {
            let shocked_market = Market {
                spot: spot * (1.0 + scenario.spot_move),
                ..market
            };
            let shocked: f64 = priced
                .iter()
                .map(|leg| {
                    let vol = leg.vol.max(scenario.vix / 100.0);
                    leg.held * greeks(leg.contract, shocked_market, leg.years, vol).price
                })
                .sum();
            let loss = now - shocked;
            if worst[index].as_ref().is_none_or(|kept| loss > kept.loss) {
                worst[index] = Some(Shock {
                    day: scenario.day,
                    what: scenario.what.to_owned(),
                    spot_move: scenario.spot_move,
                    vix: scenario.vix,
                    position: name.clone(),
                    opened,
                    loss,
                    credit: (-now).max(0.0),
                });
            }
        }
    }
    stress.shocks = worst.into_iter().flatten().collect();
    stress.shocks.sort_by(|a, b| b.loss.total_cmp(&a.loss));
    Some(stress)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32) -> NaiveDateTime {
        day(2026, 8, 3).and_hms_opt(hour, 0, 0).expect("valid")
    }

    fn leg(instrument: &str, direction: Direction, entry: f64) -> Trade {
        Trade {
            instrument: instrument.to_owned(),
            opened: at(14),
            closed: None,
            direction,
            quantity: 100.0,
            entry,
            exit: None,
            pnl: 0.0,
            commission: 0.0,
            exit_reason: crate::ExitReason::StillOpen,
            journal: None,
        }
    }

    /// Model prices at 15% volatility for puts on SPY at 600, a month out.
    fn price(strike: u32) -> f64 {
        let contract =
            OptionContract::parse(&format!("SPY260904P{:08}", strike * 1000)).expect("valid");
        let market = Market {
            spot: 600.0,
            rate: 0.04,
            dividend_yield: 0.013,
        };
        greeks(&contract, market, years_to_expiry(&contract, at(14)), 0.15).price
    }

    #[test]
    fn a_put_spread_loses_about_its_width_less_its_credit_on_a_crash() {
        let ledger = [
            leg("SPY260904P00570000.AOPT", Direction::Short, price(570)),
            leg("SPY260904P00565000.AOPT", Direction::Long, price(565)),
        ];
        let stress = replay(&ledger, |_| Some(600.0), 0.04, 0.013).expect("an option position");
        let worst = stress.worst().expect("a shock");
        assert_eq!(
            worst.day,
            day(2020, 3, 16),
            "the largest move is the worst for a put spread"
        );
        let credit = (price(570) - price(565)) * 100.0;
        assert!((worst.credit - credit).abs() < 1e-6);
        // SPY at 534, both puts in the money. With a month left at a VIX of 83
        // the spread still has time value, so it is worth well over half its
        // $5 width and never more than all of it: the loss lies between half
        // the most it can lose and the most.
        let most = 500.0 - credit;
        assert!(
            worst.loss > 0.5 * most && worst.loss <= most + 1e-6,
            "{} of {most}",
            worst.loss
        );
        assert_eq!(stress.shocks.len(), SCENARIOS.len());
        assert_eq!(stress.unpriced, 0);
    }

    #[test]
    fn a_naked_short_put_loses_many_times_its_credit() {
        let ledger = [leg("SPY260904P00570000.AOPT", Direction::Short, price(570))];
        let worst = replay(&ledger, |_| Some(600.0), 0.04, 0.013)
            .expect("a position")
            .worst()
            .cloned()
            .expect("a shock");
        assert!(
            worst.loss > 10.0 * worst.credit,
            "{} against {}",
            worst.loss,
            worst.credit
        );
    }

    #[test]
    fn a_long_put_gains_and_stock_is_not_replayed() {
        let long = [leg("SPY260904P00570000.AOPT", Direction::Long, price(570))];
        let stress = replay(&long, |_| Some(600.0), 0.04, 0.013).expect("a position");
        assert!(
            stress.shocks.iter().all(|shock| shock.loss < 0.0),
            "a crash pays a long put"
        );

        let stock = [leg("SPY.AIEX", Direction::Long, 600.0)];
        assert_eq!(replay(&stock, |_| Some(600.0), 0.04, 0.013), None);
    }

    #[test]
    fn a_position_with_no_underlying_price_is_counted_not_guessed() {
        let ledger = [leg("SPY260904P00570000.AOPT", Direction::Short, price(570))];
        let stress = replay(&ledger, |_| None, 0.04, 0.013).expect("a position");
        assert!(stress.shocks.is_empty());
        assert_eq!(stress.unpriced, 1);
    }
}
