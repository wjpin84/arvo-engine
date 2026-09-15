//! The shape of Robinhood's replies.
//!
//! The read tools answer under `data.results`, with prices as strings — which
//! is the one thing about this vendor a caller must never have to know. Pure
//! functions over JSON, so all of it is tested without a server.
//!
//! The order tools answer in a different shape and are parsed in `execution`,
//! beside the calls that produce them.

use arvo_data::source::{Fetched, Match, SourceError};
use arvo_data::Bar;
use serde_json::Value;

use crate::source::{SOURCE_ID, VENUE};

pub(crate) fn malformed(detail: impl Into<String>) -> SourceError {
    SourceError::Malformed {
        vendor: SOURCE_ID,
        detail: detail.into(),
    }
}

/// Turns one response into bars, dropping the ones the server invented.
///
/// Interpolated bars are gap-fill the server synthesised to keep a series
/// contiguous; the tool's own guide says they carry no new information. Feeding
/// one to a strategy would be feeding it a price nobody traded at, and a
/// breakout rule cannot tell the difference.
pub(crate) fn parse_bars(response: &Value, symbol: &str) -> Result<Fetched, SourceError> {
    let results = response
        .pointer("/data/results")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("no data.results array"))?;

    let result = results
        .iter()
        .find(|result| result.get("symbol").and_then(Value::as_str) == Some(symbol))
        .ok_or_else(|| malformed(format!("no result for {symbol}")))?;

    let raw = result
        .get("bars")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("no bars array"))?;

    let mut bars = Vec::with_capacity(raw.len());
    let mut interpolated = 0;

    for bar in raw {
        if bar.get("interpolated").and_then(Value::as_bool) == Some(true) {
            interpolated += 1;
            continue;
        }
        bars.push(one_bar(bar)?);
    }

    // Ordered on the way in, so a caller never has to wonder. The server
    // returns them ascending, but that is its choice rather than a promise.
    bars.sort_by_key(|bar| bar.at);
    Ok(Fetched { bars, interpolated })
}

fn one_bar(bar: &Value) -> Result<Bar, SourceError> {
    // Prices arrive as strings — `"501.990000"` — so they cannot be read as
    // numbers, and treating a missing one as zero would put a bar at zero into
    // a price series.
    let price = |field: &str| -> Result<f64, SourceError> {
        bar.get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| malformed(format!("bar has no {field}")))?
            .parse::<f64>()
            .map_err(|err| malformed(format!("{field}: {err}")))
    };

    let begins_at = bar
        .get("begins_at")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("bar has no begins_at"))?;
    // Left-edge labelled in UTC — the instant the bar *opens*, which is what
    // `arvo_data::Bar::at` means. The engine boundary adds the interval to get
    // the close, so a right-edge reading here would shift every bar by one
    // period and hand strategies a bar before it existed.
    let at = chrono::DateTime::parse_from_rfc3339(begins_at)
        .map_err(|err| malformed(format!("begins_at {begins_at:?}: {err}")))?
        .naive_utc();

    Ok(Bar {
        at,
        open: price("open_price")?,
        high: price("high_price")?,
        low: price("low_price")?,
        close: price("close_price")?,
        volume: bar
            .get("volume")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
    })
}

