//! Arvo's instruments and bars, built as Nautilus's.

use arvo_research::SimulationError;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::{Bar, BarType},
    enums::{AssetClass, BarAggregation, OptionKind},
    identifiers::{InstrumentId, Symbol},
    instruments::{CurrencyPair, Equity, InstrumentAny, OptionContract},
    types::{Currency, Price, Quantity},
};
use rust_decimal::Decimal;
use ustr::Ustr;

/// How many decimal places one instrument's prices and quantities need.
///
/// These were two module constants — two places of price, whole units of size —
/// which is right for a US share and wrong for anything quoted finer. They are
/// the instrument's own answer now, read from the tick and lot its source
/// described (#240), and resolved once per instrument rather than once per bar.
///
/// A stock ticking at a cent in lots of one still resolves to 2 and 0, so every
/// price and quantity the engine built before this is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Precision {
    pub(crate) price: u8,
    pub(crate) size: u8,
}

impl Precision {
    /// What the instrument's own tick and lot need.
    pub(crate) fn of(described: &arvo_data::Instrument) -> Self {
        Self {
            price: described.price_precision(),
            size: described.size_precision(),
        }
    }

    /// What the name alone says, for a caller that has no described instrument
    /// to hand — the same fallback [`arvo_data::Instrument::of`] is everywhere
    /// else.
    pub(crate) fn named(id: &str) -> Self {
        Self::of(&arvo_data::Instrument::of(id))
    }
}

/// A quantity in one instrument's own units.
///
/// Every order quantity used to be built at precision 0, which is a whole share
/// and was right while everything was one. Nautilus validates an order's
/// quantity precision against the instrument's, so a coin's order was refused
/// outright: "Invalid order quantity precision ... was 0 when XRP-USD.ALPACA
/// size precision is 8". Keyed on the id rather than taken from the strategy,
/// because a ranking rule sends orders for members that are not the instrument
/// it was configured with.
///
/// `None` for a quantity Nautilus cannot represent, which every caller already
/// treats as "do not send this order".
pub(crate) fn sized(id: InstrumentId, quantity: f64) -> Option<Quantity> {
    Quantity::new_checked(quantity, Precision::named(&id.to_string()).size).ok()
}

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
    let precision = Precision::of(&described);
    let tick = Price::new_checked(described.tick, precision.price)?;

    // Optional fields are left unset rather than passed as `None`: the builder
    // applies the same defaults checked construction would.
    let equity = Equity::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        .currency(currency)
        .price_precision(precision.price)
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
    let precision = Precision::of(&described);
    let tick = Price::new_checked(described.tick, precision.price)?;
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
        .strike_price(Price::new_checked(contract.strike, precision.price)?)
        .currency(currency)
        .activation_ns(UnixNanos::default())
        .expiration_ns(UnixNanos::from(u64::try_from(expires)?))
        .price_precision(precision.price)
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

/// Builds a coin pair, in units of the base settled in the quote.
///
/// # Only a pair quoted in the account's currency, and the rest refused by name
///
/// The account is in one currency and the equity curve is in that currency, so
/// `XRP-USD` on a dollar account is a position whose value is already dollars.
/// `ETH-BTC` is not: its value is bitcoin, and turning that into the curve's
/// currency needs a BTC/USD series at every bar — a second dataset, with its own
/// gaps and its own fetch. A stablecoin quote is the same problem wearing a
/// disguise: `USDT` is not `USD`, and treating it as one would bury a 1:1
/// assumption in the one number the gates read.
///
/// So this refuses anything not quoted in the account's currency, and says which
/// pair and which currency. [`SimulationError::Unsupported`] rather than
/// `Rejected` on purpose: the experiment is well formed, the engine simply does
/// not honour it yet, and that distinction is what keeps the reproducibility
/// record honest.
pub(crate) fn pair(
    instrument_id: InstrumentId,
    described: &arvo_data::Instrument,
    base: &str,
    quote: &str,
    currency: Currency,
    commission_bps: f64,
) -> Result<InstrumentAny, SimulationError> {
    if quote != currency.code.as_str() {
        return Err(SimulationError::Unsupported(format!(
            "{instrument_id} is quoted in {quote} and the account is in {}; a pair that does not \
             settle in the account's currency needs a conversion series this engine does not hold",
            currency.code
        )));
    }

    let rejected = |what: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("{instrument_id}: {what}: {err}"))
    };
    let precision = Precision::of(described);
    let fee = Decimal::try_from(commission_bps / 10_000.0)
        .map_err(|err| rejected("commission", &err))?;

    let currency_pair = CurrencyPair::builder()
        .instrument_id(instrument_id)
        .raw_symbol(Symbol::from(instrument_id.symbol.as_str()))
        // The base is whatever the pair names. Nautilus registers a crypto
        // currency it has not seen rather than refusing it, which is what a
        // venue listing a new coin needs.
        .base_currency(Currency::get_or_create_crypto(base))
        .quote_currency(currency)
        .price_precision(precision.price)
        .size_precision(precision.size)
        .price_increment(
            Price::new_checked(described.tick, precision.price)
                .map_err(|err| rejected("tick", &err))?,
        )
        .size_increment(
            Quantity::new_checked(described.lot, precision.size)
                .map_err(|err| rejected("lot", &err))?,
        )
        // Spot, bought outright: no margin, and no rounded lot unit beyond the
        // size increment itself.
        .maker_fee(fee)
        .taker_fee(fee)
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()
        .map_err(|err| rejected("building the pair", &err))?;

    Ok(InstrumentAny::CurrencyPair(currency_pair))
}

pub(crate) fn to_nautilus_bar(
    bar_type: BarType,
    bar: &arvo_data::Bar,
    interval: arvo_data::BarInterval,
    precision: Precision,
) -> Result<Bar, SimulationError> {
    let rejected = |what: &str, err: &dyn std::fmt::Display| {
        SimulationError::Rejected(format!("bar {}: {what}: {err}", bar.at))
    };

    let price = |name: &str, value: f64| {
        Price::new_checked(value, precision.price).map_err(|err| rejected(name, &err))
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
        Quantity::new_checked(bar.volume, precision.size)
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
