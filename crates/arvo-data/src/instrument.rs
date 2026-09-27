//! What a rule has to know about an instrument before it can be sized or
//! gated honestly (#186).
//!
//! # One type, owned by the source
//!
//! An instrument was a string and a multiplier constant. That held while
//! everything was a share or a standard contract on shares, and it stops
//! holding the moment a contract's lot, tick, hours or margin are the
//! venue's to say (#114). So the facts live here, in one type, and the
//! [`Source`](crate::source::Source) that serves an instrument's bars is who
//! says what they are — with [`Instrument::of`] as what every source says
//! until it knows better, which is what the name alone can tell.
//!
//! # The unit convention, kept
//!
//! Quantities are units of the deliverable — shares, for a stock and for an
//! option on one — and prices are per unit, so a price times a quantity is
//! money everywhere with no multiplier to forget. A standard option contract
//! is therefore a *lot* of 100 units with a multiplier of one, not one
//! contract with a multiplier of 100. The multiplier field exists for the
//! day something is quoted in a unit it does not deliver (an index future's
//! point value); it is one for everything read today.

use serde::{Deserialize, Serialize};

use crate::option::OptionContract;

/// What kind of thing trades under the name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    Stock,
    Option(OptionContract),
}

/// When the instrument trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hours {
    /// The US regular session, 09:30–16:00 New York; see [`crate::session`].
    Regular,
    /// Around the clock, as crypto does.
    Continuous,
}

/// One instrument's trading facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instrument {
    /// The Arvo id, venue suffix included when the caller had one.
    pub id: String,
    pub kind: Kind,
    /// The smallest price increment, in the quote currency.
    pub tick: f64,
    /// The smallest tradeable quantity, in units. A quantity that is not a
    /// whole number of these is not an order; see `arvo_risk::Rejection`.
    pub lot: f64,
    /// Money per unit of price per unit of quantity. One for anything
    /// traded in units of what it delivers, which is everything today.
    pub multiplier: f64,
    pub hours: Hours,
    /// Initial margin per lot, in the quote currency, when the venue takes
    /// margin rather than the full price. `None` is full price — a stock —
    /// or collateral computed elsewhere (`arvo_risk::collateral`, for a
    /// short option). Nothing reads it yet; a future's venue will set it.
    pub margin: Option<f64>,
}

impl Instrument {
    /// What the name alone says: an OCC symbol is a standard contract, in
    /// lots of a hundred units; anything else is a stock, in single shares.
    /// Both tick at a cent and trade the regular session.
    #[must_use]
    pub fn of(id: &str) -> Self {
        match OptionContract::parse(id) {
            Some(contract) => Self {
                id: id.to_owned(),
                kind: Kind::Option(contract),
                tick: 0.01,
                lot: 100.0,
                multiplier: 1.0,
                hours: Hours::Regular,
                margin: None,
            },
            None => Self {
                id: id.to_owned(),
                kind: Kind::Stock,
                tick: 0.01,
                lot: 1.0,
                multiplier: 1.0,
                hours: Hours::Regular,
                margin: None,
            },
        }
    }

    /// Whether `quantity` is a whole number of lots. A tolerance of a
    /// millionth of a unit, so a float that arrived through arithmetic is
    /// judged on what it means.
    #[must_use]
    pub fn is_whole_lots(&self, quantity: f64) -> bool {
        let lots = quantity / self.lot;
        (lots - lots.round()).abs() < 1e-6
    }

    /// `quantity` rounded down to whole lots.
    #[must_use]
    pub fn whole_lots(&self, quantity: f64) -> f64 {
        (quantity / self.lot).floor() * self.lot
    }

    /// Decimal places a price needs, from the tick it moves in: two for a cent,
    /// four for a pip, eight for a satoshi.
    ///
    /// Derived rather than declared so there is one answer per instrument
    /// instead of one answer per module. A stock ticking at `0.01` gives two,
    /// which is what the engine assumed for everything before a coin arrived.
    #[must_use]
    pub fn price_precision(&self) -> u8 {
        decimals(self.tick)
    }

