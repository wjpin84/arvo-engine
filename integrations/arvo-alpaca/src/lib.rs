//! Bars and distributions from Alpaca.
//!
//! # What this is here for
//!
//! The dividends, mostly. Alpaca's corporate-actions endpoint is a documented,
//! supported distribution feed with ex-date, rate and payable date — the first
//! proper one in this codebase. Yahoo's `events=div` is a chart-endpoint side
//! effect by comparison, and Robinhood serves none at all.
//!
//! # The free tier is IEX, and that is a backtest problem
//!
//! Alpaca's free plan serves only IEX for equities. IEX is a low-single-digit
//! share of consolidated volume, so free-tier bars are a thin sample of the
//! tape rather than the tape: the prices are real, the volumes are not, and
//! anything conditioned on size is reading a different market. `vwap_reversion`
//! computed from IEX prints is not VWAP.
//!
//! That is invisible to a cross-check, because the prices agree — which is
//! exactly why [`Source::basis`] exists and why [`Alpaca::iex`] declares
//! [`Feed::SingleVenue`]. A comparison against the broker will say so rather
//! than reporting `Aligned` and leaving it there.
//!
//! SIP costs $99 a month. Against a small account that is a hurdle a strategy
//! has to clear before it earns anything, so the two feeds are separate
//! constructors filing under separate venues: `AAPL.AIEX` and `AAPL.ASIP` are
//! two datasets with two content hashes, and letting them share a name would
//! let a study silently run on whichever was fetched last.
//!
//! # Credentials
//!
//! An API key id and secret, in the OS keychain, sent as headers. No OAuth —
//! Alpaca issues keys directly, so there is no flow to run and nothing to
//! refresh.
//!
//! The environment is read as a fallback so the headless examples work before
//! any UI exists to paste a key into. `APCA_API_KEY_ID` and
//! `APCA_API_SECRET_KEY` are Alpaca's own names, which is what every one of
//! their SDKs already reads.
//!
//! # What this does not implement
//!
//! [`Source::search`] and [`Source::quotes`] stay at their trait defaults,
//! which report that this vendor does not offer them rather than returning an
//! empty list. Alpaca has both, and nothing here needs them yet — the broker
//! already answers the search box.

use arvo_data::{Bar, BarInterval, Dividend, IntervalUnit};
use serde_json::Value;

use arvo_data::source::{Adjustment, Basis, Feed, Fetched, Source, SourceError};

/// Where the key pair lives in the OS keychain.
pub const CREDENTIAL_ID: &str = "alpaca";

const DATA: &str = "https://data.alpaca.markets";

/// The most bars one request returns. Alpaca's own ceiling.
const PAGE: usize = 10_000;

/// The free plan's feed, and its venue.
pub const IEX_SOURCE_ID: &str = "alpaca-iex";
pub const IEX_VENUE: &str = "AIEX";

/// The paid plan's feed, and its venue.
pub const SIP_SOURCE_ID: &str = "alpaca-sip";
pub const SIP_VENUE: &str = "ASIP";

/// An Alpaca key pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Keys {
    pub key_id: String,
    pub secret: String,
}

/// The Alpaca source, on one feed or the other.
///
/// Which feed is part of the source's identity rather than a setting, because
/// it is part of the *data's* identity: see the module note on venues.
pub struct Alpaca {
    feed: &'static str,
    id: &'static str,
    venue: &'static str,
}

impl Alpaca {
    /// The free plan: IEX only. See the module note before backtesting on it.
    #[must_use]
    pub const fn iex() -> Self {
        Self {
            feed: "iex",
            id: IEX_SOURCE_ID,
            venue: IEX_VENUE,
        }
    }

    /// The paid plan: every US exchange.
    #[must_use]
    pub const fn sip() -> Self {
        Self {
            feed: "sip",
            id: SIP_SOURCE_ID,
            venue: SIP_VENUE,
        }
    }
}

/// The stored key pair, or the environment, or nothing.
///
/// Keychain first: the environment is a convenience for headless runs, not the
/// intended home for a credential.
fn keys() -> Result<Option<Keys>, SourceError> {
    if let Some(text) = arvo_core::secrets::get_token(CREDENTIAL_ID).map_err(credential)? {
        return serde_json::from_str(&text).map(Some).map_err(credential);
    }
    let (Ok(key_id), Ok(secret)) = (
        std::env::var("APCA_API_KEY_ID"),
        std::env::var("APCA_API_SECRET_KEY"),
    ) else {
        return Ok(None);
    };
    Ok(Some(Keys { key_id, secret }))
}

