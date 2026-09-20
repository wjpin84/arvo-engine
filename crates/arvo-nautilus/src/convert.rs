//! Arvo's instruments and bars, built as Nautilus's.

use arvo_research::SimulationError;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarType},
    enums::{AssetClass, BarAggregation, OptionKind},
    identifiers::{InstrumentId, Symbol},
    instruments::{Equity, InstrumentAny, OptionContract},
    types::{Currency, Price, Quantity},
};
use rust_decimal::Decimal;
use ustr::Ustr;

/// US equity conventions. Daily bars from the free exports are quoted in cents
/// and traded in whole shares; nothing yet needs another instrument class, and
/// guessing at one would mean guessing wrong.
pub(crate) const PRICE_PRECISION: u8 = 2;
pub(crate) const SIZE_PRECISION: u8 = 0;

/// Builds the traded instrument, with the experiment's commission applied as
/// the venue fee.
///
/// The cost model has to reach the engine or pinning it in the experiment is
/// theatre — this is the half that does reach it.
pub(crate) fn equity(
    instrument_id: InstrumentId,
    currency: Currency,
    commission_bps: f64,
) -> anyhow::Result<InstrumentAny> {
    let fee = Decimal::try_from(commission_bps / 10_000.0)?;
    let described = arvo_data::Instrument::of(&instrument_id.to_string());
    let tick = Price::new_checked(described.tick, PRICE_PRECISION)?;

    // Optional fields are left unset rather than passed as `None`: the builder
    // applies the same defaults checked construction would.
    let equity = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .currency(currency)
        .price_precision(PRICE_PRECISION)
        .price_increment(tick)
        .maker_fee(fee)
        .taker_fee(fee)
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;

    Ok(InstrumentAny::Equity(equity))
}

/// Builds an option contract, traded in shares of what it delivers.
///
/// # A multiplier of one, in lots of a hundred
///
/// A contract is 100 shares' worth, quoted per share. Nautilus can carry that
/// as a multiplier of 100 on a quantity of contracts — and then every figure
/// Arvo derives from a fill (the equity curve, a stop distance, sizing, the
/// per-unit fees) would need to know to multiply, and each one that did not
/// would be wrong by a factor of a hundred without failing.
///
/// So the unit is one share of the deliverable: a multiplier of one, a price
/// per share as quoted, and a quantity that the risk gate only ever sizes in
/// hundreds (`arvo_research::risk`). A price times a quantity is dollars
/// everywhere, as it is for a stock.
///
/// The expiration is real, so Nautilus refuses an order after it, and that
/// refusal reaches the ledger like any other.
pub(crate) fn option(
    instrument_id: InstrumentId,
    contract: &arvo_data::option::OptionContract,
    currency: Currency,
    commission_bps: f64,
) -> anyhow::Result<InstrumentAny> {
    let fee = Decimal::try_from(commission_bps / 10_000.0)?;
    let described = arvo_data::Instrument::of(&instrument_id.to_string());
    let tick = Price::new_checked(described.tick, PRICE_PRECISION)?;
    let expires = contract
        .expires_at()
        .and_utc()
        .timestamp_nanos_opt()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "expiration {} is not a representable instant",
                contract.expiration
            )
        })?;

    let option = OptionContract::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .asset_class(AssetClass::Equity)
        .underlying(Ustr::from(contract.underlying.as_str()))
        .option_kind(match contract.right {
            arvo_data::option::Right::Call => OptionKind::Call,
            arvo_data::option::Right::Put => OptionKind::Put,
        })
        .strike_price(Price::new_checked(contract.strike, PRICE_PRECISION)?)
        .currency(currency)
        .activation_ns(UnixNanos::default())
        .expiration_ns(UnixNanos::from(u64::try_from(expires)?))
        .price_precision(PRICE_PRECISION)
        .price_increment(tick)
        .multiplier(Quantity::from(1))
        .lot_size(Quantity::from(described.lot as u64))
        .maker_fee(fee)
        .taker_fee(fee)
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;

    Ok(InstrumentAny::OptionContract(option))
}

pub(crate) fn to_nautilus_bar(
    bar_type: BarType,
    bar: &arvo_data::Bar,
    interval: arvo_data::BarInterval,
) -> Result<Bar, SimulationError> {
    let rejected = |what: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("bar {}: {what}: {err}", bar.at))
    };

    let price = |name: &str, value: f64| {
        Price::new_checked(value, PRICE_PRECISION).map_err(|err| rejected(name, &err))
    };

    // A bar is only knowable once its period has closed. Timestamping it at
    // the *end* of that period is what stops a strategy acting on a close it
    // could not have seen yet — the look-ahead bias this platform exists to
    // catch. That was close-of-day while everything was daily; it is
    // close-of-bar now, and the daily case is unchanged by it.
    let ts = close_of_bar(bar.at, interval).ok_or_else(|| {
        SimulationError::Rejected(format!(
            "bar timestamp {} is outside the representable range",
            bar.at
        ))
    })?;

    Bar::new_checked(
        bar_type,
        price("open", bar.open)?,
        price("high", bar.high)?,
        price("low", bar.low)?,
        price("close", bar.close)?,
        Quantity::new_checked(bar.volume, SIZE_PRECISION)
            .map_err(|err| rejected("volume", &err))?,
        ts,
        ts,
    )
    .map_err(|err| rejected("failed Nautilus's OHLC checks", &err))
}

/// The instant a bar's period ends, as UNIX nanoseconds.
pub(crate) fn close_of_bar(at: chrono::NaiveDateTime, interval: arvo_data::BarInterval) -> Option<UnixNanos> {
    let end = at.checked_add_signed(interval.duration())?.and_utc();
    let nanos = end.timestamp_nanos_opt()?;
    u64::try_from(nanos).ok().map(UnixNanos::from)
}

/// Maps an Arvo interval onto Nautilus's own aggregation vocabulary.
///
/// # Errors
///
/// Returns [`SimulationError::Unsupported`] for a resolution Nautilus has no
/// aggregation for, rather than silently substituting a neighbouring one — a
/// backtest quietly run at the wrong resolution is worse than one refused.
pub(crate) fn aggregation_of(
    interval: arvo_data::BarInterval,
) -> Result<(usize, BarAggregation), SimulationError> {
    use arvo_data::IntervalUnit;
    let step = usize::try_from(interval.step).map_err(|_| {
        SimulationError::Unsupported(format!("interval step {} is too large", interval.step))
    })?;
    let aggregation = match interval.unit {
        IntervalUnit::Second => BarAggregation::Second,
        IntervalUnit::Minute => BarAggregation::Minute,
        IntervalUnit::Hour => BarAggregation::Hour,
        IntervalUnit::Day => BarAggregation::Day,
        IntervalUnit::Week => BarAggregation::Week,
    };
    Ok((step, aggregation))
}
