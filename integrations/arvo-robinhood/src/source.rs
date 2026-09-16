//! The Robinhood source: what it is called, what it serves, and how it asks.
//!
//! The three read tools and the one interval vocabulary. Getting a client to
//! call them with is [`crate::auth`]; reading what comes back is
//! [`crate::parse`].

use std::path::Path;

use arvo_data::source::{
    Adjustment, Basis, Credential, Feed, Fetched, Match, Quote, Source, SourceError,
};
use arvo_data::{BarInterval, IntervalUnit};
use serde_json::json;

use crate::auth::{connect, is_connected, transport};
use crate::parse::{
    parse_accounts, parse_bars, parse_cash, parse_crypto_marks, parse_crypto_positions,
    parse_equity_positions, parse_matches, parse_quotes, parse_sectors, HeldAccount,
};

/// Where the token lives in the OS keychain, and how this source is named.
pub const SOURCE_ID: &str = "robinhood";

/// The venue an instrument fetched here is filed under. See [`Source::venue`].
pub const VENUE: &str = "RH";

/// The tools this module calls. All three read; the ones that trade live in
/// [`execution`], behind `arvo_execution::Session`.
const HISTORICALS: &str = "get_equity_historicals";
const SEARCH: &str = "search";
const QUOTES: &str = "get_equity_quotes";
const FUNDAMENTALS: &str = "get_equity_fundamentals";
const ACCOUNTS: &str = "get_accounts";
const EQUITY_POSITIONS: &str = "get_equity_positions";
const CRYPTO_POSITIONS: &str = "get_crypto_positions";
const CRYPTO_QUOTES: &str = "get_crypto_quotes";
const PORTFOLIO: &str = "get_portfolio";

/// The most symbols one fundamentals request takes.
const FUNDAMENTALS_PER_CALL: usize = 10;

/// The Robinhood source.
///
/// A unit struct: everything it needs is a token read from the keychain at call
/// time, and holding one on the struct would be holding a credential that can
/// expire underneath it.
///
/// ponytail: one MCP handshake per call — `McpClient::new` plus `connect()` is
/// two extra round trips on every fetch, search and quote. Hold a client on
/// `ResearchService` behind a `OnceCell` if the latency ever shows; the reason
/// it is not done here is that the cached client would also cache a token that
/// `access_token` refreshes out from under it.
pub struct Robinhood;

impl Robinhood {
    /// Sector by ticker, as Robinhood classifies each name **today**.
    ///
    /// Not a `Source` method: no other vendor supplies this, and a trait with
    /// one implementer is a guess at a shape (ADR-0005). A name it does not
    /// classify — delisted, a fund, a typo — is absent from the map, and the
    /// caller decides what absence means.
    ///
    /// # Errors
    ///
    /// [`SourceError`] if Robinhood is not signed in or a request fails.
    pub async fn sectors(
        &self,
        tickers: &[&str],
    ) -> Result<std::collections::BTreeMap<String, String>, SourceError> {
        let mut out = std::collections::BTreeMap::new();
        if tickers.is_empty() {
            return Ok(out);
        }
        let client = connect().await?;
        for chunk in tickers.chunks(FUNDAMENTALS_PER_CALL) {
            let response = client
                .call_tool_json(FUNDAMENTALS, json!({ "symbols": chunk }))
                .await
                .map_err(transport)?;
            out.extend(parse_sectors(&response));
        }
        Ok(out)
    }

    /// Every open account's positions, priced by Robinhood, and its cash (#27).
    ///
    /// Read-only: every tool called here reads. One account failing fails the
    /// whole sync, because a portfolio written from part of what was asked
    /// for looks complete.
    ///
    /// # Errors
    ///
    /// [`SourceError`] if Robinhood is not signed in, a request fails, or a
    /// reply cannot be read whole.
    pub async fn holdings(&self) -> Result<Vec<HeldAccount>, SourceError> {
        let client = connect().await?;
        let call = |tool: &'static str, arguments: serde_json::Value| {
            let client = &client;
            async move { client.call_tool_json(tool, arguments).await.map_err(transport) }
        };

        let mut accounts = Vec::new();
        for (number, crypto_number) in parse_accounts(&call(ACCOUNTS, json!({})).await?) {
            let mut holdings = parse_equity_positions(
                &call(EQUITY_POSITIONS, json!({ "account_number": number })).await?,
            )?;
            if !holdings.is_empty() {
                let symbols: Vec<&str> = holdings.iter().map(|held| held.symbol.as_str()).collect();
                let priced = parse_quotes(&call(QUOTES, json!({ "symbols": symbols })).await?);
                for held in &mut holdings {
                    held.price = priced.get(&held.symbol).map(|(price, _)| *price);
                }
            }

            let mut coins = parse_crypto_positions(
                &call(CRYPTO_POSITIONS, json!({ "rhs_account_number": crypto_number })).await?,
            )?;
            if !coins.is_empty() {
                let symbols: Vec<&str> = coins.iter().map(|held| held.symbol.as_str()).collect();
                let marks = parse_crypto_marks(&call(CRYPTO_QUOTES, json!({ "symbols": symbols })).await?);
                for coin in &mut coins {
                    // The reply spells the pair without its hyphen.
                    coin.price = marks.get(&coin.symbol.replace('-', "")).copied();
                }
            }
            holdings.extend(coins);

            let cash = parse_cash(&call(PORTFOLIO, json!({ "account_number": number })).await?)?;
            accounts.push(HeldAccount {
                account_number: number,
                holdings,
                cash,
            });
        }
        Ok(accounts)
    }
}

