//! What an option contract is (#13).
//!
//! # A contract is an instrument named by its OCC symbol
//!
//! `SPY260914C00760000.AOPT` names one contract: SPY, expiring 14 September
//! 2026, a call, struck at 760. Everything a [`Bar`](crate::Bar) needs to know
//! about the contract it belongs to is in that name, so a contract's bars are an
//! ordinary series under an ordinary instrument name — read, fingerprinted and
//! covered by the same [`BarProvider`](crate::BarProvider) as a stock's. What is
//! new is only what the name *means*, and that is this module.
//!
//! A chain is not a type here. It is the set of contracts on an underlying that
//! exist on a given day, which changes daily as contracts list and expire; it
//! is what ingest (#15) discovers, not something a contract has to carry.
//!
//! # What a contract promises
//!
//! 100 shares of the underlying per contract, at the strike, by the close on the
//! expiration date. SPY options are American — exercisable any day — and settle
//! in shares, which is why expiry and assignment are a model of their own (#84)
//! rather than a price going to zero.
//!
//! The multiplier is 100 for every contract read here. An adjusted contract
//! after a split or special dividend delivers something else, and OCC gives it
//! a root with a digit (`SPY1`) — which [`OptionContract::parse`] refuses, so
//! such a contract cannot be read as a standard one.

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

/// Shares of the underlying one standard contract delivers.
pub const MULTIPLIER: f64 = 100.0;

/// The right a contract grants its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Right {
    /// To buy the underlying at the strike.
    Call,
    /// To sell the underlying at the strike.
    Put,
}

/// One listed option contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionContract {
    pub underlying: String,
    pub expiration: NaiveDate,
    pub right: Right,
    /// Per share, in the underlying's currency.
    pub strike: f64,
}

impl OptionContract {
    /// Reads an OCC symbol — root, `YYMMDD`, `C` or `P`, strike × 1000 in eight
    /// digits — with or without a `.VENUE` suffix.
    ///
    /// `None` for anything else, which is how a stock's name is told apart
    /// from a contract's: no ticker ends in fifteen characters of that shape.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let symbol = name.split('.').next()?;
        // Names arrive from UI fields; the slicing below is by byte.
        if !symbol.is_ascii() {
            return None;
        }
        let split = symbol.len().checked_sub(15)?;
        let (root, tail) = (symbol.get(..split)?, symbol.get(split..)?);
        if root.is_empty() || !root.chars().all(|c| c.is_ascii_uppercase()) {
            return None;
        }
        if !tail[..6].chars().all(|c| c.is_ascii_digit())
            || !tail[7..].chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let expiration = NaiveDate::parse_from_str(&format!("20{}", &tail[..6]), "%Y%m%d").ok()?;
        let right = match &tail[6..7] {
            "C" => Right::Call,
            "P" => Right::Put,
            _ => return None,
        };
        let strike = f64::from(tail[7..].parse::<u32>().ok()?) / 1000.0;
        Some(Self {
            underlying: root.to_owned(),
            expiration,
            right,
            strike,
        })
    }

    /// The OCC symbol, without a venue. The inverse of [`Self::parse`].
    #[must_use]
    pub fn symbol(&self) -> String {
        let right = match self.right {
            Right::Call => 'C',
            Right::Put => 'P',
        };
        // Rounded, not truncated: 5822.5 * 1000 is exact, but a strike read
        // from a float elsewhere may arrive a hair under its true value.
        let strike = (self.strike * 1000.0).round() as u64;
        format!(
            "{}{}{right}{strike:08}",
            self.underlying,
            self.expiration.format("%y%m%d")
        )
    }

    /// The instant trading in the contract ends, in UTC: the regular close on
    /// the expiration date.
    ///
    /// ponytail: SPY is PM-settled and stops at 16:00 New York. Index options
    /// that settle on the morning's open (SPX monthlies) end a day earlier in
    /// effect; model that when one is traded.
    #[must_use]
    pub fn expires_at(&self) -> NaiveDateTime {
        crate::session::regular_close(self.expiration)
    }

    /// What exercising one share's worth would be worth with the underlying at
    /// `spot`. Never negative: a holder does not exercise at a loss.
    #[must_use]
    pub fn intrinsic(&self, spot: f64) -> f64 {
        match self.right {
            Right::Call => (spot - self.strike).max(0.0),
            Right::Put => (self.strike - spot).max(0.0),
        }
    }

    /// Whole calendar days from `on` to expiration; zero on the day itself.
    #[must_use]
    pub fn days_to_expiry(&self, on: NaiveDate) -> i64 {
        (self.expiration - on).num_days()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid")
    }

    #[test]
    fn an_occ_symbol_reads_with_or_without_its_venue() {
        let contract = OptionContract::parse("SPY260914C00760000.AOPT").expect("a contract");
        assert_eq!(
            contract,
            OptionContract {
                underlying: "SPY".to_owned(),
                expiration: date(2026, 9, 14),
                right: Right::Call,
                strike: 760.0,
            }
        );
        assert_eq!(OptionContract::parse("SPY260914C00760000"), Some(contract));
    }

    #[test]
    fn a_longer_root_and_a_fractional_strike_round_trip() {
        for symbol in [
            "SPXW261016P05822500",
            "SPY250912P00640000",
            "QQQ270115C00000500",
        ] {
            let contract = OptionContract::parse(symbol).expect(symbol);
            assert_eq!(contract.symbol(), symbol);
        }
    }

    #[test]
    fn a_stock_is_not_a_contract() {
        for name in [
            "AAPL.RH",
            "SPY.AIEX",
            "BRK-B.YF",
            "",
            "260914C00760000",
            "SPY260914X00760000",
            "SPY26O914C00760000",
            "spy260914C00760000",
            "SPY1260914C00760000",
            "SPY26091éC00760000",
        ] {
            assert_eq!(OptionContract::parse(name), None, "{name:?}");
        }
    }

    #[test]
    fn a_contract_stops_trading_at_the_close_in_new_york() {
        let summer = OptionContract::parse("SPY260914C00760000").expect("valid");
        assert_eq!(
            summer.expires_at(),
            date(2026, 9, 14).and_hms_opt(20, 0, 0).expect("valid")
        );
        let winter = OptionContract::parse("SPY270115C00760000").expect("valid");
        assert_eq!(
            winter.expires_at(),
            date(2027, 1, 15).and_hms_opt(21, 0, 0).expect("valid")
        );
    }

    #[test]
    fn intrinsic_value_is_never_negative() {
        let call = OptionContract::parse("SPY260914C00760000").expect("valid");
        let put = OptionContract::parse("SPY260914P00760000").expect("valid");
        assert!((call.intrinsic(765.5) - 5.5).abs() < 1e-9);
        assert!(call.intrinsic(750.0).abs() < 1e-12);
        assert!((put.intrinsic(750.0) - 10.0).abs() < 1e-9);
        assert!(put.intrinsic(765.5).abs() < 1e-12);
    }

    #[test]
    fn zero_days_to_expiry_is_the_expiration_date_itself() {
        let contract = OptionContract::parse("SPY260914C00760000").expect("valid");
        assert_eq!(contract.days_to_expiry(date(2026, 9, 14)), 0);
        assert_eq!(contract.days_to_expiry(date(2026, 8, 14)), 31);
    }
}
