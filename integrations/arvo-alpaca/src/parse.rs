//! The shape of Alpaca's replies.
//!
//! Single-letter bar fields and corporate actions keyed by type. Pure
//! functions over JSON, so all of it is tested without a network.

use arvo_data::source::SourceError;
use arvo_data::{Bar, Dividend};
use serde_json::Value;

pub(crate) fn malformed(detail: impl Into<String>) -> SourceError {
    SourceError::Malformed {
        vendor: "alpaca",
        detail: detail.into(),
    }
}

/// Reads the bars for one symbol out of a multi-symbol reply.
///
/// A reply with no entry for the symbol is an empty series rather than an
/// error: Alpaca omits a symbol it has nothing for in the window, and that is
/// an ordinary answer. `ingest` refuses an empty series on its own.
pub(crate) fn parse_bars(body: &Value, symbol: &str) -> Result<Vec<Bar>, SourceError> {
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
pub(crate) fn parse_dividends(body: &Value) -> Vec<Dividend> {
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
}
