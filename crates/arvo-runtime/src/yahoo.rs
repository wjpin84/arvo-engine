//! A second source for the same bars.
//!
//! Not a second broker. This buys two things the Robinhood feed cannot:
//!
//! **History.** Robinhood caps a daily request at 5,000 bars, about nineteen
//! years, and refuses anything wider outright. That yields seven folds at the
//! pinned walk-forward cadence, which is few enough that the selection test
//! has very little to work with. Yahoo serves from listing — four decades for
//! an old name — and more folds is the only way the procedure question gets a
//! fair hearing.
//!
//! **A second opinion.** [`arvo_data::agreement`] was written to compare one
//! vendor's bars against another's and has had no second vendor to compare
//! against. Every quality check in this codebase inspects a series against
//! itself, which is the same structural weakness the reconciliation invariants
//! were written for: a number checked against a restatement of itself catches
//! nothing.
//!
//! # Still a fetcher, not a provider
//!
//! It writes files, exactly as [`crate::feed`] does, and for the reason stated
//! there and in the gap analysis: an experiment pins its dataset as a content
//! hash of the bars it ran on, and a provider reaching over the network gives
//! a different answer whenever the vendor revises a bar. Every stored verdict
//! would quietly stop being checkable. The gap was real; that prescription was
//! a regression.
//!
//! # Adjustment
//!
//! Yahoo's `open`/`high`/`low`/`close` are split-adjusted and not
//! dividend-adjusted, which is the same basis `crate::feed` asks Robinhood for
//! — `adjustment_type: "split"`. That is what makes the two comparable at all.
//! Yahoo also returns `adjclose`, which *is* dividend-adjusted, and this
//! deliberately ignores it: mixing the two bases would make one source's
//! series a rescaling of the other's, and `agreement` would say so on every
//! instrument forever.
//!
//! No key, no account, no session. It is a public endpoint and this only ever
//! reads.

use arvo_data::{Bar, BarInterval, CsvBars};

/// Where the bars come from, for the reproducibility record.
pub const FEED_ID: &str = "yahoo";

/// The venue these are filed under.
///
/// Its own, not shared with the broker's `RH`. Two sources' copies of one
/// instrument are two datasets with two content hashes, and filing them under
/// one name would let a study silently run on whichever was fetched last.
pub const FETCHED_VENUE: &str = "YF";

const ENDPOINT: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

/// Yahoo refuses a request without one, with a 429 that says nothing useful.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; arvo/0.1)";

