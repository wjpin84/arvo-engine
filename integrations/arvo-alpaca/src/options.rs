//! Option quotes, recorded because they cannot be fetched later (#83).
//!
//! Alpaca serves option bars and trades back to 2024 but no historical quotes.
//! Without a bid and an ask at the time of a fill, a backtest fills at the
//! traded price and never pays the spread — which flatters every option
//! strategy, and the short-dated far-from-the-money ones most. The spread a
//! fill model assumes (#14) has to be calibrated from quotes someone kept, so
//! this keeps them, from now.
//!
//! # The quotes are indicative, not the NBBO
//!
//! The free plan's option feed is `indicative`; the real-time OPRA feed needs
//! an agreement this account has not signed. Its trades arrive fifteen minutes
//! late, and its quotes are Alpaca's indicative prices rather than the
//! consolidated best bid and offer. So the feed is part of the file's path, the
//! same way a stock feed is part of its venue: a spread measured on it is a
//! spread *on this feed*, and a model calibrated from it has to say so.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use arvo_data::source::SourceError;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use crate::auth::get;
use crate::parse::malformed;

const DATA: &str = "https://data.alpaca.markets";

/// The only option feed this plan is entitled to.
pub const FEED: &str = "indicative";

/// Strikes this far either side of the underlying's price are kept.
///
/// Wide enough for a 30-45 day put spread sold at a low delta, and for every
/// strike a 0DTE trade touches.
pub const STRIKE_BAND: f64 = 0.08;

/// Expirations this many days out are kept: 0DTE through the premium-selling
/// range.
pub const HORIZON_DAYS: i64 = 60;

/// One contract's quote, as recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainQuote {
    pub symbol: String,
    pub expiration: NaiveDate,
    /// `'C'` or `'P'`.
    pub right: char,
    pub strike: f64,
    pub quote_at: DateTime<Utc>,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

/// What one recording wrote.
#[derive(Debug)]
pub struct Recorded {
    pub path: PathBuf,
    pub contracts: usize,
}

/// Snapshots the chain around the underlying's price and appends it to
/// `<dir>/<UNDERLYING>.<feed>/<date>.csv`.
///
/// The underlying's bid and ask are read first and written on every row, so a
/// quote can be placed against the price it was quoted beside. They come from
/// the IEX feed: for SPY its top of book is the market's, within a cent.
///
/// ponytail: ~3,400 SPY contracts per snapshot, ~9 MB a day at one every fifteen
/// minutes. Compress closed days if the folder grows past what anyone wants.
///
/// # Errors
///
/// [`SourceError::NoSession`] without keys, and the transport or shape errors of
/// either request. Nothing is written unless both succeeded.
pub async fn record_chain(
    underlying: &str,
    dir: &Path,
    now: DateTime<Utc>,
) -> Result<Recorded, SourceError> {
    let latest = get(&format!(
        "{DATA}/v2/stocks/quotes/latest?symbols={underlying}&feed=iex"
    ))
    .await?;
    let quote = latest
        .pointer(&format!("/quotes/{underlying}"))
        .ok_or_else(|| malformed(format!("no latest quote for {underlying}")))?;
    let (Some(under_bid), Some(under_ask)) = (
        quote.get("bp").and_then(Value::as_f64),
        quote.get("ap").and_then(Value::as_f64),
    ) else {
        return Err(malformed(format!(
            "{underlying}'s latest quote has no bid or ask"
        )));
    };
    // A one-sided book reads 0 on the missing side; the mid would then be half
    // the price and the band would miss the money entirely.
    if under_bid <= 0.0 || under_ask <= 0.0 {
        return Err(malformed(format!(
            "{underlying}'s latest quote is one-sided"
        )));
    }
    let spot = (under_bid + under_ask) / 2.0;
    let until = now.date_naive() + chrono::Duration::days(HORIZON_DAYS);

    let mut quotes = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let mut url = format!(
            "{DATA}/v1beta1/options/snapshots/{underlying}?feed={FEED}\
             &strike_price_gte={:.0}&strike_price_lte={:.0}&expiration_date_lte={until}&limit=1000",
            (spot * (1.0 - STRIKE_BAND)).floor(),
            (spot * (1.0 + STRIKE_BAND)).ceil(),
        );
        if let Some(token) = &page {
            url.push_str(&format!("&page_token={token}"));
        }
        let body = get(&url).await?;
        quotes.extend(parse_chain(&body)?);
        page = body
            .get("next_page_token")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if page.is_none() {
            break;
        }
    }
    quotes.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    let folder = dir.join(format!("{underlying}.{FEED}"));
    std::fs::create_dir_all(&folder).map_err(io)?;
    let path = folder.join(format!("{}.csv", now.date_naive()));
    let fresh = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(io)?;
    let mut text = String::new();
    if fresh {
        text.push_str(HEADER);
        text.push('\n');
    }
    for quote in &quotes {
        text.push_str(&row(now, quote, under_bid, under_ask));
        text.push('\n');
    }
    // One write, so a crash mid-snapshot leaves at most one torn line rather
    // than a snapshot that silently holds half the chain.
    file.write_all(text.as_bytes()).map_err(io)?;

    Ok(Recorded {
        path,
        contracts: quotes.len(),
    })
}

const HEADER: &str = "recorded_at,symbol,expiration,right,strike,quote_at,bid,ask,bid_size,ask_size,underlying_bid,underlying_ask";

fn row(recorded_at: DateTime<Utc>, q: &ChainQuote, under_bid: f64, under_ask: f64) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{}",
        recorded_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        q.symbol,
        q.expiration,
        q.right,
        q.strike,
        q.quote_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        q.bid,
        q.ask,
        q.bid_size,
        q.ask_size,
        under_bid,
        under_ask,
    )
}