pub(crate) fn parse_matches(
    response: &Value,
    known: &std::collections::HashMap<String, String>,
) -> Vec<Match> {
    response
        .pointer("/data/results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|found| {
                    let symbol = found.get("symbol").and_then(Value::as_str)?;
                    // `simple_name` is what a person would call it; `name` is
                    // the legal one. Prefer the readable, fall back to the
                    // exact, and never show an empty row.
                    let name = found
                        .get("simple_name")
                        .and_then(Value::as_str)
                        .or_else(|| found.get("name").and_then(Value::as_str))
                        .unwrap_or(symbol);
                    let venue = known.get(symbol).map_or(VENUE, String::as_str);
                    Some(Match {
                        instrument: format!("{symbol}.{venue}"),
                        symbol: symbol.to_owned(),
                        name: name.to_owned(),
                        price: None,
                        change: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One position at the broker. Priced separately, because positions carry no
/// price.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    /// `MSFT` for a stock, `XRP-USD` for a coin.
    pub symbol: String,
    pub quantity: f64,
    /// Total paid for what is held now. `None` when the broker cannot say for
    /// all of it — see [`parse_crypto_positions`].
    pub cost_basis: Option<f64>,
    /// The broker's own current price. `None` when it did not quote one.
    pub price: Option<f64>,
}

/// What one brokerage account holds.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldAccount {
    pub account_number: String,
    pub holdings: Vec<Held>,
    pub cash: f64,
}

fn number(value: Option<&Value>) -> Option<f64> {
    value?.as_str()?.parse::<f64>().ok()
}

/// Account numbers to read: `(account_number, rhs_account_number)`, skipping
/// closed accounts.
pub(crate) fn parse_accounts(response: &Value) -> Vec<(String, String)> {
    response
        .pointer("/data/accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|account| account.get("deactivated").and_then(Value::as_bool) != Some(true))
        .filter_map(|account| {
            let number = account.get("account_number")?.as_str()?.to_owned();
            let crypto = account
                .get("rhs_account_number")
                .and_then(Value::as_str)
                .map_or_else(|| number.clone(), str::to_owned);
            Some((number, crypto))
        })
        .collect()
}

/// A reply that says there is more is refused rather than half-read: a
/// portfolio missing its second page looks complete and is not.
///
/// ponytail: one page only; follow the cursor if an account ever has one.
fn single_page(response: &Value) -> Result<(), SourceError> {
    match response.pointer("/data/next") {
        Some(Value::String(next)) if !next.is_empty() => Err(malformed(
            "positions run to a second page, and reading only the first would drop holdings",
        )),
        _ => Ok(()),
    }
}

/// Stock positions, unpriced.
///
/// Cost basis is quantity times `average_buy_price`, which the tool says
/// already reflects partial sells; a position still reconciling has none.
pub(crate) fn parse_equity_positions(response: &Value) -> Result<Vec<Held>, SourceError> {
    single_page(response)?;
    let positions = response
        .pointer("/data/positions")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("no data.positions array"))?;

    let mut out = Vec::new();
    for position in positions {
        let symbol = position
            .get("symbol")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("a position has no symbol"))?;
        if position.get("type").and_then(Value::as_str).is_some_and(|kind| kind != "long") {
            return Err(SourceError::Unsupported(format!(
                "{symbol} is held short, and a holdings file has no way to say so"
            )));
        }
        let quantity = number(position.get("quantity"))
            .ok_or_else(|| malformed(format!("{symbol} has no readable quantity")))?;
        if quantity == 0.0 {
            continue;
        }
        out.push(Held {
            symbol: symbol.to_owned(),
            quantity,
            cost_basis: number(position.get("average_buy_price")).map(|average| average * quantity),
            price: None,
        });
    }
    Ok(out)
}

/// Coin positions, unpriced, as `CODE-USD`.
///
/// The cost bases cover **direct purchases over the position's life**, not
/// the units held: 400 coins bought for $800 with 100 still held. So the
/// average is taken over what was bought and applied to what is held. When
/// fewer units were bought than are held, the rest arrived by transfer or
/// reward with no cost, and a basis over part of the units would understate
/// the whole — `None`.
pub(crate) fn parse_crypto_positions(response: &Value) -> Result<Vec<Held>, SourceError> {
    single_page(response)?;
    let results = response
        .pointer("/data/results")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("no data.results array"))?;

    let mut out = Vec::new();
    for position in results {
        let code = position
            .pointer("/currency/code")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("a coin position has no currency code"))?;
        let quantity = number(position.get("quantity"))
            .ok_or_else(|| malformed(format!("{code} has no readable quantity")))?;
        if quantity == 0.0 {
            continue;
        }
        let (bought, paid) = position
            .get("cost_bases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .fold((0.0, 0.0), |(bought, paid), basis| {
                (
                    bought + number(basis.get("direct_quantity")).unwrap_or_default(),
                    paid + number(basis.get("direct_cost_basis")).unwrap_or_default(),
                )
            });
        out.push(Held {
            symbol: format!("{code}-USD"),
            quantity,
            cost_basis: (bought >= quantity && bought > 0.0).then(|| paid / bought * quantity),
            price: None,
        });
    }
    Ok(out)
}

