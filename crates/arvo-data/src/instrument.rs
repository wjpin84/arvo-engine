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
    /// A coin pair: `base` is what is bought and sold, `quote` is what it is
    /// priced and settled in. `BTC-USD` buys Bitcoin with dollars.
    Crypto { base: String, quote: String },
}

/// The quote currencies a pair is recognised by.
///
/// A dash is not enough on its own — `BRK-B.YF` is a share class and is in the
/// library today — so a pair is a symbol whose last dashed part is one of
/// these. That is genuinely what the name tells, which is all
/// [`Instrument::of`] is allowed to claim.
const QUOTES: [&str; 5] = ["USD", "USDT", "USDC", "BTC", "ETH"];

/// The finest step anything here is described in: one hundred-millionth, the
/// smallest unit of a Bitcoin and as fine as [`MAX_DECIMALS`] allows.
const SATOSHI: f64 = 0.000_000_01;

/// The base and quote a pair-shaped symbol names, if it is one.
///
/// Takes the id's symbol — the part before the venue — so `BTC-USD.ALPACA`
/// asks about `BTC-USD`.
fn pair(symbol: &str) -> Option<(String, String)> {
    let (base, quote) = symbol.rsplit_once('-')?;
    if base.is_empty() || !QUOTES.contains(&quote) {
        return None;
    }
    Some((base.to_owned(), quote.to_owned()))
}

/// When the instrument trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hours {
    /// The US regular session, 09:30–16:00 New York; see [`crate::session`].
    Regular,
    /// Around the clock, as crypto does.
    Continuous,
}

