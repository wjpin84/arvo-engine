//! What Alpaca is called, what it serves, and how it asks.
//!
//! Two constructors rather than a feed setting, because which feed a bar came
//! from is part of the *data's* identity — see the crate docs.

use arvo_data::source::{Adjustment, Basis, Credential, Feed, Fetched, Source, SourceError};
use arvo_data::{BarInterval, Dividend, IntervalUnit};
use serde_json::Value;

use crate::auth::{get, keys};
use crate::parse::{parse_bars, parse_dividends};

const DATA: &str = "https://data.alpaca.markets";

/// The most bars one request returns. Alpaca's own ceiling.
const PAGE: usize = 10_000;

/// The free plan's feed, and its venue.
pub const IEX_SOURCE_ID: &str = "alpaca-iex";
pub const IEX_VENUE: &str = "AIEX";

/// The paid plan's feed, and its venue.
pub const SIP_SOURCE_ID: &str = "alpaca-sip";
pub const SIP_VENUE: &str = "ASIP";

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

    fn credential(&self) -> Credential {
        // A key pair typed in once, not a flow. Both feeds share one pair:
        // entitlement is a property of the plan, not of the key.
        Credential::Keys
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
        // Alpaca aggregates pre-market and after-hours trades into its bars
        // and has no parameter to leave them out — Robinhood is asked for
        // `bounds=regular` and Yahoo omits them by default. Kept, they would
        // form the opening range from 04:00 prints and break the 390-minute
        // day every annualised figure assumes.
        if interval.is_intraday() {
            bars.retain(|bar| arvo_data::session::in_regular_session(bar.at));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
