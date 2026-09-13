//! A second source for the same bars, and the only one that serves dividends.
//!
//! Not a second broker. This buys three things the Robinhood source cannot:
//!
//! **History.** Robinhood caps a daily request at 5,000 bars, about nineteen
//! years, and refuses anything wider outright. That yields seven folds at the
//! pinned walk-forward cadence, which is few enough that the selection test has
//! very little to work with. Yahoo serves from listing — four decades for an old
//! name — and more folds is the only way the procedure question gets a fair
//! hearing.
//!
//! **A second opinion.** [`arvo_data::agreement`] was written to compare one
//! vendor's bars against another's and has had no second vendor to compare
//! against from inside the app. Every quality check elsewhere inspects a series
//! against itself, which is the same structural weakness the reconciliation
//! invariants were written for: a number checked against a restatement of itself
//! catches nothing. See [`arvo_data::source::compare`].
//!
//! **Dividends.** The distribution series nothing else here offers, and the one
//! that makes the platform's excess-return bias measurable rather than merely
//! reported. See [`arvo_data::Dividend`].
//!
//! # Still a fetcher, not a provider
//!
//! It writes files through [`arvo_data::source::ingest`], exactly as every source does, and
//! for the reason stated there: an experiment pins its dataset as a content hash
//! of the bars it ran on, and a provider reaching over the network gives a
//! different answer whenever the vendor revises a bar.
//!
//! # Adjustment
//!
//! Yahoo's `open`/`high`/`low`/`close` are split-adjusted and not
//! dividend-adjusted, which is the same basis the Robinhood source asks for —
//! `adjustment_type: "split"`. That is what makes the two comparable at all.
//!
//! Yahoo also returns `adjclose`, which *is* dividend-adjusted. [`Yahoo::new`]
//! ignores it: mixing the two bases would make one source's series a rescaling
//! of the other's, and `agreement` would say so on every instrument forever.
//! [`Yahoo::total_return`] is the same endpoint read on that basis instead — a
//! second source under its own venue, because a total-return series is a
//! different dataset (ADR-0013). Dividends are still fetched as their own
//! series on both: on a total-return dataset the gap describes how much of a
//! margin was distributions rather than correcting it.
//!
//! No key, no account, no session. It is a public endpoint and this only ever
//! reads.

use arvo_data::{Bar, BarInterval, IntervalUnit};

use arvo_data::source::{Adjustment, Basis, Feed, Fetched, Source, SourceError};
use arvo_data::Dividend;

/// How this source is named, for the reproducibility record.
pub const SOURCE_ID: &str = "yahoo";

/// The venue these are filed under.
///
/// Its own, not shared with the broker's `RH`. Two sources' copies of one
/// instrument are two datasets with two content hashes, and filing them under
/// one name would let a study silently run on whichever was fetched last.
pub const VENUE: &str = "YF";

/// The total-return source's name and venue.
pub const TOTAL_RETURN_SOURCE_ID: &str = "yahoo-tr";
pub const TOTAL_RETURN_VENUE: &str = "YFTR";

const ENDPOINT: &str = "https://query1.finance.yahoo.com/v8/finance/chart";

/// Yahoo refuses a request without one, with a 429 that says nothing useful.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; arvo/0.1)";

/// The Yahoo source, on one adjustment basis or the other.
pub struct Yahoo {
    total_return: bool,
}

impl Yahoo {
    /// Split-adjusted, the broker's basis.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            total_return: false,
        }
    }

    /// Split- and dividend-adjusted, from `adjclose`.
    #[must_use]
    pub const fn total_return() -> Self {
        Self { total_return: true }
    }
}

impl Default for Yahoo {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Source for Yahoo {
    fn id(&self) -> &'static str {
        if self.total_return {
            TOTAL_RETURN_SOURCE_ID
        } else {
            SOURCE_ID
        }
    }