/// Stores a key pair.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the write.
pub fn store(keys: &Keys) -> Result<(), SourceError> {
    let text = serde_json::to_string(keys).map_err(credential)?;
    arvo_core::secrets::store_token(CREDENTIAL_ID, &text).map_err(credential)
}

/// Forgets the stored key pair.
///
/// # Errors
///
/// Returns [`SourceError::Credential`] if the keychain rejects the delete.
pub fn forget() -> Result<(), SourceError> {
    arvo_core::secrets::delete_token(CREDENTIAL_ID).map_err(credential)
}

fn credential(err: impl std::fmt::Display) -> SourceError {
    SourceError::Credential {
        vendor: "alpaca",
        detail: err.to_string(),
    }
}

#[async_trait::async_trait]
impl Source for Alpaca {
    fn id(&self) -> &'static str {
        self.id
    }

    fn label(&self) -> &'static str {
        if self.feed == "iex" {
            "Alpaca (IEX, free)"
        } else {
            "Alpaca (all exchanges)"
        }
    }

    fn venue(&self) -> &'static str {
        self.venue
    }

    fn basis(&self) -> Basis {
        Basis {
            // The declaration this source exists to make. IEX prices agree with
            // the tape and IEX volume does not, and no check on the bars can
            // tell you which you have.
            feed: if self.feed == "iex" {
                Feed::SingleVenue("IEX")
            } else {
                Feed::Consolidated
            },
            // `adjustment=split`, matching the other two sources. Alpaca's
            // default is `raw`, which would make every split read as a crash —
            // so it is passed explicitly at the call site.
            adjustment: Adjustment::Split,
        }
    }

    async fn connected(&self) -> Result<bool, SourceError> {
        Ok(keys()?.is_some())
    }

    async fn bars(
        &self,
        symbol: &str,
        interval: BarInterval,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Fetched, SourceError> {
        let mut bars = Vec::new();
        let mut page: Option<String> = None;

        // Paginated because one request caps at 10,000 bars and a decade of
        // daily data is more than that at intraday resolutions. Looping until
        // the token is absent rather than a fixed number of times: a partial
        // series that looked complete is the failure this avoids.
        loop {
            let mut url = format!(
                "{DATA}/v2/stocks/bars?symbols={symbol}&timeframe={}&start={from}&end={to}\
                 &adjustment=split&feed={}&limit={PAGE}",
                spelling(interval)?,
                self.feed,
            );
            if let Some(token) = &page {
                url.push_str(&format!("&page_token={token}"));
            }

            let body = get(&url).await?;
            bars.extend(parse_bars(&body, symbol)?);

            page = body
                .get("next_page_token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if page.is_none() {
                break;
            }
        }

        bars.sort_by_key(|bar| bar.at);
        bars.dedup_by_key(|bar| bar.at);
        Ok(Fetched {
            bars,
            // Alpaca returns bars it has and omits the rest; nothing is
            // synthesised, so there is nothing invented to count.
            interpolated: 0,
        })
    }

    async fn dividends(
        &self,
        symbol: &str,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<Dividend>, SourceError> {
        let mut paid = Vec::new();
        let mut page: Option<String> = None;

        loop {
            let mut url = format!(
                "{DATA}/v1/corporate-actions?symbols={symbol}&types=cash_dividend\
                 &start={from}&end={to}&limit=1000"
            );
            if let Some(token) = &page {
                url.push_str(&format!("&page_token={token}"));
            }

            let body = get(&url).await?;
            paid.extend(parse_dividends(&body));

            page = body
                .get("next_page_token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if page.is_none() {
                break;
            }
        }

        paid.sort_by_key(|dividend| dividend.ex_date);
        Ok(paid)
    }
}

/// One authenticated GET.
async fn get(url: &str) -> Result<Value, SourceError> {
    let keys = keys()?.ok_or(SourceError::NoSession { vendor: "alpaca" })?;
    let response = reqwest::Client::new()
        .get(url)
        .header("APCA-API-KEY-ID", &keys.key_id)
        .header("APCA-API-SECRET-KEY", &keys.secret)
        .send()
        .await
        .map_err(transport)?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        // The one failure a caller can act on: the keys are wrong, missing, or
        // not entitled to the feed being asked for — which is what a free plan
        // gets for asking `feed=sip`.
        return Err(SourceError::NoSession { vendor: "alpaca" });
    }
    if !status.is_success() {
        return Err(SourceError::Transport {
            vendor: "alpaca",
            detail: format!("HTTP {}", status.as_u16()),
        });
    }
    response.json().await.map_err(transport)
}