/// Coin prices by pair as the reply spells it, `XRPUSD`. A zero mark means
/// the book is empty, and is no price at all.
pub(crate) fn parse_crypto_marks(response: &Value) -> std::collections::HashMap<String, f64> {
    response
        .pointer("/data/results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|quote| {
            let symbol = quote.get("symbol")?.as_str()?;
            let mark = number(quote.get("mark_price")).filter(|mark| *mark > 0.0)?;
            Some((symbol.to_owned(), mark))
        })
        .collect()
}

/// Cash in the account.
pub(crate) fn parse_cash(response: &Value) -> Result<f64, SourceError> {
    number(response.pointer("/data/cash")).ok_or_else(|| malformed("no readable data.cash"))
}

/// Sector by symbol, for the names the vendor classifies.
///
/// A name with no sector is absent rather than filed under an empty string. An
/// empty label would be a sector of its own, and a sector cap would admit one
/// unclassified name as if it were diversification.
///
/// `Miscellaneous` is treated the same way. It is where funds go — SPY came
/// back as it on 2026-09-15 — and taking it as a sector would cap an equity
/// index, gold and long bonds as one bet.
pub(crate) fn parse_sectors(response: &Value) -> std::collections::BTreeMap<String, String> {
    response
        .pointer("/data/results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|found| {
            let symbol = found.get("symbol").and_then(Value::as_str)?;
            let sector = found
                .get("sector")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|sector| !sector.is_empty() && *sector != "Miscellaneous")?;
            Some((symbol.to_owned(), sector.to_owned()))
        })
        .collect()
}