    fn label(&self) -> &'static str {
        if self.total_return {
            "Yahoo Finance (total return)"
        } else {
            "Yahoo Finance"
        }
    }

    fn venue(&self) -> &'static str {
        if self.total_return {
            TOTAL_RETURN_VENUE
        } else {
            VENUE
        }
    }

    fn basis(&self) -> Basis {
        Basis {
            feed: Feed::Consolidated,
            // `open`/`high`/`low`/`close` is the broker's basis, which is what
            // makes the two comparable; `adjclose` is total return. See the
            // module note.
            adjustment: if self.total_return {
                Adjustment::TotalReturn
            } else {
                Adjustment::Split
            },
        }
    }

    async fn bars(
        &self,
        symbol: &str,
        interval: BarInterval,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Fetched, SourceError> {
        if self.total_return && interval.is_intraday() {
            // Yahoo computes `adjclose` for daily bars and coarser only.
            // Refused rather than served split-adjusted under a venue that
            // declares total return.
            return Err(SourceError::Unsupported(format!(
                "Yahoo serves total-return prices daily and coarser, not at {interval}"
            )));
        }
        let body = chart(symbol, interval, from, to).await?;
        Ok(Fetched {
            bars: parse_bars(&body, self.total_return)?,
            // Yahoo drops a session it has no print for rather than filling it,
            // so there is nothing invented to count. `parse_bars` drops any
            // half-formed bar for the same reason.
            interpolated: 0,
        })
    }

    async fn dividends(
        &self,
        symbol: &str,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<Dividend>, SourceError> {
        // Daily, whatever resolution the bars were pulled at. A distribution
        // happens on a date, not in a five-minute bucket, and asking for it
        // intraday would return the same handful of events against a window
        // Yahoo caps far shorter.
        let body = chart_with(symbol, BarInterval::DAILY, from, to, "&events=div").await?;
        parse_dividends(&body)
    }
}

/// One chart reply.
async fn chart(
    symbol: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<serde_json::Value, SourceError> {
    chart_with(symbol, interval, from, to, "").await
}

async fn chart_with(
    symbol: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    extra: &str,
) -> Result<serde_json::Value, SourceError> {
    let url = format!(
        "{ENDPOINT}/{symbol}?period1={}&period2={}&interval={}{extra}",
        from.and_time(chrono::NaiveTime::MIN).and_utc().timestamp(),
        to.and_time(chrono::NaiveTime::MIN).and_utc().timestamp(),
        spelling(interval),
    );

    let response = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(transport)?;

    if !response.status().is_success() {
        return Err(SourceError::Transport {
            vendor: SOURCE_ID,
            detail: format!("HTTP {} for {symbol}", response.status().as_u16()),
        });
    }

    response.json().await.map_err(transport)
}

fn transport(err: reqwest::Error) -> SourceError {
    SourceError::Transport {
        vendor: SOURCE_ID,
        detail: err.to_string(),
    }
}

fn malformed(detail: impl Into<String>) -> SourceError {
    SourceError::Malformed {
        vendor: SOURCE_ID,
        detail: detail.into(),
    }
}

/// Arvo's interval spelling in Yahoo's vocabulary.
fn spelling(interval: BarInterval) -> String {
    let unit = match interval.unit {
        IntervalUnit::Minute => "m",
        IntervalUnit::Hour => "h",
        IntervalUnit::Day => "d",
        IntervalUnit::Week => "wk",
        IntervalUnit::Second => "s",
    };
    format!("{}{unit}", interval.step)
}

/// The chart result, with Yahoo's in-body error reporting handled.
///
/// It reports its own errors in the body with a 200, so a reader checking only
/// the status code would call an unknown symbol a successful fetch of nothing.
fn result(body: &serde_json::Value) -> Result<&serde_json::Value, SourceError> {
    if let Some(error) = body.pointer("/chart/error").filter(|e| !e.is_null()) {
        return Err(malformed(error.to_string()));
    }
    body.pointer("/chart/result/0")
        .ok_or_else(|| malformed("no chart result"))
}

/// Reads the chart reply.
///
/// Yahoo returns five parallel arrays beside a timestamp array, and any of the
/// five can hold a null where a session had no print. Such a bar is dropped
/// rather than filled: an invented price is the one thing a breakout rule cannot
/// tell from a real one, which is the same reason the Robinhood source drops the
/// interpolated bars its own server synthesises.
///
/// With `total_return`, every price in a bar is scaled by that bar's
/// `adjclose / close`. Yahoo adjusts only the close, and a bar whose open, high
/// and low stayed on the split basis would have its close outside its own
/// range on every day before a distribution.
fn parse_bars(body: &serde_json::Value, total_return: bool) -> Result<Vec<Bar>, SourceError> {
    let result = result(body)?;
    let times = result
        .pointer("/timestamp")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| malformed("no timestamps"))?;
    let quote = result
        .pointer("/indicators/quote/0")
        .ok_or_else(|| malformed("no quote block"))?;

    let column = |name: &str| {
        quote
            .get(name)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| malformed(format!("no {name} column")))
    };
    let (open, high, low, close, volume) = (
        column("open")?,
        column("high")?,
        column("low")?,
        column("close")?,
        column("volume")?,
    );
    let adjusted = if total_return {
        Some(
            result
                .pointer("/indicators/adjclose/0/adjclose")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| malformed("no adjclose column"))?,
        )
    } else {
        None
    };

    let mut bars = Vec::with_capacity(times.len());
    for (index, time) in times.iter().enumerate() {
        let Some(seconds) = time.as_i64() else {
            continue;
        };
        let at = chrono::DateTime::from_timestamp(seconds, 0)
            .map(|utc| utc.naive_utc())
            .ok_or_else(|| malformed("a timestamp outside the representable range"))?;

        // All five or none. A bar missing any leg is a session Yahoo has no
        // print for, and half of one is worse than neither.
        let value =
            |column: &[serde_json::Value]| column.get(index).and_then(serde_json::Value::as_f64);
        let (Some(open), Some(high), Some(low), Some(close), Some(volume)) = (
            value(open),
            value(high),
            value(low),
            value(close),
            value(volume),
        ) else {
            continue;
        };
        // The same all-or-none rule for the adjusted close: a bar that cannot
        // be put on the declared basis is dropped, not left on the other one.
        let factor = match adjusted {
            None => 1.0,
            Some(adjusted) => match value(adjusted) {
                Some(adjclose) if close > 0.0 => adjclose / close,
                _ => continue,
            },
        };
        let (open, high, low, close) = (open * factor, high * factor, low * factor, close * factor);

        bars.push(Bar {
            // Yahoo stamps a daily bar at the session *open* in exchange time.
            // Arvo's `Bar::at` is also the opening instant, so the two agree —
            // and the engine adds the interval to reach the close, which is the
            // convention that keeps trade instants and curve instants on one
            // clock.
            at: at.date().and_time(chrono::NaiveTime::MIN),
            open,
            high,
            low,
            close,
            volume,
        });
    }

    // Oldest first, which is what every consumer assumes and what Yahoo already
    // does — asserted rather than trusted, because a reversed series would look
    // like a working fetch and produce nonsense downstream.
    bars.sort_by_key(|bar| bar.at);
    bars.dedup_by_key(|bar| bar.at);
    Ok(bars)
}