fn transport(err: reqwest::Error) -> SourceError {
    SourceError::Transport {
        vendor: "alpaca",
        detail: err.to_string(),
    }
}

fn malformed(detail: impl Into<String>) -> SourceError {
    SourceError::Malformed {
        vendor: "alpaca",
        detail: detail.into(),
    }
}

/// Arvo's interval spelling in Alpaca's vocabulary.
///
/// Alpaca caps minutes at 59 and hours at 23, and serves only a fixed set of
/// month steps — so an unsupported step is refused rather than rounded, the
/// same reasoning as the broker's spelling function.
fn spelling(interval: BarInterval) -> Result<String, SourceError> {
    let step = interval.step;
    match interval.unit {
        IntervalUnit::Minute if (1..=59).contains(&step) => Ok(format!("{step}Min")),
        IntervalUnit::Hour if (1..=23).contains(&step) => Ok(format!("{step}Hour")),
        IntervalUnit::Day if step == 1 => Ok("1Day".to_owned()),
        IntervalUnit::Week if step == 1 => Ok("1Week".to_owned()),
        // Alpaca has no second-resolution bars at all.
        _ => Err(SourceError::Unsupported(format!(
            "{interval} is not a resolution Alpaca serves — it offers 1-59 minutes, \
             1-23 hours, one day and one week"
        ))),
    }
}

/// Reads the bars for one symbol out of a multi-symbol reply.
///
/// A reply with no entry for the symbol is an empty series rather than an
/// error: Alpaca omits a symbol it has nothing for in the window, and that is
/// an ordinary answer. `ingest` refuses an empty series on its own.
fn parse_bars(body: &Value, symbol: &str) -> Result<Vec<Bar>, SourceError> {
    let Some(rows) = body
        .pointer("/bars")
        .and_then(Value::as_object)
        .and_then(|bars| bars.get(symbol))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    let mut bars = Vec::with_capacity(rows.len());
    for row in rows {
        let field = |name: &str| row.get(name).and_then(Value::as_f64);
        let (Some(open), Some(high), Some(low), Some(close)) =
            (field("o"), field("h"), field("l"), field("c"))
        else {
            // All four or none. Half a bar is worse than neither, and the same
            // rule the other two sources apply.
            return Err(malformed("a bar is missing one of o/h/l/c"));
        };
        let at = row
            .get("t")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("a bar has no timestamp"))?;
        let at = chrono::DateTime::parse_from_rfc3339(at)
            .map_err(|err| malformed(format!("timestamp {at:?}: {err}")))?
            .naive_utc();

        bars.push(Bar {
            at,
            open,
            high,
            low,
            close,
            volume: field("v").unwrap_or_default(),
        });
    }
    Ok(bars)
}