impl Hours {
    /// The hours every one of `ids` trades, or `None` when they disagree.
    ///
    /// For anything pooled across instruments. A year is 252 sessions for an
    /// equity and 365 days for a coin, so a curve combining both has no
    /// annualisation that is right for either — and `None` here means the
    /// pooled figure is reported as absent rather than as a number nobody
    /// should read. An empty set has no hours to share.
    #[must_use]
    pub fn shared<'a>(ids: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut hours = ids.into_iter().map(|id| Instrument::of(id).hours);
        let first = hours.next()?;
        hours.all(|next| next == first).then_some(first)
    }
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
    /// lots of a hundred units; a symbol quoted in a currency from [`QUOTES`]
    /// is a coin pair, around the clock; anything else is a stock, in single
    /// shares. The first and last tick at a cent.
    ///
    /// # A pair is described finer than any venue, on purpose
    ///
    /// A coin's real tick and minimum order size are the venue's to say and
    /// differ per pair — Bitcoin is not quoted like XRP. The name cannot tell,
    /// so the fallback takes the finest step this crate describes rather than
    /// guessing a coarser one. That errs in the one safe direction: carrying
    /// more decimals than a venue needs loses nothing, while assuming a cent
    /// would silently round XRP at 2.4567 to 2.46 and anything sub-cent to
    /// zero. A size finer than the venue's minimum is refused by the venue,
    /// out loud, which is the failure this platform prefers.
    ///
    /// ponytail: the real tick and lot arrive when a crypto source describes
    /// what it serves (arvo-desktop #246); until then every pair is satoshis.
    #[must_use]
    pub fn of(id: &str) -> Self {
        let symbol = id.split_once('.').map_or(id, |(symbol, _)| symbol);
        match (OptionContract::parse(id), pair(symbol)) {
            (Some(contract), _) => Self {
                id: id.to_owned(),
                kind: Kind::Option(contract),
                tick: 0.01,
                lot: 100.0,
                multiplier: 1.0,
                hours: Hours::Regular,
                margin: None,
            },
            (None, Some((base, quote))) => Self {
                id: id.to_owned(),
                kind: Kind::Crypto { base, quote },
                tick: SATOSHI,
                lot: SATOSHI,
                multiplier: 1.0,
                hours: Hours::Continuous,
                margin: None,
            },
            (None, None) => Self {
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
    /// millionth of a lot, so a float that arrived through arithmetic is
    /// judged on what it means.
    ///
    /// # Why the tolerance is not just a millionth
    ///
    /// A millionth of a lot is meaningful while the lot count is small. With a
    /// satoshi lot the count is enormous — 691358.0247 of a coin is 69 trillion
    /// lots — and above about 4.5e9 lots consecutive `f64` values are already
    /// further apart than a millionth, so a fixed millionth is a tolerance
    /// nothing can satisfy. `98765.4321 * 7` is exactly that case: a whole
    /// number of satoshis that missed by 8e-3 lots and was refused as
    /// [`crate::Instrument::is_whole_lots`]` == false`, which the risk gate
    /// turns into `NotWholeLot` on a size it computed itself.
    ///
    /// So the tolerance is a millionth of a lot *or* the float spacing at this
    /// magnitude, whichever is coarser. It stays far below half a lot either
    /// way, so a genuine two-and-a-half contracts is still refused.
    #[must_use]
    pub fn is_whole_lots(&self, quantity: f64) -> bool {
        let lots = quantity / self.lot;
        let tolerance = 1e-6_f64.max(lots.abs() * f64::EPSILON * 4.0);
        (lots - lots.round()).abs() < tolerance
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

        // A satoshi lot puts the lot count where a fixed millionth stops
        // meaning anything: 691358.0247 of a coin is 69 trillion satoshis, and
        // consecutive f64 values there are 8e-3 lots apart. Refusing it would
        // be `NotWholeLot` on a size the risk gate itself computed (#244).
        let coin = Instrument::of("XRP-USD.ALPACA");
        assert!(coin.is_whole_lots(98_765.432_1 * 7.0), "a whole number of satoshis");
        assert!(coin.is_whole_lots(123.456_789_01));
        assert!(!coin.is_whole_lots(0.000_000_015), "half a satoshi is not a size");
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
    fn a_pair_is_a_pair_and_a_share_class_is_not() {
        let coin = Instrument::of("BTC-USD.ALPACA");
        assert_eq!(
            coin.kind,
            Kind::Crypto { base: "BTC".to_owned(), quote: "USD".to_owned() }
        );
        assert_eq!(coin.hours, Hours::Continuous);
        assert_eq!(coin.price_precision(), 8, "a coin keeps its decimals");
        assert_eq!(coin.size_precision(), 8, "and a fraction of one is a size");

        // The reason a dash alone cannot mean "pair": this is in the library.
        let class = Instrument::of("BRK-B.YF");
        assert_eq!(class.kind, Kind::Stock);
        assert_eq!(class.hours, Hours::Regular);
        assert_eq!(class.price_precision(), 2);
        assert_eq!(class.size_precision(), 0);
    }

    #[test]
    fn a_pair_is_recognised_by_what_it_is_quoted_in() {
        for id in ["ETH-USDT.ALPACA", "SOL-USDC", "ETH-BTC.ALPACA", "DOGE-USD"] {
            assert!(
                matches!(Instrument::of(id).kind, Kind::Crypto { .. }),
                "{id} is a pair"
            );
        }
        for id in ["BRK-B.YF", "AAPL.YF", "MSFT", "-USD.ALPACA", "SPY-GBP"] {
            assert!(
                !matches!(Instrument::of(id).kind, Kind::Crypto { .. }),
                "{id} is not a pair"
            );
        }
    }

    /// The venue is not part of the symbol, and a coin's venue can be dashed
    /// without making the instrument something else.
    #[test]
    fn the_venue_suffix_is_not_read_as_a_quote_currency() {
        assert_eq!(Instrument::of("AAPL.ALPACA-IEX").kind, Kind::Stock);
        assert!(matches!(
            Instrument::of("BTC-USD.ALPACA-CRYPTO").kind,
            Kind::Crypto { .. }
        ));
    }

    /// What the pooled half of a panel depends on: instruments that do not
    /// share a calendar have no shared annualisation, and saying so is the
    /// point.
    #[test]
    fn hours_are_shared_only_when_every_instrument_agrees() {
        assert_eq!(Hours::shared(["MSFT.RH", "AAPL.YF"]), Some(Hours::Regular));
        assert_eq!(
            Hours::shared(["BTC-USD.ALPACA", "ETH-USD.ALPACA"]),
            Some(Hours::Continuous)
        );
        assert_eq!(
            Hours::shared(["MSFT.RH", "BTC-USD.ALPACA"]),
            None,
            "252 sessions and 365 days have no average worth reporting"
        );
        assert_eq!(Hours::shared(["MSFT.RH"]), Some(Hours::Regular));
        assert_eq!(Hours::shared([]), None, "nothing has no hours to share");
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