/// Reads the `events.dividends` block.
///
/// Yahoo keys it by epoch second rather than returning an array, so this reads
/// the values and sorts, rather than trusting a JSON object's ordering — which
/// `serde_json` does not promise and which arrives in whatever order the wire
/// had it.
///
/// A reply with no `events` block at all is an instrument that paid nothing in
/// the window, which is an empty list rather than an error. That is safe here
/// and only here: the caller asked a source that *does* serve dividends, so
/// "none" genuinely means none — the case that must never be silently empty is a
/// source that does not serve them, and that one returns
/// [`SourceError::Unoffered`] from the trait default.
fn parse_dividends(body: &serde_json::Value) -> Result<Vec<Dividend>, SourceError> {
    let result = result(body)?;
    let Some(events) = result
        .pointer("/events/dividends")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Vec::new());
    };

    let mut paid = Vec::with_capacity(events.len());
    for event in events.values() {
        // The `date` field rather than the key: they agree, and the key is a
        // string that has to be re-parsed to be trusted.
        let (Some(seconds), Some(amount)) = (
            event.get("date").and_then(serde_json::Value::as_i64),
            event.get("amount").and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        let Some(ex_date) = chrono::DateTime::from_timestamp(seconds, 0).map(|at| at.date_naive())
        else {
            continue;
        };
        paid.push(Dividend { ex_date, amount });
    }

    paid.sort_by_key(|dividend| dividend.ex_date);
    Ok(paid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(times: &[i64], closes: &[Option<f64>]) -> serde_json::Value {
        let column = |values: &[Option<f64>]| {
            serde_json::Value::Array(
                values
                    .iter()
                    .map(|v| v.map_or(serde_json::Value::Null, |v| serde_json::json!(v)))
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

    fn parse_bars_split(body: &serde_json::Value) -> Result<Vec<Bar>, SourceError> {
        parse_bars(body, false)
    }

    #[test]
    fn a_total_return_bar_is_scaled_whole_so_its_close_stays_inside_its_range() {
        let mut body = reply(&[1_700_000_000, 1_700_086_400], &[Some(100.0), Some(100.0)]);
        body["chart"]["result"][0]["indicators"]["quote"][0]["high"] =
            serde_json::json!([101.0, 101.0]);
        body["chart"]["result"][0]["indicators"]["adjclose"] =
            serde_json::json!([{ "adjclose": [98.0, 100.0] }]);

        let bars = parse_bars(&body, true).expect("a total-return reply");
        assert!((bars[0].close - 98.0).abs() < 1e-9);
        assert!(
            (bars[0].high - 98.98).abs() < 1e-9,
            "scaled by 0.98: {}",
            bars[0].high
        );
        assert!(bars[0].close <= bars[0].high);
        assert!(
            (bars[1].close - 100.0).abs() < 1e-9,
            "no distribution since, no factor"
        );

        let split = parse_bars(&body, false).expect("the same reply, split basis");
        assert!((split[0].close - 100.0).abs() < 1e-9, "adjclose ignored");
    }

    #[test]
    fn a_total_return_reply_without_adjclose_is_refused_not_served_split() {
        let body = reply(&[1_700_000_000], &[Some(100.0)]);
        let Err(SourceError::Malformed { detail, .. }) = parse_bars(&body, true) else {
            panic!("a missing adjclose is a shape problem");
        };
        assert!(detail.contains("adjclose"), "{detail}");
    }

    #[test]
    fn the_two_yahoo_sources_are_two_datasets() {
        let (split, total) = (Yahoo::new(), Yahoo::total_return());
        assert_ne!(split.id(), total.id());
        assert_ne!(split.venue(), total.venue());
        assert_eq!(split.basis().adjustment, Adjustment::Split);
        assert_eq!(total.basis().adjustment, Adjustment::TotalReturn);
    }

    #[test]
    fn reads_a_chart_into_bars() {
        let bars = parse_bars_split(&reply(
            &[1_700_000_000, 1_700_086_400],
            &[Some(1.0), Some(2.0)],
        ))
        .expect("a well-formed reply");
        assert_eq!(bars.len(), 2);
        assert!((bars[0].close - 1.0).abs() < 1e-9);
        assert!(bars[0].at < bars[1].at, "oldest first");
    }

    #[test]
    fn a_session_with_no_print_is_dropped_rather_than_filled() {
        // The one thing a breakout rule cannot tell from a real price is an
        // invented one, which is why the broker source drops its server's
        // synthesised bars too.
        let bars = parse_bars_split(&reply(&[1_700_000_000, 1_700_086_400], &[Some(1.0), None]))
            .expect("a reply with a hole is still readable");
        assert_eq!(bars.len(), 1, "the null bar is gone, not zeroed");
    }

    #[test]
    fn yahoos_own_error_is_an_error_here_rather_than_an_empty_series() {
        // It reports them in the body with a 200, so a reader checking only the
        // status code would call an unknown symbol a successful fetch of
        // nothing.
        let body = serde_json::json!({
            "chart": { "error": { "code": "Not Found" }, "result": null }
        });
        assert!(matches!(
            parse_bars_split(&body),
            Err(SourceError::Malformed { .. })
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
        let Err(SourceError::Malformed { detail, .. }) = parse_bars_split(&body) else {
            panic!("a missing column is a shape problem");
        };
        assert!(detail.contains("high"), "{detail}");
    }

    #[test]
    fn intervals_are_spelled_the_way_yahoo_spells_them() {
        assert_eq!(spelling(BarInterval::DAILY), "1d");
        assert_eq!(spelling(BarInterval::new(5, IntervalUnit::Minute)), "5m");
        assert_eq!(spelling(BarInterval::new(1, IntervalUnit::Week)), "1wk");
    }

    fn with_dividends(events: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "chart": { "error": null, "result": [{
                "timestamp": [1_700_000_000],
                "indicators": { "quote": [{
                    "open": [1.0], "high": [1.0], "low": [1.0],
                    "close": [1.0], "volume": [1.0],
                }]},
                "events": { "dividends": events }
            }]}
        })
    }

    #[test]
    fn dividends_come_out_in_date_order_whatever_order_the_object_had() {
        // Keyed by epoch second, not an array — and a JSON object's ordering is
        // not something serde_json promises.
        let paid = parse_dividends(&with_dividends(serde_json::json!({
            "1715300000": { "amount": 0.25, "date": 1_715_300_000 },
            "1707400000": { "amount": 0.24, "date": 1_707_400_000 },
        })))
        .unwrap();

        assert_eq!(paid.len(), 2);
        assert!(paid[0].ex_date < paid[1].ex_date, "oldest first");
        assert!((paid[0].amount - 0.24).abs() < 1e-9);
    }

    #[test]
    fn an_instrument_that_paid_nothing_is_an_empty_list_not_an_error() {
        // Safe here because the caller asked a source that does serve
        // dividends, so "none" genuinely means none. A source that does not
        // serve them returns Unoffered from the trait default instead.
        let body = serde_json::json!({
            "chart": { "error": null, "result": [{ "timestamp": [] }]}
        });
        assert_eq!(parse_dividends(&body).unwrap(), Vec::new());
    }

    #[test]
    fn a_half_formed_dividend_event_is_skipped_rather_than_credited_as_zero() {
        // A cash credit of the wrong amount is worse than a missing one.
        let paid = parse_dividends(&with_dividends(serde_json::json!({
            "1707400000": { "date": 1_707_400_000 },
            "1715300000": { "amount": 0.25, "date": 1_715_300_000 },
        })))
        .unwrap();
        assert_eq!(paid.len(), 1);
        assert!((paid[0].amount - 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_dividend_reply_that_errored_is_not_read_as_no_dividends() {
        let body = serde_json::json!({
            "chart": { "error": { "code": "Not Found" }, "result": null }
        });
        assert!(matches!(
            parse_dividends(&body),
            Err(SourceError::Malformed { .. })
        ));
    }
}