#[derive(Debug, thiserror::Error)]
pub enum YahooError {
    #[error("talking to yahoo about {symbol}: {source}")]
    Transport {
        symbol: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("yahoo returned {status} for {symbol}")]
    Status { symbol: String, status: u16 },
    #[error("yahoo's reply for {symbol} was not the shape this reads: {detail}")]
    Shape { symbol: String, detail: String },
    #[error("yahoo returned no bars for {symbol} between {from} and {to}")]
    Empty {
        symbol: String,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    },
    #[error(transparent)]
    Data(#[from] arvo_data::DataError),
}

/// What one fetch did.
#[derive(Debug, Clone)]
pub struct FetchReport {
    pub instrument: String,
    pub bars: usize,
    pub from: Option<chrono::NaiveDateTime>,
    pub to: Option<chrono::NaiveDateTime>,
    pub path: std::path::PathBuf,
    pub quality: arvo_data::quality::Report,
    /// How this compares to the copy already held under the same name.
    pub revision: Option<arvo_data::agreement::Agreement>,
}

/// Fetches one symbol's history into the library.
///
/// # Errors
///
/// Returns [`YahooError`] if the call fails, the reply is not the shape this
/// reads, or nothing comes back for the window.
pub async fn fetch(
    root: &std::path::Path,
    symbol: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<FetchReport, YahooError> {
    let bars = bars(symbol, interval, from, to).await?;
    if bars.is_empty() {
        return Err(YahooError::Empty {
            symbol: symbol.to_owned(),
            from,
            to,
        });
    }

    let instrument = format!("{symbol}.{FETCHED_VENUE}");
    let quality = arvo_data::quality::inspect(&bars, interval);

    // Against what is held, before it is overwritten — the same order and the
    // same reason as the broker fetcher: reading after the write compares the
    // new series against itself.
    let library = CsvBars::new(root);
    let revision = arvo_data::BarProvider::bars(&library, &instrument, interval, from, to)
        .ok()
        .filter(|held| !held.is_empty())
        .map(|held| arvo_data::agreement::compare(&held, &bars).0);

    let path = library.write(&instrument, interval, &bars)?;
    Ok(FetchReport {
        instrument,
        bars: bars.len(),
        from: bars.first().map(|bar| bar.at),
        to: bars.last().map(|bar| bar.at),
        path,
        quality,
        revision,
    })
}

/// The bars themselves, without writing anything.
///
/// Separate so the comparison in [`crate::compare_sources`] can hold two
/// series side by side without either touching the library.
///
/// # Errors
///
/// Returns [`YahooError`] if the call fails or the reply cannot be read.
pub async fn bars(
    symbol: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<Vec<Bar>, YahooError> {
    let url = format!(
        "{ENDPOINT}/{symbol}?period1={}&period2={}&interval={}",
        from.and_time(chrono::NaiveTime::MIN).and_utc().timestamp(),
        to.and_time(chrono::NaiveTime::MIN).and_utc().timestamp(),
        spelling(interval),
    );

    let response = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|source| YahooError::Transport {
            symbol: symbol.to_owned(),
            source,
        })?;

    if !response.status().is_success() {
        return Err(YahooError::Status {
            symbol: symbol.to_owned(),
            status: response.status().as_u16(),
        });
    }

    let body: serde_json::Value = response.json().await.map_err(|source| {
        YahooError::Transport {
            symbol: symbol.to_owned(),
            source,
        }
    })?;

    parse(symbol, &body)
}

/// Arvo's interval spelling in Yahoo's vocabulary.
fn spelling(interval: BarInterval) -> String {
    use arvo_data::IntervalUnit;
    let unit = match interval.unit {
        IntervalUnit::Minute => "m",
        IntervalUnit::Hour => "h",
        IntervalUnit::Day => "d",
        IntervalUnit::Week => "wk",
        IntervalUnit::Second => "s",
    };
    format!("{}{unit}", interval.step)
}

/// Reads the chart reply.
///
/// Yahoo returns five parallel arrays beside a timestamp array, and any of the
/// five can hold a null where a session had no print. Such a bar is dropped
/// rather than filled: an invented price is the one thing a breakout rule
/// cannot tell from a real one, which is the same reason `crate::feed` drops
/// the interpolated bars its own server synthesises.
fn parse(symbol: &str, body: &serde_json::Value) -> Result<Vec<Bar>, YahooError> {
    let shape = |detail: &str| YahooError::Shape {
        symbol: symbol.to_owned(),
        detail: detail.to_owned(),
    };

    // Yahoo reports its own errors in the body with a 200.
    if let Some(error) = body.pointer("/chart/error").filter(|e| !e.is_null()) {
        return Err(shape(&error.to_string()));
    }

    let result = body
        .pointer("/chart/result/0")
        .ok_or_else(|| shape("no chart result"))?;
    let times = result
        .pointer("/timestamp")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| shape("no timestamps"))?;
    let quote = result
        .pointer("/indicators/quote/0")
        .ok_or_else(|| shape("no quote block"))?;

    let column = |name: &str| {
        quote
            .get(name)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| shape(&format!("no {name} column")))
    };
    let (open, high, low, close, volume) = (
        column("open")?,
        column("high")?,
        column("low")?,
        column("close")?,
        column("volume")?,
    );

