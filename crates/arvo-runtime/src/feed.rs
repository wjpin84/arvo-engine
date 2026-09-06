//! Pulling bars from Robinhood's MCP server into the local data library.
//!
//! # Why this fetches into files rather than being a `BarProvider`
//!
//! A live provider looks tidier and would quietly destroy the thing this
//! platform is for. An experiment pins its dataset as a **content hash of the
//! bars it ran on**; that is what makes a stored finding reproducible and what
//! makes staleness detectable. A provider that reaches over the network gives
//! a different answer whenever the vendor revises a bar, so the hash would
//! describe nothing and every stored verdict would silently stop being
//! checkable.
//!
//! Two lesser reasons point the same way. A panel is dozens of backtests over
//! the same bars, so a live provider would re-fetch the same series dozens of
//! times against a rate limit. And a network blip would fail a *backtest*,
//! which is a bad place to discover the internet is down.
//!
//! So: fetch is an explicit act that writes files, and everything downstream
//! keeps reading [`arvo_data::CsvBars`].
//!
//! # Signing in
//!
//! OAuth 2.1 with PKCE and dynamic client registration, via [`arvo_oauth`]. No
//! pasted token, and nothing about Robinhood is embedded beyond the endpoint —
//! every URL comes from the server's own discovery document.
//!
//! What is stored is a client id and a token pair, in the OS keychain. The
//! access token is refreshed on use when it has run out, and the refreshed
//! pair is written back, so an unattended sync survives an expiry.
//!
//! # What is deliberately not here
//!
//! Only `get_equity_historicals` is called, and only that. The same server
//! offers `place_equity_order`; live execution with real capital is a
//! separate, later, explicit decision and not a matter of which tool name a
//! function happens to pass.
//!
//! The scope asked for is whatever the server advertises. That is not a way of
//! asking for more than is needed — it is one scope, `internal`, and the same
//! one the endpoint requires for any call at all — but it is worth knowing
//! that the token this holds could place an order if something asked it to.
//! Nothing does, and that is a property of the code above rather than of the
//! token.

use std::path::PathBuf;

use arvo_data::{Bar, BarInterval, CsvBars, IntervalUnit};
use serde_json::{json, Value};

/// Where the token lives in the OS keychain.
pub const FEED_ID: &str = "robinhood";

/// Robinhood's MCP endpoint.
const ENDPOINT: &str = "https://agent.robinhood.com/mcp/trading";

/// The only tool this module calls.
const HISTORICALS: &str = "get_equity_historicals";