    /// Decimal places a quantity needs, from the smallest tradeable lot: zero
    /// for whole shares or a hundred-share contract, eight for a coin quoted in
    /// satoshis.
    #[must_use]
    pub fn size_precision(&self) -> u8 {
        decimals(self.lot)
    }
}

/// The most decimal places anything here is described to.
///
/// Nautilus's own `FIXED_PRECISION` is nine without its high-precision
/// feature, and a satoshi is eight, so nine is both the ceiling downstream and
/// more than any real tick asks for.
///
/// ponytail: a finer step than this is silently described at nine places.
/// Return an error instead if a venue ever quotes one.
const MAX_DECIMALS: u8 = 9;

/// Decimal places `step` needs to be written exactly.
///
/// Scaling by ten until the value is whole, rather than `-log10(step)`, because
/// `log10` of a value a binary float cannot hold exactly lands either side of
/// the integer it should be — and `0.01` resolving to three places instead of
/// two would change every price the engine has ever built.
///
/// The whole number has to be at least one, and the tolerance is relative to it.
/// An absolute tolerance would call any step smaller than itself whole at zero
/// places, so a tick of `1e-12` would be described as dollars.
fn decimals(step: f64) -> u8 {
    let mut scaled = step.abs();
    for places in 0..MAX_DECIMALS {
        let whole = scaled.round();
        if whole >= 1.0 && (scaled - whole).abs() < 1e-9 * whole {
            return places;
        }
        scaled *= 10.0;
    }
    MAX_DECIMALS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contract_is_a_lot_of_a_hundred_and_a_stock_is_a_lot_of_one() {
        let contract = Instrument::of("SPY260914C00760000.AOPT");
        assert!(matches!(contract.kind, Kind::Option(_)));
        assert!((contract.lot - 100.0).abs() < 1e-9);
        assert!((contract.multiplier - 1.0).abs() < 1e-9, "units of the deliverable, so no multiplier");
        let stock = Instrument::of("MSFT.RH");
        assert_eq!(stock.kind, Kind::Stock);
        assert!((stock.lot - 1.0).abs() < 1e-9);
        assert_eq!(stock.id, "MSFT.RH");
    }

    #[test]
    fn whole_lots_are_judged_and_rounded_in_lots() {
        let contract = Instrument::of("SPY260914C00760000");
        assert!(contract.is_whole_lots(200.0));
        assert!(contract.is_whole_lots(300.0 - 1e-9), "arithmetic noise is not half a contract");
        assert!(!contract.is_whole_lots(250.0));
        assert!((contract.whole_lots(250.0) - 200.0).abs() < 1e-9);
        assert!(Instrument::of("MSFT").is_whole_lots(7.0));
    }

    /// The equality the rest of the crypto work rests on: what an equity and an
    /// option resolve to is exactly what the two constants in
    /// `arvo-nautilus/src/convert.rs` said before they were derived.
    #[test]
    fn a_stock_and_a_contract_still_resolve_to_two_places_of_price_and_none_of_size() {
        let stock = Instrument::of("MSFT.RH");
        assert_eq!(stock.price_precision(), 2);
        assert_eq!(stock.size_precision(), 0);
        let contract = Instrument::of("SPY260914C00760000.AOPT");
        assert_eq!(contract.price_precision(), 2);
        assert_eq!(contract.size_precision(), 0, "a hundred-share lot is still whole units");
    }

    #[test]
    fn a_step_is_described_to_the_places_it_needs() {
        assert_eq!(decimals(1.0), 0);
        assert_eq!(decimals(0.01), 2, "a cent, the case every existing price depends on");
        assert_eq!(decimals(0.0001), 4, "a pip");
        assert_eq!(decimals(0.00000001), 8, "a satoshi");
        assert_eq!(decimals(100.0), 0, "a lot larger than one is still whole units");
        assert_eq!(decimals(0.5), 1, "a step that is not a power of ten");
        assert_eq!(decimals(0.005), 3);
        assert_eq!(decimals(1e-12), MAX_DECIMALS, "finer than the ceiling stops at it");
    }
}