    let mut bars = Vec::with_capacity(times.len());
    for (index, time) in times.iter().enumerate() {
        let Some(seconds) = time.as_i64() else {
            continue;
        };
        let at = chrono::DateTime::from_timestamp(seconds, 0)
            .map(|utc| utc.naive_utc())
            .ok_or_else(|| shape("a timestamp outside the representable range"))?;

        // All five or none. A bar missing any leg is a session Yahoo has no
        // print for, and half of one is worse than neither.
        let value = |column: &[serde_json::Value]| column.get(index).and_then(serde_json::Value::as_f64);
        let (Some(open), Some(high), Some(low), Some(close), Some(volume)) = (
            value(open),
            value(high),
            value(low),
            value(close),
            value(volume),
        ) else {
            continue;
        };

        bars.push(Bar {
            // Yahoo stamps a daily bar at the session *open* in exchange time.
            // Arvo's `Bar::at` is also the opening instant, so the two agree —
            // and the engine adds the interval to reach the close, which is
            // the convention that keeps trade instants and curve instants on
            // one clock.
            at: at.date().and_time(chrono::NaiveTime::MIN),
            open,
            high,
            low,
            close,
            volume,
        });
    }

    // Oldest first, which is what every consumer assumes and what Yahoo
    // already does — asserted rather than trusted, because a reversed series
    // would look like a working fetch and produce nonsense downstream.
    bars.sort_by_key(|bar| bar.at);
    bars.dedup_by_key(|bar| bar.at);
    Ok(bars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(times: &[i64], closes: &[Option<f64>]) -> serde_json::Value {
        let column = |values: &[Option<f64>]| {
            serde_json::Value::Array(
                values
                    .iter()
                    .map(|v| {
                        v.map_or(serde_json::Value::Null, |v| {
                            serde_json::json!(v)
                        })
                    })
                    .collect(),
            )
        };
        serde_json::json!({
            "chart": {
                "error": null,
                "result": [{
                    "timestamp": times,
                    "indicators": { "quote": [{
                        "open": column(closes),
                        "high": column(closes),
                        "low": column(closes),
                        "close": column(closes),
                        "volume": column(closes),
                    }]}
                }]
            }
        })
    }

    #[test]
    fn reads_a_chart_into_bars() {
        let bars = parse("AAPL", &reply(&[1_700_000_000, 1_700_086_400], &[Some(1.0), Some(2.0)]))
            .expect("a well-formed reply");
        assert_eq!(bars.len(), 2);
        assert!((bars[0].close - 1.0).abs() < 1e-9);
        assert!(bars[0].at < bars[1].at, "oldest first");
    }

    #[test]
    fn a_session_with_no_print_is_dropped_rather_than_filled() {
        // The one thing a breakout rule cannot tell from a real price is an
        // invented one, which is why the broker fetcher drops its server's
        // synthesised bars too.
        let bars = parse("AAPL", &reply(&[1_700_000_000, 1_700_086_400], &[Some(1.0), None]))
            .expect("a reply with a hole is still readable");
        assert_eq!(bars.len(), 1, "the null bar is gone, not zeroed");
    }

    #[test]
    fn yahoos_own_error_is_an_error_here_rather_than_an_empty_series() {
        // It reports them in the body with a 200, so a reader checking only
        // the status code would call an unknown symbol a successful fetch of
        // nothing.
        let body = serde_json::json!({
            "chart": { "error": { "code": "Not Found" }, "result": null }
        });
        assert!(matches!(
            parse("NOSUCH", &body),
            Err(YahooError::Shape { .. })
        ));
    }

    #[test]
    fn a_reply_missing_a_column_is_named_rather_than_half_read() {
        let body = serde_json::json!({
            "chart": { "error": null, "result": [{
                "timestamp": [1_700_000_000],
                "indicators": { "quote": [{ "open": [1.0] }] }
            }]}
        });
        let Err(YahooError::Shape { detail, .. }) = parse("AAPL", &body) else {
            panic!("a missing column is a shape problem");
        };
        assert!(detail.contains("high"), "{detail}");
    }

    #[test]
    fn intervals_are_spelled_the_way_yahoo_spells_them() {
        use arvo_data::IntervalUnit;
        assert_eq!(spelling(BarInterval::DAILY), "1d");
        assert_eq!(spelling(BarInterval::new(5, IntervalUnit::Minute)), "5m");
        assert_eq!(spelling(BarInterval::new(1, IntervalUnit::Week)), "1wk");
    }
}
