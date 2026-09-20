//! What a cash account must hold against the options it is short (#84).
//!
//! A cash account cannot borrow, so every loss an open position could deliver
//! at expiry has to be sitting in the account already. Nautilus's cash account
//! does not know that — it accepted a naked short call without complaint — so
//! the rule lives here, in the gate every entry passes through.
//!
//! # The most a position can lose, not a broker's formula
//!
//! Brokers publish per-strategy tables: a short put reserves its strike, a put
//! spread its width. Both are special cases of one question — over every price
//! the underlying could close at on the expiration date, what is the worst the
//! contracts on it are worth together? A book of option payoffs is piecewise
//! linear in that price with corners only at strikes, so the worst case is at a
//! strike, at zero, or off towards infinity; checking those is exact, and it
//! handles a spread, a condor or a ladder without a table.
//!
//! Premium received is not netted against it. It is already cash in the
//! account, and counting it twice is how a cash account ends up short of cash.
//!
//! Positions on different expirations are reserved separately and summed, even
//! where a calendar spread would offset: the near leg settles first, and the
//! cash has to be there on that day.

use std::collections::BTreeMap;

use arvo_data::option::{OptionContract, Right};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// A short that no cash can secure: more calls sold than bought on one
/// expiration, which loses without limit as the underlying rises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uncovered {
    pub underlying: String,
    pub expiration: NaiveDate,
}

/// Cash reserved against a set of positions, each an instrument and a signed
/// quantity in shares of the deliverable (a contract is 100). Anything that is
/// not an option contract reserves nothing here.
///
/// # Errors
///
/// [`Uncovered`] if any expiration is net short calls.
pub fn reserved<'a>(positions: impl IntoIterator<Item = (&'a str, f64)>) -> Result<f64, Uncovered> {
    let mut groups: BTreeMap<(String, NaiveDate), Vec<(OptionContract, f64)>> = BTreeMap::new();
    for (instrument, quantity) in positions {
        if let Some(contract) = OptionContract::parse(instrument) {
            groups
                .entry((contract.underlying.clone(), contract.expiration))
                .or_default()
                .push((contract, quantity));
        }
    }

    let mut total = 0.0;
    for ((underlying, expiration), legs) in groups {
        let net_calls: f64 = legs
            .iter()
            .filter(|(contract, _)| contract.right == Right::Call)
            .map(|(_, quantity)| quantity)
            .sum();
        // Tolerance for float sums of whole lots.
        if net_calls < -1e-9 {
            return Err(Uncovered {
                underlying,
                expiration,
            });
        }
        let value_at = |spot: f64| -> f64 {
            legs.iter()
                .map(|(contract, quantity)| quantity * contract.intrinsic(spot))
                .sum()
        };
        let worst = std::iter::once(0.0)
            .chain(legs.iter().map(|(contract, _)| contract.strike))
            .map(value_at)
            .fold(0.0_f64, f64::min);
        total += -worst;
    }
    Ok(total)
}

/// The largest number of whole contracts of `proposed` that can be sold to
/// open, alongside `held`, without the cash reserved rising by more than
/// `available`. Capped at `wanted` contracts.
///
/// Checked a contract at a time because reserve is not linear in size once
/// other legs are involved: the first short put against a long one costs the
/// spread's width, and the second, with nothing left to pair with, costs its
/// strike.
///
/// # Errors
///
/// [`Uncovered`] if even one contract would leave an expiration net short calls.
pub fn sellable<'a>(
    held: &[(&'a str, f64)],
    proposed: &'a str,
    available: f64,
    wanted: u32,
) -> Result<u32, Uncovered> {
    let before = reserved(held.iter().copied())?;
    let lot = arvo_data::Instrument::of(proposed).lot;
    let mut contracts = 0;
    while contracts < wanted {
        let next = f64::from(contracts + 1) * lot;
        let mut after: Vec<(&str, f64)> = held.to_vec();
        after.push((proposed, -next));
        if reserved(after)? - before > available + 1e-9 {
            break;
        }
        contracts += 1;
    }
    Ok(contracts)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT_PUT: &str = "SPY250912P00640000.AOPT";
    const LONG_PUT: &str = "SPY250912P00630000.AOPT";
    const SHORT_CALL: &str = "SPY250912C00680000.AOPT";
    const LONG_CALL: &str = "SPY250912C00690000.AOPT";

    #[test]
    fn a_short_put_reserves_its_strike() {
        assert_eq!(reserved([(SHORT_PUT, -100.0)]), Ok(64_000.0));
    }

    #[test]
    fn a_put_spread_reserves_its_width() {
        assert_eq!(
            reserved([(SHORT_PUT, -100.0), (LONG_PUT, 100.0)]),
            Ok(1_000.0)
        );
    }

    #[test]
    fn a_long_option_reserves_nothing_and_a_stock_is_not_counted() {
        assert_eq!(reserved([(LONG_PUT, 100.0), ("SPY.AIEX", -500.0)]), Ok(0.0));
    }

    #[test]
    fn a_naked_call_cannot_be_secured_and_a_call_spread_can() {
        assert!(reserved([(SHORT_CALL, -100.0)]).is_err());
        assert_eq!(
            reserved([(SHORT_CALL, -100.0), (LONG_CALL, 100.0)]),
            Ok(1_000.0)
        );
    }

    #[test]
    fn an_iron_condor_reserves_its_wider_side_once() {
        // Both spreads $10 wide: only one side can finish in the money.
        let condor = [
            (SHORT_PUT, -100.0),
            (LONG_PUT, 100.0),
            (SHORT_CALL, -100.0),
            (LONG_CALL, 100.0),
        ];
        assert_eq!(reserved(condor), Ok(1_000.0));
    }

    #[test]
    fn different_expirations_are_reserved_separately() {
        let calendar = [(SHORT_PUT, -100.0), ("SPY251017P00630000.AOPT", 100.0)];
        assert_eq!(
            reserved(calendar),
            Ok(64_000.0),
            "the near leg settles first"
        );
    }

    #[test]
    fn a_second_short_against_one_long_costs_its_strike() {
        // $70k: one spread ($1k) fits, and a second short put with nothing to
        // pair with would need its whole $64k on top.
        let held = [(LONG_PUT, 100.0)];
        assert_eq!(
            sellable(&held, SHORT_PUT, 70_000.0, 5),
            Ok(2),
            "1k then 64k: 65k fits in 70k"
        );
        assert_eq!(sellable(&held, SHORT_PUT, 60_000.0, 5), Ok(1));
        assert_eq!(
            sellable(&[], SHORT_PUT, 60_000.0, 5),
            Ok(0),
            "a naked put needs its strike"
        );
        assert!(sellable(&[], SHORT_CALL, 1e12, 1).is_err());
    }
}
