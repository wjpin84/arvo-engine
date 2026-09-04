//! The single boundary between Arvo and NautilusTrader.
//!
//! # The containment rule
//!
//! **No crate other than this one may name a Nautilus type.** Nautilus's Rust
//! crates are published at `0.x` (0.63.0 at time of writing), so every release
//! may break; its own README treats Python as the primary API and Rust as
//! internal infrastructure. That is survivable only while the blast radius is
//! one crate, and the rule is what keeps it there.
//!
//! Concretely: nothing in this crate's public API mentions a Nautilus type.
//! Callers hand over plain Rust values and receive Arvo-owned types back.
//! There is a test at the bottom of this file that states that intent.
//!
//! # What crosses the boundary
//!
//! Experiments and outcomes — never trading primitives. Arvo does not model
//! orders, fills, positions or accounts, because Arvo never manipulates them;
//! they live entirely on the Nautilus side of this line. That is what lets
//! containment coexist with *not* duplicating Nautilus's domain model.
//!
//! # Licensing
//!
//! NautilusTrader is LGPL-3.0-only and is linked into this binary. See the
//! repository `NOTICE`. A crate boundary is not a licensing boundary — the
//! obligation attaches to the distributed binary — but keeping the dependency
//! to this one crate keeps the fact obvious rather than diffuse.

use nautilus_model::identifiers::InstrumentId;
use nautilus_model::types::{Price, Quantity};
use std::str::FromStr;

/// Why a value could not be admitted into the trading domain.
///
/// Nautilus reports these as `anyhow::Error` (via its `CorrectnessResult`) or
/// as its own error enums. Both are flattened here, deliberately: the whole
/// point of this crate is that its callers never see a Nautilus type, and an
/// error type is part of the public API like any other.
#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("invalid instrument id {value:?}: {reason}")]
    InstrumentId { value: String, reason: String },
    #[error("invalid price {value} at precision {precision}: {reason}")]
    Price {
        value: f64,
        precision: u8,
        reason: String,
    },
    #[error("invalid quantity {value} at precision {precision}: {reason}")]
    Quantity {
        value: f64,
        precision: u8,
        reason: String,
    },
}

/// A price and size for an instrument, validated by Nautilus's own domain
/// rules but expressed in plain Rust.
///
/// The fields are the *normalised* values — Nautilus rounds to the requested
/// precision — so this is what Nautilus would actually trade on, not what the
/// caller typed.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedQuote {
    /// Canonical `SYMBOL.VENUE` form, as Nautilus renders it.
    pub instrument: String,
    pub price: f64,
    pub quantity: f64,
}

/// Validates a quote through Nautilus's domain types.
///
/// This is the spike that proves the bridge: it links Nautilus's Rust crates,
/// constructs real domain values, and hands back an Arvo type.
///
/// # Errors
///
/// Returns [`BoundaryError`] if the instrument id is not `SYMBOL.VENUE`, or if
/// the price or quantity violate Nautilus's own correctness rules (precision
/// out of range, non-finite, negative size, and so on).
///
/// # Panics
///
/// Never. Note that Nautilus's ergonomic constructors — `Price::new`,
/// `Quantity::new` — *do* panic on invalid input; only the `_checked` variants
/// return a result. Everything crossing this boundary uses the checked forms,
/// because a bad value from a config file or a UI field is ordinary input, not
/// a bug worth aborting the process over.
pub fn validate_quote(
    instrument: &str,
    price: f64,
    quantity: f64,
    precision: u8,
) -> Result<ValidatedQuote, BoundaryError> {
    let instrument_id =
        InstrumentId::from_str(instrument).map_err(|err| BoundaryError::InstrumentId {
            value: instrument.to_owned(),
            reason: err.to_string(),
        })?;

    let price = Price::new_checked(price, precision).map_err(|err| BoundaryError::Price {
        value: price,
        precision,
        reason: err.to_string(),
    })?;

    let quantity =
        Quantity::new_checked(quantity, precision).map_err(|err| BoundaryError::Quantity {
            value: quantity,
            precision,
            reason: err.to_string(),
        })?;

    Ok(ValidatedQuote {
        instrument: instrument_id.to_string(),
        price: price.as_f64(),
        quantity: quantity.as_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_a_well_formed_quote_through_nautilus() {
        let quote = validate_quote("AAPL.NASDAQ", 191.25, 100.0, 2).expect("should be valid");

        assert_eq!(quote.instrument, "AAPL.NASDAQ");
        assert!((quote.price - 191.25).abs() < f64::EPSILON);
        assert!((quote.quantity - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn instrument_id_without_a_venue_is_an_error_not_a_panic() {
        // Nautilus requires SYMBOL.VENUE; a bare symbol is ordinary bad input.
        let err = validate_quote("AAPL", 191.25, 100.0, 2).expect_err("should reject");
        assert!(matches!(err, BoundaryError::InstrumentId { .. }), "{err}");
    }

    #[test]
    fn invalid_price_is_an_error_not_a_panic() {
        // Precision beyond what Nautilus supports. `Price::new` would panic
        // here; the boundary uses `new_checked` precisely so it does not.
        let err = validate_quote("AAPL.NASDAQ", 191.25, 100.0, 200).expect_err("should reject");
        assert!(
            matches!(
                err,
                BoundaryError::Price { .. } | BoundaryError::Quantity { .. }
            ),
            "{err}"
        );
    }

    #[test]
    fn nan_price_is_an_error_not_a_panic() {
        let err = validate_quote("AAPL.NASDAQ", f64::NAN, 100.0, 2).expect_err("should reject");
        assert!(matches!(err, BoundaryError::Price { .. }), "{err}");
    }

    /// The containment rule, stated as a test rather than only as prose: this
    /// crate's public surface is expressible without naming a Nautilus type.
    /// If someone later leaks one into a signature, this stops compiling in a
    /// way that points at why.
    #[test]
    fn public_api_is_free_of_nautilus_types() {
        fn assert_arvo_only(_: fn(&str, f64, f64, u8) -> Result<ValidatedQuote, BoundaryError>) {}
        assert_arvo_only(validate_quote);
    }
}