#[async_trait::async_trait]
impl Source for Robinhood {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn label(&self) -> &'static str {
        "Robinhood"
    }

    fn provides(&self) -> &'static [&'static str] {
        &["bars", "quotes", "holdings"]
    }

    fn venue(&self) -> &'static str {
        VENUE
    }

    fn credential(&self) -> Credential {
        Credential::SignIn
    }

    fn basis(&self) -> Basis {
        Basis {
            // Robinhood serves the consolidated tape; nothing in the request
            // narrows it to a venue.
            feed: Feed::Consolidated,
            // `adjustment_type: "split"` at the call site, and deliberately not
            // total return — see the note there.
            adjustment: Adjustment::Split,
        }
    }

    fn max_days(&self, interval: BarInterval) -> Option<u32> {
        max_days(interval)
    }

    async fn connected(&self) -> Result<bool, SourceError> {
        is_connected()
    }

    async fn bars(
        &self,
        symbol: &str,
        interval: BarInterval,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Fetched, SourceError> {
        let client = connect().await?;
        let response = client
            .call_tool_json(
                HISTORICALS,
                json!({
                    "symbols": [symbol],
                    "start_time": format!("{from}T00:00:00Z"),
                    "end_time": format!("{to}T23:59:59Z"),
                    "interval": spelling(interval)?,
                    // Regular hours only. `periods_per_year` assumes a
                    // 390-minute session, so pulling extended hours would
                    // silently annualise every intraday statistic against the
                    // wrong session length.
                    "bounds": "regular",
                    // Split-adjusted. Raw prices make a split look like a
                    // crash, which a breakout rule would trade.
                    "adjustment_type": "split",
                }),
            )
            .await
            .map_err(transport)?;

        parse_bars(&response, symbol)
    }

    async fn search(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Match>, SourceError> {
        let client = connect().await?;
        let response = client
            .call_tool_json(
                SEARCH,
                json!({ "query": query, "limit": limit.clamp(1, 20) }),
            )
            .await
            .map_err(transport)?;

        let mut matches = parse_matches(&response, &arvo_data::source::existing_venues(root));
        if matches.is_empty() {
            return Ok(matches);
        }

        // Best effort. The prices are the garnish, not the dish: a search that
        // returned names is still a useful search, and failing the whole thing
        // because a quote lookup timed out would be the wrong trade.
        let symbols: Vec<&str> = matches.iter().map(|found| found.symbol.as_str()).collect();
        if let Ok(quotes) = client
            .call_tool_json(QUOTES, json!({ "symbols": symbols }))
            .await
        {
            let priced = parse_quotes(&quotes);
            for found in &mut matches {
                if let Some((price, change)) = priced.get(&found.symbol) {
                    found.price = Some(*price);
                    found.change = *change;
                }
            }
        }
        Ok(matches)
    }

    async fn quotes(&self, instruments: &[String]) -> Result<Vec<Quote>, SourceError> {
        if instruments.is_empty() {
            return Ok(Vec::new());
        }

        let client = connect().await?;
        let symbols: Vec<&str> = instruments
            .iter()
            .map(|id| arvo_data::source::symbol_of(id))
            .collect();
        let response = client
            .call_tool_json(QUOTES, json!({ "symbols": symbols }))
            .await
            .map_err(transport)?;
        let priced = parse_quotes(&response);

        // One request for the whole list rather than one per symbol: a
        // watchlist polling every few seconds would otherwise be a request per
        // row per tick. An instrument the feed does not price is simply absent
        // rather than an error — a watchlist with one bad ticker in it should
        // still show the other nine.
        Ok(instruments
            .iter()
            .filter_map(|instrument| {
                let (price, change) = priced.get(arvo_data::source::symbol_of(instrument))?;
                Some(Quote {
                    instrument: instrument.clone(),
                    price: *price,
                    change: *change,
                })
            })
            .collect())
    }
}

/// Arvo's interval spelling in Robinhood's vocabulary.
///
/// Not a formatting difference. Robinhood serves a fixed set and does not
/// aggregate, so asking for a three-minute bar gets a rejection rather than
/// something close — and its one-minute bar is named `minute`, not `1minute`,
/// which is exactly the sort of near-miss that would come back as data for the
/// wrong resolution if it were guessed.
/// The most bars one historicals request may be estimated at.
const BAR_CAP: u32 = 5_000;

/// The widest window Robinhood will accept at `interval`, in calendar days.
///
/// Its refusal says how it counts — "estimated 20435 bars exceeds cap of 5000"
/// for a year of five-minute bars is 262 weekdays times 78 — so this counts the
/// same way: bars in a 390-minute session, times weekdays, under the cap. Only
/// whole weeks are taken, so where a window starts cannot add a weekday to it.
fn max_days(interval: BarInterval) -> Option<u32> {
    const SESSION_SECONDS: u32 = 390 * 60;
    let step = interval.step.max(1);
    let bar_seconds = match interval.unit {
        IntervalUnit::Second => step,
        IntervalUnit::Minute => step * 60,
        IntervalUnit::Hour => step * 3_600,
        IntervalUnit::Day => SESSION_SECONDS,
        // A weekly bar is a fifth of a bar per weekday; the cap is years off.
        IntervalUnit::Week => return Some(BAR_CAP * 7 * step),
    };
    let per_weekday = SESSION_SECONDS.div_ceil(bar_seconds);
    let weekdays = BAR_CAP / per_weekday;
    Some(weekdays / 5 * 7)
}

fn spelling(interval: BarInterval) -> Result<String, SourceError> {
    let supported: &[(u32, IntervalUnit)] = &[
        (15, IntervalUnit::Second),
        (30, IntervalUnit::Second),
        (1, IntervalUnit::Minute),
        (5, IntervalUnit::Minute),
        (10, IntervalUnit::Minute),
        (30, IntervalUnit::Minute),
        (1, IntervalUnit::Hour),
        (4, IntervalUnit::Hour),
        (1, IntervalUnit::Day),
        (1, IntervalUnit::Week),
    ];
    if !supported.contains(&(interval.step, interval.unit)) {
        return Err(SourceError::Unsupported(format!(
            "{interval} is not one of the resolutions Robinhood serves, and it does not \
             aggregate — ask for a finer fixed interval instead"
        )));
    }

    let unit = match interval.unit {
        IntervalUnit::Second => "second",
        IntervalUnit::Minute => "minute",
        IntervalUnit::Hour => "hour",
        IntervalUnit::Day => "day",
        IntervalUnit::Week => "week",
    };
    Ok(if interval.step == 1 {
        unit.to_owned()
    } else {
        format!("{}{unit}", interval.step)
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_default_window_never_asks_for_more_than_robinhood_estimates_it_allows() {
        // It counts weekdays times bars per session, and refuses above 5,000:
        // "estimated 20435 bars exceeds cap of 5000" for a year of five-minute.
        for (step, unit) in [
            (15, IntervalUnit::Second),
            (1, IntervalUnit::Minute),
            (5, IntervalUnit::Minute),
            (30, IntervalUnit::Minute),
            (1, IntervalUnit::Hour),
            (4, IntervalUnit::Hour),
            (1, IntervalUnit::Day),
        ] {
            let interval = BarInterval::new(step, unit);
            let days = max_days(interval).expect("capped");
            let per_session = (390 * 60_u32)
                .div_ceil(u32::try_from(interval.duration().num_seconds()).expect("small"))
                .max(1);
            // Whole weeks, so exactly this many weekdays whatever day it starts.
            let estimated = days / 7 * 5 * per_session;
            assert!(estimated <= BAR_CAP, "{interval}: {days} days is {estimated} estimated bars");
        }
        assert_eq!(max_days(BarInterval::new(5, IntervalUnit::Minute)), Some(84));
    }

    use super::*;

    #[test]
    fn an_unserved_resolution_is_refused_rather_than_rounded() {
        // Robinhood does not aggregate, so a near-miss would come back as data
        // for the wrong resolution.
        assert!(matches!(
            spelling(BarInterval::new(3, IntervalUnit::Minute)),
            Err(SourceError::Unsupported(_))
        ));
    }

    #[test]
    fn a_one_step_interval_drops_the_one() {
        // Its one-minute bar is named `minute`, not `1minute`.
        assert_eq!(spelling(BarInterval::DAILY).unwrap(), "day");
        assert_eq!(
            spelling(BarInterval::new(1, IntervalUnit::Minute)).unwrap(),
            "minute"
        );
        assert_eq!(
            spelling(BarInterval::new(5, IntervalUnit::Minute)).unwrap(),
            "5minute"
        );
    }

    #[tokio::test]
    async fn this_source_does_not_claim_to_serve_dividends() {
        // An empty list would be a claim that the instrument paid nothing.
        let err = Robinhood
            .dividends(
                "MSFT",
                chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SourceError::Unoffered {
                what: "dividends",
                ..
            }
        ));
    }
}