/// What is kept in the keychain once someone has signed in.
///
/// The client id travels with the tokens rather than being registered afresh
/// each time: dynamic registration works on every sign-in, but it leaves one
/// abandoned client on the server per attempt, and reusing the id is what
/// makes a re-authorisation look like the same app coming back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Connection {
    client_id: String,
    tokens: arvo_oauth::Tokens,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("not connected to {FEED_ID}; sign in before fetching")]
    NoToken,
    #[error("signing in to {FEED_ID}: {0}")]
    OAuth(#[from] arvo_oauth::OAuthError),
    #[error("the stored {FEED_ID} connection could not be read: {0}")]
    Stored(String),
    #[error("reading the stored token: {0}")]
    Secrets(#[from] arvo_core::secrets::SecretsError),
    #[error("talking to {FEED_ID}: {0}")]
    Transport(#[from] arvo_mcp::ClientError),
    #[error("{0}")]
    Unsupported(String),
    #[error("{FEED_ID} returned no bars for {instrument} at {interval}")]
    Empty { instrument: String, interval: String },
    #[error("{FEED_ID} sent something this does not understand: {0}")]
    Malformed(String),
    #[error("writing bars: {0}")]
    Write(#[from] arvo_data::DataError),
}

/// What one fetch did, so the caller can say so rather than just succeeding.
#[derive(Debug, Clone)]
pub struct FetchReport {
    pub instrument: String,
    pub interval: BarInterval,
    pub bars: usize,
    /// Gap-fill bars the server synthesised, which were dropped. Reported
    /// rather than hidden: a series that is a quarter invented is one to know
    /// about before drawing a conclusion from it.
    pub interpolated: usize,
    pub from: Option<chrono::NaiveDateTime>,
    pub to: Option<chrono::NaiveDateTime>,
    pub path: PathBuf,
}

/// Fetches one instrument's history and writes it into the library.
///
/// # Errors
///
/// Returns [`FeedError`] if there is no stored token, the call fails, the
/// resolution is one Robinhood does not serve, or nothing comes back.
pub async fn fetch(
    root: &std::path::Path,
    instrument: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<FetchReport, FeedError> {
    let token = access_token().await?;
    let symbol = symbol_of(instrument);
    let client = arvo_mcp::McpClient::new(ENDPOINT, token);
    client.connect().await?;

    let response = client
        .call_tool_json(
            HISTORICALS,
            json!({
                "symbols": [symbol],
                "start_time": format!("{from}T00:00:00Z"),
                "end_time": format!("{to}T23:59:59Z"),
                "interval": robinhood_interval(interval)?,
                // Regular hours only. `periods_per_year` assumes a 390-minute
                // session, so pulling extended hours would silently annualise
                // every intraday statistic against the wrong session length.
                "bounds": "regular",
                // Split-adjusted. Raw prices make a split look like a crash,
                // which a breakout rule would trade.
                "adjustment_type": "split",
            }),
        )
        .await?;

    let (bars, interpolated) = parse_bars(&response, symbol)?;
    if bars.is_empty() {
        return Err(FeedError::Empty {
            instrument: instrument.to_owned(),
            interval: interval.to_string(),
        });
    }

    let path = CsvBars::new(root).write(instrument, interval, &bars)?;
    Ok(FetchReport {
        instrument: instrument.to_owned(),
        interval,
        bars: bars.len(),
        interpolated,
        from: bars.first().map(|bar| bar.at),
        to: bars.last().map(|bar| bar.at),
        path,
    })
}

/// The ticker out of an Arvo instrument id: `MSFT.NASDAQ` is `MSFT`.
fn symbol_of(instrument: &str) -> &str {
    instrument.split('.').next().unwrap_or(instrument)
}

/// Arvo's interval spelling in Robinhood's vocabulary.
///
/// Not a formatting difference. Robinhood serves a fixed set and does not
/// aggregate, so asking for a three-minute bar gets a rejection rather than
/// something close — and its one-minute bar is named `minute`, not `1minute`,
/// which is exactly the sort of near-miss that would come back as data for
/// the wrong resolution if it were guessed.
fn robinhood_interval(interval: BarInterval) -> Result<String, FeedError> {
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
        return Err(FeedError::Unsupported(format!(
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
/// Returns the bars and how many were dropped. Interpolated bars are gap-fill
/// the server synthesised to keep a series contiguous; the tool's own guide
/// says they carry no new information. Feeding one to a strategy would be
/// feeding it a price nobody traded at, and a breakout rule cannot tell the
/// difference.
fn parse_bars(response: &Value, symbol: &str) -> Result<(Vec<Bar>, usize), FeedError> {
    let results = response
        .pointer("/data/results")
        .and_then(Value::as_array)
        .ok_or_else(|| FeedError::Malformed("no data.results array".to_owned()))?;

    let result = results
        .iter()
        .find(|result| result.get("symbol").and_then(Value::as_str) == Some(symbol))
        .ok_or_else(|| FeedError::Malformed(format!("no result for {symbol}")))?;

    let raw = result
        .get("bars")
        .and_then(Value::as_array)
        .ok_or_else(|| FeedError::Malformed("no bars array".to_owned()))?;

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
    Ok((bars, interpolated))
}

fn one_bar(bar: &Value) -> Result<Bar, FeedError> {
    // Prices arrive as strings — `"501.990000"` — so they cannot be read as
    // numbers, and treating a missing one as zero would put a bar at zero
    // into a price series.
    let price = |field: &str| -> Result<f64, FeedError> {
        bar.get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| FeedError::Malformed(format!("bar has no {field}")))?
            .parse::<f64>()
            .map_err(|err| FeedError::Malformed(format!("{field}: {err}")))
    };

    let begins_at = bar
        .get("begins_at")
        .and_then(Value::as_str)
        .ok_or_else(|| FeedError::Malformed("bar has no begins_at".to_owned()))?;
    // Left-edge labelled in UTC — the instant the bar *opens*, which is what
    // `arvo_data::Bar::at` means. The engine boundary adds the interval to get
    // the close, so a right-edge reading here would shift every bar by one
    // period and hand strategies a bar before it existed.
    let at = chrono::DateTime::parse_from_rfc3339(begins_at)
        .map_err(|err| FeedError::Malformed(format!("begins_at {begins_at:?}: {err}")))?
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

/// Whether a connection is stored, without revealing anything about it.
///
/// A getter for the token itself would put a bearer credential on the wire to
/// the web view for no reason a UI actually has — the only thing a UI needs to
/// know is whether to offer sign-in or sign-out.
///
/// # Errors
///
/// Returns [`FeedError::Secrets`] if the keychain cannot be read.
pub fn is_connected() -> Result<bool, FeedError> {
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
/// Returns [`FeedError::Secrets`] if the keychain rejects the delete.
pub fn disconnect() -> Result<(), FeedError> {
    arvo_core::secrets::delete_token(FEED_ID)?;
    Ok(())
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
/// Returns [`FeedError::OAuth`] if discovery or registration fails.
pub async fn begin_sign_in() -> Result<arvo_oauth::Pending, FeedError> {
    // Reuse the client id from a previous sign-in when there is one, even if
    // its tokens have since expired or been revoked — the registration is
    // still good, and re-registering would strand it.
    let client_id = stored().ok().flatten().map(|held| held.client_id);
    Ok(arvo_oauth::begin(&arvo_oauth::AuthConfig {
        resource: ENDPOINT.to_owned(),
        client_name: "Arvo".to_owned(),
        // Empty: take whatever the server advertises rather than asserting a
        // scope name that may not exist. Asking for one it does not know is a
        // refusal, and asking for more than it offers is worse.
        scopes: Vec::new(),
        client_id,
    })
    .await?)
}

/// Waits for the browser redirect and stores what comes back.
///
/// # Errors
///
/// Returns [`FeedError::OAuth`] if consent is refused or nobody finishes.
pub async fn complete_sign_in(pending: arvo_oauth::Pending) -> Result<(), FeedError> {
    let client_id = pending.client_id.clone();
    let tokens = pending.finish(arvo_oauth::DEFAULT_TIMEOUT).await?;
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
/// Returns [`FeedError::NoToken`] if nobody has signed in, or
/// [`FeedError::OAuth`] if the refresh token has been revoked — in which case
/// a person has to sign in again.
pub async fn access_token() -> Result<String, FeedError> {
    let held = stored()?.ok_or(FeedError::NoToken)?;
    if !held.tokens.is_expired(std::time::SystemTime::now()) {
        return Ok(held.tokens.access_token);
    }

    let refresh_token = held
        .tokens
        .refresh_token
        .as_deref()
        .ok_or(FeedError::NoToken)?;

    // The token endpoint again from discovery rather than remembered: an
    // endpoint cached at sign-in and moved since would fail every refresh
    // with no way to recover but a reinstall.
    let document: serde_json::Value = reqwest::Client::new()
        .get(arvo_oauth::metadata_url(ENDPOINT).map_err(FeedError::OAuth)?)
        .send()
        .await
        .map_err(|err| FeedError::OAuth(arvo_oauth::OAuthError::Http(err)))?
        .json()
        .await
        .map_err(|err| FeedError::OAuth(arvo_oauth::OAuthError::Http(err)))?;
    let metadata = arvo_oauth::ServerMetadata::parse(&document)?;

    let tokens = arvo_oauth::refresh(
        &metadata.token_endpoint,
        &held.client_id,
        refresh_token,
        ENDPOINT,
    )
    .await?;

    let access = tokens.access_token.clone();
    store(&Connection {
        client_id: held.client_id,
        tokens,
    })?;
    Ok(access)
}

fn stored() -> Result<Option<Connection>, FeedError> {
    let Some(text) = arvo_core::secrets::get_token(FEED_ID)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| FeedError::Stored(err.to_string()))
}

fn store(connection: &Connection) -> Result<(), FeedError> {
    let text = serde_json::to_string(connection)
        .map_err(|err| FeedError::Stored(err.to_string()))?;
    arvo_core::secrets::store_token(FEED_ID, &text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real response, trimmed. Captured from the live endpoint rather than
    /// invented: the fields that matter here are the ones a guess gets wrong —
    /// prices as strings, `begins_at` as the left edge, and `interpolated`.
    fn response() -> Value {
        serde_json::from_str(
            r#"{"data":{"results":[{"symbol":"MSFT","interval":"5minute","bounds":"regular",
            "bars":[
            {"begins_at":"2026-09-03T13:30:00Z","open_price":"501.990000","close_price":"508.490000",
             "high_price":"509.610000","low_price":"500.800000","volume":715524,"session":"reg"},
            {"begins_at":"2026-09-03T13:35:00Z","open_price":"508.450700","close_price":"511.130000",
             "high_price":"511.330000","low_price":"508.050300","volume":308447,"session":"reg"},
            {"begins_at":"2026-09-03T13:40:00Z","open_price":"511.220000","close_price":"510.705000",
             "high_price":"511.695000","low_price":"509.955000","volume":0,"session":"reg",
             "interpolated":true}
            ]}]}}"#,
        )
        .expect("valid fixture")
    }

    #[test]
    fn prices_arrive_as_strings_and_are_read_as_numbers() {
        let (bars, _) = parse_bars(&response(), "MSFT").expect("parses");
        let first = bars.first().expect("two real bars");
        assert!((first.open - 501.99).abs() < 1e-9);
        assert!((first.high - 509.61).abs() < 1e-9);
        assert!((first.low - 500.80).abs() < 1e-9);
        assert!((first.close - 508.49).abs() < 1e-9);
        assert!((first.volume - 715_524.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_bar_is_timestamped_where_it_opens() {
        // `begins_at` is the left edge. Reading it as the close would shift
        // every bar by one period and hand a strategy a price before it
        // existed — the exact look-ahead this platform exists to catch.
        let (bars, _) = parse_bars(&response(), "MSFT").expect("parses");
        assert_eq!(
            bars[0].at,
            chrono::NaiveDate::from_ymd_opt(2026, 9, 3)
                .expect("valid")
                .and_hms_opt(13, 30, 0)
                .expect("valid")
        );
    }

    #[test]
    fn invented_bars_are_dropped_and_counted() {
        // The server synthesises gap-fill bars and says so. A breakout rule
        // cannot tell an invented price from a traded one.
        let (bars, interpolated) = parse_bars(&response(), "MSFT").expect("parses");
        assert_eq!(bars.len(), 2);
        assert_eq!(interpolated, 1);
    }

    #[test]
    fn a_response_for_another_symbol_is_not_accepted_as_this_one() {
        let err = parse_bars(&response(), "AAPL").expect_err("wrong symbol");
        assert!(matches!(err, FeedError::Malformed(_)), "{err}");
    }

    #[test]
    fn robinhood_names_the_one_minute_bar_minute() {
        // Not a formatting quirk to smooth over: `1minute` is rejected, and a
        // near-miss that silently returned another resolution would be worse.
        assert_eq!(
            robinhood_interval(BarInterval::new(1, IntervalUnit::Minute)).expect("supported"),
            "minute"
        );
        assert_eq!(
            robinhood_interval(BarInterval::new(5, IntervalUnit::Minute)).expect("supported"),
            "5minute"
        );
        assert_eq!(
            robinhood_interval(BarInterval::DAILY).expect("supported"),
            "day"
        );
    }

    #[test]
    fn a_resolution_robinhood_does_not_serve_is_refused() {
        // It does not aggregate, so asking for three minutes gets nothing
        // rather than something close.
        for interval in [
            BarInterval::new(3, IntervalUnit::Minute),
            BarInterval::new(2, IntervalUnit::Hour),
            BarInterval::new(2, IntervalUnit::Day),
        ] {
            assert!(
                robinhood_interval(interval).is_err(),
                "{interval} should be refused"
            );
        }
    }

    #[test]
    fn an_instrument_id_yields_its_ticker() {
        assert_eq!(symbol_of("MSFT.NASDAQ"), "MSFT");
        assert_eq!(symbol_of("MSFT"), "MSFT");
    }
}