/// Reads cash dividends out of a corporate-actions reply.
///
/// Only `cash_dividends`. The endpoint also returns splits, mergers, spin-offs
/// and the rest; a split already reaches the platform through the price
/// adjustment, and the others have no consumer yet.
///
/// The **ex-date** is taken and the payable date deliberately ignored, because
/// `arvo_data::Dividend` models entitlement rather than settlement — see
/// `arvo_research::dividend` for why that is the date the measurement turns on.
/// The payable date is what a *cash* model would need, and there is no cash
/// model yet.
fn parse_dividends(body: &Value) -> Vec<Dividend> {
    body.pointer("/corporate_actions/cash_dividends")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let ex_date = row.get("ex_date").and_then(Value::as_str)?;
                    let ex_date = chrono::NaiveDate::parse_from_str(ex_date, "%Y-%m-%d").ok()?;
                    // A rate that cannot be read is skipped rather than taken
                    // as zero: a credit of the wrong amount is worse than a
                    // missing one.
                    let amount = row.get("rate").and_then(Value::as_f64)?;
                    Some(Dividend { ex_date, amount })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bars_reply(rows: Value) -> Value {
        json!({ "bars": { "MSFT": rows }, "next_page_token": null })
    }

    #[test]
    fn reads_alpacas_single_letter_bar_fields() {
        let bars = parse_bars(
            &bars_reply(json!([{
                "t": "2024-01-02T05:00:00Z",
                "o": 370.0, "h": 375.5, "l": 369.0, "c": 374.25,
                "v": 1_234_567.0, "vw": 372.1, "n": 8_901,
            }])),
            "MSFT",
        )
        .expect("a well-formed reply");

        assert_eq!(bars.len(), 1);
        assert!((bars[0].close - 374.25).abs() < 1e-9);
        assert!((bars[0].volume - 1_234_567.0).abs() < 1e-9);
    }

    #[test]
    fn a_symbol_the_reply_does_not_mention_is_an_empty_series_not_an_error() {
        // Alpaca omits a symbol it has nothing for in the window. `ingest`
        // refuses an empty series on its own, with a better message than this
        // could give.
        let bars = parse_bars(&bars_reply(json!([])), "NOSUCH").expect("readable");
        assert!(bars.is_empty());
    }

    #[test]
    fn a_half_formed_bar_is_refused_rather_than_zeroed() {
        let broken = json!([{ "t": "2024-01-02T05:00:00Z", "o": 370.0, "h": 375.5 }]);
        assert!(matches!(
            parse_bars(&bars_reply(broken), "MSFT"),
            Err(SourceError::Malformed { .. })
        ));
    }

    #[test]
    fn reads_cash_dividends_and_ignores_the_other_actions() {
        // A split already reaches the platform through the price adjustment,
        // and nothing consumes mergers or spin-offs yet.
        let body = json!({
            "corporate_actions": {
                "cash_dividends": [
                    { "symbol": "MSFT", "ex_date": "2024-02-14", "rate": 0.75,
                      "payable_date": "2024-03-14" },
                    { "symbol": "MSFT", "ex_date": "2024-05-15", "rate": 0.75,
                      "payable_date": "2024-06-13" },
                ],
                "forward_splits": [{ "symbol": "MSFT", "ex_date": "2024-06-01" }],
            },
            "next_page_token": null,
        });

        let paid = parse_dividends(&body);
        assert_eq!(paid.len(), 2, "two dividends, and the split is not one");
        assert_eq!(
            paid[0].ex_date,
            chrono::NaiveDate::from_ymd_opt(2024, 2, 14).expect("valid")
        );
        assert!((paid[0].amount - 0.75).abs() < 1e-9);
    }

    #[test]
    fn a_dividend_with_an_unreadable_rate_is_skipped_not_credited_as_zero() {
        // A cash credit of the wrong amount is worse than a missing one.
        let body = json!({ "corporate_actions": { "cash_dividends": [
            { "symbol": "MSFT", "ex_date": "2024-02-14" },
            { "symbol": "MSFT", "ex_date": "2024-05-15", "rate": 0.75 },
        ]}});
        let paid = parse_dividends(&body);
        assert_eq!(paid.len(), 1);
        assert!((paid[0].amount - 0.75).abs() < 1e-9);
    }

    #[test]
    fn a_reply_with_no_dividends_is_an_empty_list() {
        // Safe here because the caller asked a source that *does* serve them,
        // so none genuinely means none. A source that does not serve them
        // returns Unoffered from the trait default instead.
        assert!(parse_dividends(&json!({ "corporate_actions": {} })).is_empty());
    }

    #[test]
    fn the_free_feed_declares_itself_as_one_venue() {
        // The whole reason this source needed #40 first. IEX prices agree with
        // the tape, so no check on the bars could ever reveal this.
        assert_eq!(
            Alpaca::iex().basis().feed,
            Feed::SingleVenue("IEX"),
            "the free plan is IEX only and has to say so"
        );
        assert_eq!(Alpaca::sip().basis().feed, Feed::Consolidated);
    }

    #[test]
    fn the_two_feeds_file_under_different_venues() {
        // Two datasets with two content hashes. Sharing a venue would let a
        // study silently run on whichever was fetched last.
        assert_ne!(Alpaca::iex().venue(), Alpaca::sip().venue());
        assert_ne!(Alpaca::iex().id(), Alpaca::sip().id());
    }

    #[test]
    fn intervals_alpaca_does_not_serve_are_refused_rather_than_rounded() {
        assert_eq!(spelling(BarInterval::DAILY).expect("served"), "1Day");
        assert_eq!(
            spelling(BarInterval::new(5, IntervalUnit::Minute)).expect("served"),
            "5Min"
        );
        assert!(
            matches!(
                spelling(BarInterval::new(30, IntervalUnit::Second)),
                Err(SourceError::Unsupported(_))
            ),
            "Alpaca has no second bars, and a near miss would be the wrong data"
        );
        assert!(matches!(
            spelling(BarInterval::new(90, IntervalUnit::Minute)),
            Err(SourceError::Unsupported(_))
        ));
    }
}