/// Last price and the move since the previous close, by symbol.
pub(crate) fn parse_quotes(response: &Value) -> std::collections::HashMap<String, (f64, Option<f64>)> {
    let mut out = std::collections::HashMap::new();
    let Some(results) = response.pointer("/data/results").and_then(Value::as_array) else {
        return out;
    };

    for entry in results {
        let Some(quote) = entry.get("quote") else {
            continue;
        };
        // Prices are strings here as they are everywhere else in this feed.
        let number = |field: &str| {
            quote
                .get(field)
                .and_then(Value::as_str)
                .and_then(|text| text.parse::<f64>().ok())
        };
        let (Some(symbol), Some(price)) = (
            quote.get("symbol").and_then(Value::as_str),
            number("last_trade_price"),
        ) else {
            continue;
        };
        // Against the *adjusted* previous close, so a split does not read as a
        // fifty percent crash in the search results.
        let change = number("adjusted_previous_close")
            .filter(|previous| *previous > 0.0)
            .map(|previous| (price - previous) / previous);
        out.insert(symbol.to_owned(), (price, change));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(bars: Value) -> Value {
        json!({ "data": { "results": [{ "symbol": "MSFT", "bars": bars }] } })
    }

    fn bar(at: &str, close: &str, interpolated: bool) -> Value {
        json!({
            "begins_at": at,
            "open_price": close,
            "high_price": close,
            "low_price": close,
            "close_price": close,
            "volume": 1000.0,
            "interpolated": interpolated,
        })
    }

    #[test]
    fn prices_arrive_as_strings_and_are_read_as_numbers() {
        let fetched = parse_bars(
            &reply(json!([bar("2024-01-02T00:00:00Z", "501.990000", false)])),
            "MSFT",
        )
        .unwrap();
        assert_eq!(fetched.bars.len(), 1);
        assert!((fetched.bars[0].close - 501.99).abs() < 1e-9);
    }

    #[test]
    fn a_bar_the_server_invented_is_dropped_and_counted() {
        // Gap-fill carries no new information, and a breakout rule cannot tell
        // an invented price from a real one.
        let fetched = parse_bars(
            &reply(json!([
                bar("2024-01-02T00:00:00Z", "10.0", false),
                bar("2024-01-03T00:00:00Z", "10.0", true),
                bar("2024-01-04T00:00:00Z", "11.0", false),
            ])),
            "MSFT",
        )
        .unwrap();
        assert_eq!(fetched.bars.len(), 2);
        assert_eq!(fetched.interpolated, 1, "reported, not silently dropped");
    }

    #[test]
    fn bars_come_out_oldest_first_whatever_order_they_arrived_in() {
        let fetched = parse_bars(
            &reply(json!([
                bar("2024-01-04T00:00:00Z", "11.0", false),
                bar("2024-01-02T00:00:00Z", "10.0", false),
            ])),
            "MSFT",
        )
        .unwrap();
        assert!(fetched.bars[0].at < fetched.bars[1].at);
    }

    #[test]
    fn a_bar_missing_a_price_is_named_rather_than_zeroed() {
        let broken = json!({ "begins_at": "2024-01-02T00:00:00Z", "open_price": "1.0" });
        let Err(SourceError::Malformed { detail, .. }) = parse_bars(&reply(json!([broken])), "MSFT")
        else {
            panic!("a missing price is a shape problem, not a zero");
        };
        assert!(detail.contains("high_price"), "{detail}");
    }

    #[test]
    fn a_reply_for_another_symbol_is_not_read_as_this_one() {
        let body = json!({ "data": { "results": [{ "symbol": "AAPL", "bars": [] }] } });
        assert!(matches!(
            parse_bars(&body, "MSFT"),
            Err(SourceError::Malformed { .. })
        ));
    }

    #[test]
    fn a_change_is_measured_against_the_adjusted_previous_close() {
        // Otherwise a split reads as a fifty percent crash in the results.
        let body = json!({ "data": { "results": [{ "quote": {
            "symbol": "MSFT",
            "last_trade_price": "110.00",
            "adjusted_previous_close": "100.00",
        }}]}});
        let priced = parse_quotes(&body);
        let (price, change) = priced.get("MSFT").unwrap();
        assert!((price - 110.0).abs() < 1e-9);
        assert!((change.unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn a_quote_with_no_previous_close_has_a_price_and_no_change() {
        // `None` rather than zero: a zero change is a claim, and a wrong one.
        let body = json!({ "data": { "results": [{ "quote": {
            "symbol": "MSFT",
            "last_trade_price": "110.00",
        }}]}});
        assert_eq!(parse_quotes(&body).get("MSFT").unwrap().1, None);
    }

    #[test]
    fn a_symbol_already_held_keeps_the_venue_it_is_filed_under() {
        // Otherwise a second fetch becomes a near-duplicate under another name,
        // and every comparison between the two is wrong.
        let known = std::collections::HashMap::from([("MSFT".to_owned(), "NASDAQ".to_owned())]);
        let body = json!({ "data": { "results": [
            { "symbol": "MSFT", "simple_name": "Microsoft" },
            { "symbol": "NVDA", "simple_name": "Nvidia" },
        ]}});
        let found = parse_matches(&body, &known);
        assert_eq!(found[0].instrument, "MSFT.NASDAQ");
        assert_eq!(found[1].instrument, "NVDA.RH", "unheld falls back to ours");
    }

    #[test]
    fn stock_cost_is_quantity_times_the_average_and_a_reconciling_one_has_none() {
        // Fields as `get_equity_positions` answered on 2026-09-15.
        let body = json!({ "data": { "positions": [
            { "symbol": "CSX", "quantity": "3.500000", "average_buy_price": "25.000000", "type": "long" },
            { "symbol": "NEW", "quantity": "2.0", "type": "long" },
            { "symbol": "SOLD", "quantity": "0.000000", "average_buy_price": "10.0", "type": "long" },
        ]}});
        let held = parse_equity_positions(&body).unwrap();
        assert_eq!(held.len(), 2, "a closed-out row is not a holding");
        assert!((held[0].cost_basis.unwrap() - 87.5).abs() < 1e-9);
        assert_eq!(held[1].cost_basis, None);
    }

    #[test]
    fn a_second_page_or_a_short_is_refused_rather_than_half_read() {
        let paged = json!({ "data": { "positions": [], "next": "https://…?cursor=abc" } });
        assert!(parse_equity_positions(&paged).is_err());
        let short = json!({ "data": { "positions": [
            { "symbol": "GME", "quantity": "5", "type": "short" },
        ]}});
        assert!(matches!(parse_equity_positions(&short), Err(SourceError::Unsupported(_))));
    }

    #[test]
    fn coin_cost_is_the_bought_average_applied_to_what_is_still_held() {
        // The real shape: 400 bought over the position's life for $800, 100
        // still held. Summing the basis would say $800 was paid for 100 coins.
        let body = json!({ "data": { "results": [
            { "currency": { "code": "XRP" }, "quantity": "100",
              "cost_bases": [{ "direct_quantity": "400", "direct_cost_basis": "800.00" }] },
            { "currency": { "code": "DOGE" }, "quantity": "100",
              "cost_bases": [{ "direct_quantity": "40", "direct_cost_basis": "4.00" }] },
        ]}});
        let held = parse_crypto_positions(&body).unwrap();
        assert_eq!(held[0].symbol, "XRP-USD");
        assert!((held[0].cost_basis.unwrap() - 200.0).abs() < 1e-9);
        assert_eq!(held[1].cost_basis, None, "60 of 100 arrived with no cost");
    }

    #[test]
    fn an_empty_book_is_no_coin_price_and_cash_must_be_readable() {
        let quotes = json!({ "data": { "results": [
            { "symbol": "XRPUSD", "mark_price": "1.40044333" },
            { "symbol": "DEADUSD", "mark_price": "0" },
        ]}});
        let marks = parse_crypto_marks(&quotes);
        assert!((marks["XRPUSD"] - 1.400_443_33).abs() < 1e-12);
        assert!(!marks.contains_key("DEADUSD"));

        assert!((parse_cash(&json!({ "data": { "cash": "10.1" } })).unwrap() - 10.1).abs() < 1e-12);
        assert!(parse_cash(&json!({ "data": {} })).is_err(), "not zero");
    }

    #[test]
    fn a_closed_account_is_not_read() {
        let body = json!({ "data": { "accounts": [
            { "account_number": "111", "rhs_account_number": "111", "deactivated": false },
            { "account_number": "222", "deactivated": true },
        ]}});
        assert_eq!(parse_accounts(&body), [("111".to_owned(), "111".to_owned())]);
    }

    #[test]
    fn a_name_with_no_sector_is_left_out_rather_than_given_an_empty_one() {
        // The shape `get_equity_fundamentals` answered with on 2026-09-15:
        // delisted names go to `not_found`, never into `results`.
        let body = json!({ "data": {
            "results": [
                { "symbol": "KO", "sector": "Consumer Non-Durables", "industry": "Beverages: Non-Alcoholic" },
                { "symbol": "NEE", "sector": "Utilities" },
                { "symbol": "SPAC", "sector": "" },
                { "symbol": "ETF", "sector": null },
                { "symbol": "SPY", "sector": "Miscellaneous" },
            ],
            "not_found": ["SIVB"],
        }});
        let sectors = parse_sectors(&body);
        assert_eq!(sectors.len(), 2, "{sectors:?}");
        assert_eq!(sectors["KO"], "Consumer Non-Durables");
        assert_eq!(sectors["NEE"], "Utilities");
    }

    #[test]
    fn a_readable_name_is_preferred_over_the_legal_one() {
        let body = json!({ "data": { "results": [
            { "symbol": "MSFT", "simple_name": "Microsoft", "name": "Microsoft Corp. Common Stock" },
            { "symbol": "NVDA", "name": "NVIDIA Corporation" },
        ]}});
        let found = parse_matches(&body, &std::collections::HashMap::new());
        assert_eq!(found[0].name, "Microsoft");
        assert_eq!(found[1].name, "NVIDIA Corporation", "falls back to exact");
    }
}
