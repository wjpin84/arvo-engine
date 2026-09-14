//! Option contracts that existed, and the bars they traded (#15).
//!
//! # Expired contracts, or the history is survivors
//!
//! A chain listed today holds only contracts that have not expired. A backtest
//! built from it would never see the 0DTE put that went to zero or the spread
//! that was assigned — every one of those is gone from the listing. Alpaca's
//! contract endpoint serves `inactive` contracts too, back to 2024-01, so both
//! statuses are always asked for.
//!
//! # What the bars are
//!
//! OPRA trades aggregated by Alpaca, filed under [`VENUE`]. Not adjusted for
//! anything — an option's price is not restated after the fact. A period with
//! no trade has **no bar**, and that absence is the answer: a thinly traded
//! contract did not trade, and a fill model that invents a price for it is
//! making the data up (#14).
//!
//! No quotes: Alpaca serves none historically. See [`crate::options`].

use std::collections::BTreeMap;

use arvo_data::option::OptionContract;
use arvo_data::source::SourceError;
use arvo_data::{Bar, BarInterval};
use chrono::NaiveDate;
use serde_json::Value;

use crate::auth::get;
use crate::parse::{malformed, parse_bars};

/// Where option contracts are listed. The trading API, not the data API; the
/// paper host, because listing needs no live account and a paper key is the
/// only kind this platform holds.
const CONTRACTS: &str = "https://paper-api.alpaca.markets/v2/options/contracts";
const DATA: &str = "https://data.alpaca.markets";

/// The venue option bars from Alpaca file under.
pub const VENUE: &str = "AOPT";

/// Symbols per bars request. Keeps the URL well under any server's limit.
const SYMBOLS_PER_REQUEST: usize = 100;

/// Every standard contract on `underlying` expiring between `from` and `to`,
/// expired or not.
///
/// A contract whose multiplier is not 100 is left out rather than read as if
/// it were: it is an adjusted contract delivering something other than 100
/// shares, and pricing it as standard would be wrong by that ratio.
///
/// # Errors
///
/// Transport and shape errors from the listing.
pub async fn contracts(
    underlying: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<OptionContract>, SourceError> {
    let mut found = Vec::new();
    for status in ["inactive", "active"] {
        let mut page: Option<String> = None;
        loop {
            let mut url = format!(
                "{CONTRACTS}?underlying_symbols={underlying}&status={status}\
                 &expiration_date_gte={from}&expiration_date_lte={to}&limit=10000"
            );
            if let Some(token) = &page {
                url.push_str(&format!("&page_token={token}"));
            }
            let body = get(&url).await?;
            found.extend(parse_contracts(&body)?);
            page = body
                .get("next_page_token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if page.is_none() {
                break;
            }
        }
    }
    found.sort_by_key(OptionContract::symbol);
    found.dedup();
    Ok(found)
}

pub(crate) fn parse_contracts(body: &Value) -> Result<Vec<OptionContract>, SourceError> {
    let Some(rows) = body.get("option_contracts").and_then(Value::as_array) else {
        return Err(malformed("a contract listing with no option_contracts"));
    };
    Ok(rows
        .iter()
        .filter(|row| row.get("multiplier").and_then(Value::as_str) == Some("100"))
        .filter_map(|row| row.get("symbol").and_then(Value::as_str))
        .filter_map(OptionContract::parse)
        .collect())
}

/// Bars for many contracts at once, keyed by OCC symbol. A contract that never
/// traded in the window is absent from the map.
///
/// Intraday bars are kept to the stock's regular session. SPY options print
/// until 16:15 on days they are not expiring, but the underlying they are priced
/// against has closed at 16:00, and a bar with no underlying beside it cannot be
/// valued; an expiring contract stops at 16:00 anyway.
///
/// # Errors
///
/// Transport and shape errors from any request; nothing partial is returned.
pub async fn bars(
    symbols: &[String],
    interval: BarInterval,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<BTreeMap<String, Vec<Bar>>, SourceError> {
    let timeframe = crate::source::spelling(interval)?;
    let mut all: BTreeMap<String, Vec<Bar>> = BTreeMap::new();
    for batch in symbols.chunks(SYMBOLS_PER_REQUEST) {
        let joined = batch.join(",");
        let mut page: Option<String> = None;
        loop {
            // A bare `end` date covers that whole day, intraday included.
            let mut url = format!(
                "{DATA}/v1beta1/options/bars?symbols={joined}&timeframe={timeframe}\
                 &start={from}&end={to}&limit=10000"
            );
            if let Some(token) = &page {
                url.push_str(&format!("&page_token={token}"));
            }
            let body = get(&url).await?;
            for symbol in batch {
                let bars = parse_bars(&body, symbol)?;
                if !bars.is_empty() {
                    all.entry(symbol.clone()).or_default().extend(bars);
                }
            }
            page = body
                .get("next_page_token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if page.is_none() {
                break;
            }
        }
    }
    for series in all.values_mut() {
        series.sort_by_key(|bar| bar.at);
        series.dedup_by_key(|bar| bar.at);
        if interval.is_intraday() {
            series.retain(|bar| arvo_data::session::in_regular_session(bar.at));
        }
    }
    all.retain(|_, series| !series.is_empty());
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_adjusted_contract_is_left_out_of_the_listing() {
        let body = json!({ "option_contracts": [
            { "symbol": "SPY250912C00640000", "multiplier": "100", "status": "inactive" },
            { "symbol": "SPY250912P00640000", "multiplier": "100", "status": "inactive" },
            { "symbol": "XYZ1250912C00050000", "multiplier": "150" },
        ], "next_page_token": null });
        let listed = parse_contracts(&body).expect("well formed");
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|contract| contract.underlying == "SPY"));
    }

    #[test]
    fn a_listing_without_contracts_is_an_error_not_an_empty_chain() {
        // An error body parsed as "no contracts" would read as an underlying
        // with nothing listed, and a backtest would run on silence.
        assert!(matches!(
            parse_contracts(&json!({ "message": "forbidden" })),
            Err(SourceError::Malformed { .. })
        ));
    }
}
