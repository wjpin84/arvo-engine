//! Bars, search and quotes from Robinhood's MCP server.
//!
//! Was `crate::feed`. What moved out of it is everything that was not about
//! Robinhood — the inspect/compare/write pipeline now lives in
//! [`arvo_data::source::ingest`], shared with every other source. What stayed is the four
//! things that are genuinely this vendor's: the endpoint, the OAuth, the
//! interval spelling, and the shape of the reply.
//!
//! # Signing in
//!
//! OAuth 2.1 with PKCE and dynamic client registration, via [`arvo_oauth`]. No
//! pasted token, and nothing about Robinhood is embedded beyond the endpoint —
//! every URL comes from the server's own discovery document.
//!
//! What is stored is a client id and a token pair, in the OS keychain. The
//! access token is refreshed on use when it has run out, and the refreshed pair
//! is written back, so an unattended sync survives an expiry.
//!
//! # What is deliberately not here
//!
//! Only the three read tools below are called, and only those. The same server
//! offers `place_equity_order`; live execution with real capital is a separate,
//! later, explicit decision and not a matter of which tool name a function
//! happens to pass.
//!
//! The scope asked for is whatever the server advertises. That is not a way of
//! asking for more than is needed — it is one scope, `internal`, and the same
//! one the endpoint requires for any call at all — but it is worth knowing that
//! the token this holds could place an order if something asked it to. Nothing
//! does, and that is a property of the code above rather than of the token.
//!
//! # No dividends
//!
//! [`Source::dividends`] is left at its default, which reports that this vendor
//! does not serve them. The MCP server exposes no distribution tool, and
//! reporting an empty list would be a claim that the instrument paid nothing.
//! Fetch dividend-sensitive work from `arvo_yfinance`, which does serve them.

pub mod execution;

use std::path::Path;

use arvo_data::{Bar, BarInterval, IntervalUnit};
use serde_json::{json, Value};

use arvo_data::source::{Adjustment, Basis, Feed, Fetched, Match, Quote, Source, SourceError};

/// Where the token lives in the OS keychain, and how this source is named.
pub const SOURCE_ID: &str = "robinhood";

/// The venue an instrument fetched here is filed under. See [`Source::venue`].
pub const VENUE: &str = "RH";

/// Robinhood's MCP endpoint.
const ENDPOINT: &str = "https://agent.robinhood.com/mcp/trading";

/// The tools this module calls. All three read; the ones that trade live in
/// [`execution`], behind `arvo_execution::Session`.
const HISTORICALS: &str = "get_equity_historicals";
const SEARCH: &str = "search";
const QUOTES: &str = "get_equity_quotes";

/// What is kept in the keychain once someone has signed in.
///
/// The client id travels with the tokens rather than being registered afresh
/// each time: dynamic registration works on every sign-in, but it leaves one
/// abandoned client on the server per attempt, and reusing the id is what makes
/// a re-authorisation look like the same app coming back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Connection {
    client_id: String,
    tokens: arvo_oauth::Tokens,
}

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

