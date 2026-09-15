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