/// Reads the contracts out of a chain snapshot reply.
///
/// A contract with no quote is left out rather than written as zeros: a zero
/// *bid* is real information about a far out-of-the-money contract, and an
/// absent quote written the same way would be indistinguishable from it.
pub(crate) fn parse_chain(body: &Value) -> Result<Vec<ChainQuote>, SourceError> {
    let Some(snapshots) = body.get("snapshots").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut quotes = Vec::with_capacity(snapshots.len());
    for (symbol, snapshot) in snapshots {
        let Some(quote) = snapshot.get("latestQuote") else {
            continue;
        };
        let (expiration, right, strike) = occ(symbol)
            .ok_or_else(|| malformed(format!("{symbol:?} is not an OCC option symbol")))?;
        let field = |name: &str| quote.get(name).and_then(Value::as_f64);
        let (Some(bid), Some(ask)) = (field("bp"), field("ap")) else {
            return Err(malformed(format!(
                "{symbol}'s quote is missing a bid or ask"
            )));
        };
        let at = quote
            .get("t")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed(format!("{symbol}'s quote has no time")))?;
        let quote_at = DateTime::parse_from_rfc3339(at)
            .map_err(|err| malformed(format!("quote time {at:?}: {err}")))?
            .with_timezone(&Utc);
        quotes.push(ChainQuote {
            symbol: symbol.clone(),
            expiration,
            right,
            strike,
            quote_at,
            bid,
            ask,
            bid_size: field("bs").unwrap_or_default(),
            ask_size: field("as").unwrap_or_default(),
        });
    }
    Ok(quotes)
}

/// Expiration, right and strike from an OCC symbol: root, `YYMMDD`, `C`/`P`,
/// strike × 1000 in eight digits. Read from the end, because the root is
/// variable length.
fn occ(symbol: &str) -> Option<(NaiveDate, char, f64)> {
    let tail = symbol.get(symbol.len().checked_sub(15)?..)?;
    let expiration = NaiveDate::parse_from_str(&format!("20{}", &tail[..6]), "%Y%m%d").ok()?;
    let right = tail[6..7]
        .chars()
        .next()
        .filter(|c| *c == 'C' || *c == 'P')?;
    let strike = tail[7..].parse::<u32>().ok()?;
    Some((expiration, right, f64::from(strike) / 1000.0))
}

fn io(err: std::io::Error) -> SourceError {
    SourceError::Transport {
        vendor: "alpaca",
        detail: format!("writing option quotes: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_occ_symbol_reads_from_the_end() {
        assert_eq!(
            occ("SPY260914C00760000"),
            Some((
                NaiveDate::from_ymd_opt(2026, 9, 14).expect("valid"),
                'C',
                760.0
            ))
        );
        assert_eq!(
            occ("SPXW261016P05822500"),
            Some((
                NaiveDate::from_ymd_opt(2026, 10, 16).expect("valid"),
                'P',
                5822.5
            )),
            "a longer root and a half-dollar strike"
        );
        assert_eq!(occ("SPY"), None);
        assert_eq!(occ("SPY260914X00760000"), None);
    }

    #[test]
    fn reads_a_chain_snapshot_and_leaves_out_contracts_with_no_quote() {
        let body = json!({ "snapshots": {
            "SPY260914C00760000": {
                "latestQuote": { "ap": 2.28, "as": 47, "bp": 2.27, "bs": 147, "t": "2026-09-14T18:22:59.38237261Z" },
                "latestTrade": { "p": 2.28, "s": 1, "t": "2026-09-14T18:07:59.482013968Z" }
            },
            "SPY260914P00700000": {
                "latestQuote": { "ap": 0.01, "as": 900, "bp": 0.0, "bs": 0, "t": "2026-09-14T18:22:59Z" }
            },
            "SPY260914P00650000": { "latestTrade": { "p": 0.01 } }
        }});
        let mut quotes = parse_chain(&body).expect("well formed");
        quotes.sort_by(|a, b| a.symbol.cmp(&b.symbol));

        assert_eq!(quotes.len(), 2, "no quote is not a zero quote");
        assert_eq!(quotes[0].symbol, "SPY260914C00760000");
        assert!((quotes[0].ask - quotes[0].bid - 0.01).abs() < 1e-9);
        assert_eq!(quotes[1].right, 'P');
        assert!(
            quotes[1].bid.abs() < 1e-12,
            "a zero bid is kept: it is the answer"
        );
    }

    #[test]
    fn a_quote_missing_a_side_is_refused_rather_than_zeroed() {
        let body = json!({ "snapshots": {
            "SPY260914C00760000": { "latestQuote": { "ap": 2.28, "t": "2026-09-14T18:22:59Z" } }
        }});
        assert!(matches!(
            parse_chain(&body),
            Err(SourceError::Malformed { .. })
        ));
    }

    #[test]
    fn a_row_has_a_value_for_every_header_column() {
        let quote = ChainQuote {
            symbol: "SPY260914C00760000".to_owned(),
            expiration: NaiveDate::from_ymd_opt(2026, 9, 14).expect("valid"),
            right: 'C',
            strike: 760.0,
            quote_at: DateTime::parse_from_rfc3339("2026-09-14T18:22:59Z")
                .expect("valid")
                .with_timezone(&Utc),
            bid: 2.27,
            ask: 2.28,
            bid_size: 147.0,
            ask_size: 47.0,
        };
        let line = row(quote.quote_at, &quote, 762.17, 762.32);
        assert_eq!(line.split(',').count(), HEADER.split(',').count());
        assert!(line.contains(",2.27,2.28,147,47,762.17,762.32"));
    }
}