#[async_trait::async_trait]
impl Source for Robinhood {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn label(&self) -> &'static str {
        "Robinhood"
    }

    fn venue(&self) -> &'static str {
        VENUE
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

    async fn connected(&self) -> Result<bool, SourceError> {
        Ok(stored()?.is_some())
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

/// An MCP client with a fresh token, already through `initialize`.
pub(crate) async fn connect() -> Result<arvo_mcp::McpClient, SourceError> {
    let token = access_token().await?;
    let client = arvo_mcp::McpClient::new(ENDPOINT, token);
    client.connect().await.map_err(transport)?;
    Ok(client)
}

/// An MCP failure as a [`SourceError`].
///
/// The one classification that matters happens here: `Unauthorized` is a dead
/// session and everything else is the network. Doing it at the boundary is why
/// no caller has to know what an `arvo_mcp::ClientError` is.
fn transport(err: arvo_mcp::ClientError) -> SourceError {
    match err {
        arvo_mcp::ClientError::Unauthorized { .. } => SourceError::NoSession { vendor: SOURCE_ID },
        other => SourceError::Transport {
            vendor: SOURCE_ID,
            detail: other.to_string(),
        },
    }
}

/// An OAuth failure as a [`SourceError`].
///
/// `Denied` means the authorization server refused the grant, which a person
/// fixes by signing in again. Every other OAuth error is a refresh that could
/// not *reach* the server — a network problem wearing an auth error's clothing,
/// and reporting it as a dead session would sign someone out over flaky wifi.
fn oauth(err: arvo_oauth::OAuthError) -> SourceError {
    match err {
        arvo_oauth::OAuthError::Denied { .. } => SourceError::NoSession { vendor: SOURCE_ID },
        other => SourceError::Transport {
            vendor: SOURCE_ID,
            detail: other.to_string(),
        },
    }
}

fn malformed(detail: impl Into<String>) -> SourceError {
    SourceError::Malformed {
        vendor: SOURCE_ID,
        detail: detail.into(),
    }
}

/// Arvo's interval spelling in Robinhood's vocabulary.
///
/// Not a formatting difference. Robinhood serves a fixed set and does not
/// aggregate, so asking for a three-minute bar gets a rejection rather than
/// something close — and its one-minute bar is named `minute`, not `1minute`,
/// which is exactly the sort of near-miss that would come back as data for the
/// wrong resolution if it were guessed.
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

/// Turns one response into bars, dropping the ones the server invented.
///
/// Interpolated bars are gap-fill the server synthesised to keep a series
/// contiguous; the tool's own guide says they carry no new information. Feeding
/// one to a strategy would be feeding it a price nobody traded at, and a
/// breakout rule cannot tell the difference.
fn parse_bars(response: &Value, symbol: &str) -> Result<Fetched, SourceError> {
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

fn parse_matches(
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

/// Last price and the move since the previous close, by symbol.
fn parse_quotes(response: &Value) -> std::collections::HashMap<String, (f64, Option<f64>)> {
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

/// Whether a connection is stored, without revealing anything about it.
///
/// A getter for the token itself would put a bearer credential on the wire to
/// the web view for no reason a UI actually has — the only thing a UI needs to
/// know is whether to offer sign-in or sign-out.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain cannot be read.
pub fn is_connected() -> Result<bool, SourceError> {
    Ok(stored()?.is_some())
}

/// Forgets the stored connection.
///
/// Local only. Whether the tokens are also revoked at the server is the
/// server's business and there is no revocation endpoint in its metadata, so
/// this does not claim to have done more than it did.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the delete.
pub fn disconnect() -> Result<(), SourceError> {
    arvo_core::secrets::delete_token(SOURCE_ID).map_err(credential)
}

/// Starts a sign-in: discovers the server, registers, and returns the URL to
/// open along with the pending flow.
///
/// Returning the URL rather than opening it keeps `arvo-oauth` free of a
/// browser dependency, and keeps *this* function testable up to the point a
/// person is actually required.
///
/// # Errors
///
/// Returns [`SourceError`] if discovery or registration fails.
pub async fn begin_sign_in() -> Result<arvo_oauth::Pending, SourceError> {
    // Reuse the client id from a previous sign-in when there is one, even if
    // its tokens have since expired or been revoked — the registration is still
    // good, and re-registering would strand it.
    let client_id = stored().ok().flatten().map(|held| held.client_id);
    arvo_oauth::begin(&arvo_oauth::AuthConfig {
        resource: ENDPOINT.to_owned(),
        client_name: "Arvo".to_owned(),
        // Empty: take whatever the server advertises rather than asserting a
        // scope name that may not exist. Asking for one it does not know is a
        // refusal, and asking for more than it offers is worse.
        scopes: Vec::new(),
        client_id,
    })
    .await
    .map_err(oauth)
}

/// Waits for the browser redirect and stores what comes back.
///
/// # Errors
///
/// Returns [`SourceError`] if consent is refused or nobody finishes.
pub async fn complete_sign_in(pending: arvo_oauth::Pending) -> Result<(), SourceError> {
    let client_id = pending.client_id.clone();
    let tokens = pending
        .finish(arvo_oauth::DEFAULT_TIMEOUT)
        .await
        .map_err(oauth)?;
    store(&Connection { client_id, tokens })
}

/// A usable access token, refreshed if the stored one has run out.
///
/// The refreshed pair is written back before it is used. Refreshing without
/// storing works exactly once and then asks for a browser again, which is the
/// sort of bug that only shows up an hour after someone stops watching.
///
/// # Errors
///
/// Returns [`SourceError::NoSession`] if nobody has signed in or the refresh
/// token has been revoked — in which case a person has to sign in again.
pub async fn access_token() -> Result<String, SourceError> {
    let held = stored()?.ok_or(SourceError::NoSession { vendor: SOURCE_ID })?;
    if !held.tokens.is_expired(std::time::SystemTime::now()) {
        return Ok(held.tokens.access_token);
    }

    let refresh_token = held
        .tokens
        .refresh_token
        .as_deref()
        .ok_or(SourceError::NoSession { vendor: SOURCE_ID })?;

    // The token endpoint again from discovery rather than remembered: an
    // endpoint cached at sign-in and moved since would fail every refresh with
    // no way to recover but a reinstall.
    let metadata_url = arvo_oauth::metadata_url(ENDPOINT).map_err(oauth)?;
    let document: serde_json::Value = reqwest::Client::new()
        .get(metadata_url)
        .send()
        .await
        .map_err(|err| oauth(arvo_oauth::OAuthError::Http(err)))?
        .json()
        .await
        .map_err(|err| oauth(arvo_oauth::OAuthError::Http(err)))?;
    let metadata = arvo_oauth::ServerMetadata::parse(&document).map_err(oauth)?;

    let tokens = arvo_oauth::refresh(
        &metadata.token_endpoint,
        &held.client_id,
        refresh_token,
        ENDPOINT,
    )
    .await
    .map_err(oauth)?;

    let access = tokens.access_token.clone();
    store(&Connection {
        client_id: held.client_id,
        tokens,
    })?;
    Ok(access)
}

fn credential(err: impl std::fmt::Display) -> SourceError {
    SourceError::Credential {
        vendor: SOURCE_ID,
        detail: err.to_string(),
    }
}

fn stored() -> Result<Option<Connection>, SourceError> {
    let Some(text) = arvo_core::secrets::get_token(SOURCE_ID).map_err(credential)? else {
        return Ok(None);
    };
    serde_json::from_str(&text).map(Some).map_err(credential)
}

fn store(connection: &Connection) -> Result<(), SourceError> {
    let text = serde_json::to_string(connection).map_err(credential)?;
    arvo_core::secrets::store_token(SOURCE_ID, &text).map_err(credential)
}

#[cfg(test)]
mod tests {
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
    fn a_readable_name_is_preferred_over_the_legal_one() {
        let body = json!({ "data": { "results": [
            { "symbol": "MSFT", "simple_name": "Microsoft", "name": "Microsoft Corp. Common Stock" },
            { "symbol": "NVDA", "name": "NVIDIA Corporation" },
        ]}});
        let found = parse_matches(&body, &std::collections::HashMap::new());
        assert_eq!(found[0].name, "Microsoft");
        assert_eq!(found[1].name, "NVIDIA Corporation", "falls back to exact");
    }

    #[test]
    fn a_refused_token_is_a_dead_session_and_a_reset_connection_is_not() {
        // The distinction the whole error mapping exists for: signing someone
        // out over flaky wifi would make them re-authorize in a browser to fix
        // a network blip.
        assert!(transport(arvo_mcp::ClientError::Unauthorized { status: 401 }).needs_sign_in());
        assert!(!oauth(arvo_oauth::OAuthError::Metadata("token_endpoint"))
            .needs_sign_in());
        assert!(oauth(arvo_oauth::OAuthError::Denied {
            error: "invalid_grant".into(),
            description: None,
        })
        .needs_sign_in());
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
