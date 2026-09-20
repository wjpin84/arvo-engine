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
}
