//! What Alpaca is called, what it serves, and how it asks.
//!
//! Two constructors rather than a feed setting, because which feed a bar came
//! from is part of the *data's* identity — see the crate docs.

use arvo_data::source::{Adjustment, Basis, Credential, Feed, Fetched, Source, SourceError};
use arvo_data::{BarInterval, Dividend, IntervalUnit};
use serde_json::Value;

use crate::auth::{get_with, keys, Keys};
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

/// Crypto, which is a different endpoint rather than a different feed.
pub const CRYPTO_SOURCE_ID: &str = "alpaca-crypto";
pub const CRYPTO_VENUE: &str = "ACRYPTO";

/// The crypto endpoint's location. Alpaca partitions its crypto venues
/// geographically; `us` is the one US keys are entitled to.
const CRYPTO_LOCATION: &str = "us";

/// Each feed again, total-return adjusted, under venues of their own.
pub const IEX_TOTAL_RETURN_SOURCE_ID: &str = "alpaca-iex-tr";
pub const IEX_TOTAL_RETURN_VENUE: &str = "AIEXTR";
pub const SIP_TOTAL_RETURN_SOURCE_ID: &str = "alpaca-sip-tr";
pub const SIP_TOTAL_RETURN_VENUE: &str = "ASIPTR";

/// The Alpaca source, on one feed or the other, at one adjustment or the other.
///
/// Both are part of the source's identity rather than settings, because both
/// are part of the *data's* identity: see the module note on venues, and
/// ADR-0013 for why a total-return series is a different dataset rather than
/// the same one with a flag.
pub struct Alpaca {
    feed: &'static str,
    adjustment: Adjustment,
    id: &'static str,
    venue: &'static str,
    label: &'static str,
    market: Market,
    keys: KeySource,
}

/// Which of Alpaca's markets a source serves.
///
/// Not a setting: the two are different endpoints, asked for with different
/// symbol spellings, over different calendars. A source serving one cannot
/// serve the other by flipping a flag, which is the same reason a feed is a
/// constructor here rather than a parameter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Market {
    Equities,
    Crypto,
}

/// Where a data call's key pair comes from.
#[derive(Clone)]
enum KeySource {
    /// The keychain, paper first, then the environment. The compiled-in
    /// source.
    Keychain,
    /// Exactly what the caller handed over, and nothing else. The plugin: a
    /// call that arrived with no grant is a call with no session, whatever
    /// the machine the plugin runs on might hold (ADR-0022 point 4).
    Given(Option<Keys>),
}

impl Alpaca {
    /// The free plan: IEX only. See the module note before backtesting on it.
    #[must_use]
    pub const fn iex() -> Self {
        Self {
            feed: "iex",
            adjustment: Adjustment::Split,
            id: IEX_SOURCE_ID,
            venue: IEX_VENUE,
            label: "Alpaca (IEX, free)",
            market: Market::Equities,
            keys: KeySource::Keychain,
        }
    }

    /// The paid plan: every US exchange.
    #[must_use]
    pub const fn sip() -> Self {
        Self {
            feed: "sip",
            adjustment: Adjustment::Split,
            id: SIP_SOURCE_ID,
            venue: SIP_VENUE,
            label: "Alpaca (all exchanges)",
            market: Market::Equities,
            keys: KeySource::Keychain,
        }
    }

    /// The free plan, with every distribution reinvested at the ex-date.
    #[must_use]
    pub const fn iex_total_return() -> Self {
        Self {
            feed: "iex",
            adjustment: Adjustment::TotalReturn,
            id: IEX_TOTAL_RETURN_SOURCE_ID,
            venue: IEX_TOTAL_RETURN_VENUE,
            label: "Alpaca (IEX, free, total return)",
            market: Market::Equities,
            keys: KeySource::Keychain,
        }
    }

    /// The paid plan, with every distribution reinvested at the ex-date.
    #[must_use]
    pub const fn sip_total_return() -> Self {
        Self {
            feed: "sip",
            adjustment: Adjustment::TotalReturn,
            id: SIP_TOTAL_RETURN_SOURCE_ID,
            venue: SIP_TOTAL_RETURN_VENUE,
            label: "Alpaca (all exchanges, total return)",
            market: Market::Equities,
            keys: KeySource::Keychain,
        }
    }

    /// Crypto: coin pairs, around the clock, on the same key pair.
    ///
    /// A different endpoint rather than a different feed, so it takes neither a
    /// feed nor an adjustment — a coin has no splits to adjust for and pays no
    /// distributions, so split-adjusted and total-return are the same series and
    /// [`Adjustment::Split`] is the honest label rather than a choice.
    #[must_use]
    pub const fn crypto() -> Self {
        Self {
            feed: "",
            adjustment: Adjustment::Split,
            id: CRYPTO_SOURCE_ID,
            venue: CRYPTO_VENUE,
            label: "Alpaca (crypto)",
            market: Market::Crypto,
            keys: KeySource::Keychain,
        }
    }

    /// The same source, using only `keys` and never the keychain. What a
    /// plugin builds per call from the grant it was handed: `None` is a call
    /// with no session, not an invitation to look elsewhere.
    #[must_use]
    pub fn with_keys(mut self, keys: Option<Keys>) -> Self {
        self.keys = KeySource::Given(keys);
        self
    }

    /// The pair this call signs with, from wherever this source was told to
    /// look.
    fn keys(&self) -> Result<Option<Keys>, SourceError> {
        match &self.keys {
            KeySource::Keychain => keys(),
            KeySource::Given(given) => Ok(given.clone()),
        }
    }
}

impl Market {
    /// Alpaca's spelling of an Arvo symbol.
    ///
    /// Arvo files a pair as `XRP-USD` because `csv::safe_name` refuses a slash —
    /// that refusal is what stops an instrument id walking out of the data
    /// directory, and it is not worth trading for a spelling. Alpaca asks for
    /// `XRP/USD`, so the slash is put back here, at the one boundary that wants
    /// it. Split on the last dash, the same way the instrument is described.
    fn symbol(self, symbol: &str) -> String {
        match self {
            Self::Equities => symbol.to_owned(),
            Self::Crypto => match symbol.rsplit_once('-') {
                // Encoded, because it is going in a query string.
                Some((base, quote)) => format!("{base}%2F{quote}"),
                None => symbol.to_owned(),
            },
        }
    }
}

/// Alpaca's name for an adjustment basis.
const fn adjustment_parameter(adjustment: Adjustment) -> &'static str {
    match adjustment {
        Adjustment::Split => "split",
        // Splits, dividends and spin-offs: total return.
        Adjustment::TotalReturn => "all",
    }
}

#[async_trait::async_trait]
impl Source for Alpaca {
    fn id(&self) -> &'static str {
        self.id
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn vendor_label(&self) -> &'static str {
        "Alpaca"
    }

    fn provides(&self) -> &'static [&'static str] {
        match self.market {
            // No chains on a coin, and crypto holdings are a separate endpoint
            // that nothing asks for yet.
            Market::Crypto => &["bars"],
            Market::Equities => &["bars", "option quotes", "holdings"],
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
            feed: match self.market {
                // Alpaca aggregates several crypto exchanges into one book, so
                // this is consolidated in the same sense SIP is.
                Market::Crypto => Feed::Consolidated,
                Market::Equities if self.feed == "iex" => Feed::SingleVenue("IEX"),
                Market::Equities => Feed::Consolidated,
            },
            // Always passed explicitly at the call site: Alpaca's default is
            // `raw`, which would make every split read as a crash.
            adjustment: self.adjustment,
        }
    }

    async fn connected(&self) -> Result<bool, SourceError> {
        Ok(self.keys()?.is_some())
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
        // The venue's spelling, which for a pair is not Arvo's.
        let asked = self.market.symbol(symbol);

        // Paginated because one request caps at 10,000 bars and a decade of
        // daily data is more than that at intraday resolutions. Looping until
        // the token is absent rather than a fixed number of times: a partial
        // series that looked complete is the failure this avoids.
        loop {
            // A different endpoint per market, and the crypto one takes neither
            // a feed nor an adjustment: there is one book and nothing to adjust.
            let mut url = match self.market {
                Market::Equities => format!(
                    "{DATA}/v2/stocks/bars?symbols={asked}&timeframe={}&start={from}&end={to}\
                     &adjustment={}&feed={}&limit={PAGE}",
                    spelling(interval)?,
                    adjustment_parameter(self.adjustment),
                    self.feed,
                ),
                Market::Crypto => format!(
                    "{DATA}/v1beta3/crypto/{CRYPTO_LOCATION}/bars?symbols={asked}\
                     &timeframe={}&start={from}&end={to}&limit={PAGE}",
                    spelling(interval)?,
                ),
            };
            if let Some(token) = &page {
                url.push_str(&format!("&page_token={token}"));
            }

            let body = get_with(self.keys()?, &url).await?;
            // Keyed by the symbol as the response spells it, which is the
            // slashed form for a pair.
            bars.extend(parse_bars(&body, &asked.replace("%2F", "/"))?);

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
        //
        // A coin has no outside-the-session to drop, and dropping two thirds of
        // its day here would be the same lie the quality gate stopped telling
        // (arvo-desktop #243): the bars are real, and the calendar is the
        // instrument's.
        if interval.is_intraday() && self.market == Market::Equities {
            bars.retain(|bar| arvo_data::session::in_regular_session(bar.at));
        }
        Ok(Fetched {
            bars,
            // Alpaca returns bars it has and omits the rest; nothing is
            // synthesised, so there is nothing invented to count.
            interpolated: 0,
        })
    }

    /// Alpaca streams one-minute bars; a rule on any number of minutes that
    /// divides the hour gets them aggregated as they close. Anything else
    /// keeps polling. The feed is the one this source fetches from, so a
    /// streamed bar and a fetched one are the same venue's print.
    fn stream(&self, symbol: &str, interval: BarInterval) -> Option<Box<dyn arvo_data::source::BarFeed>> {
        if interval.unit != arvo_data::IntervalUnit::Minute || interval.step == 0 || 60 % interval.step != 0 {
            return None;
        }
        let keys = self.keys().ok().flatten()?;
        Some(Box::new(crate::stream::open(self.feed, symbol.to_owned(), keys, interval.step)))
    }

    async fn dividends(
        &self,
        symbol: &str,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<Dividend>, SourceError> {
        // A coin pays no distributions, so there is nothing to ask and no
        // request spent asking it.
        if self.market == Market::Crypto {
            return Ok(Vec::new());
        }

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

            let body = get_with(self.keys()?, &url).await?;
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
pub(crate) fn spelling(interval: BarInterval) -> Result<String, SourceError> {
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

    /// The spelling boundary. Arvo files a pair with a dash because
    /// `csv::safe_name` refuses a slash; Alpaca asks with a slash. Both halves
    /// have to agree or the fetch writes an empty file and says nothing.
    #[test]
    fn a_pair_is_asked_for_with_a_slash_and_a_share_is_left_alone() {
        assert_eq!(Market::Crypto.symbol("XRP-USD"), "XRP%2FUSD");
        assert_eq!(Market::Crypto.symbol("BTC-USD"), "BTC%2FUSD");
        // Split on the last dash, as the instrument is described.
        assert_eq!(Market::Crypto.symbol("ETH-BTC"), "ETH%2FBTC");
        // Nothing to split: asked for as it stands rather than mangled.
        assert_eq!(Market::Crypto.symbol("BTCUSD"), "BTCUSD");
        // An equity keeps its name, dash and all — BRK-B is a share class.
        assert_eq!(Market::Equities.symbol("BRK-B"), "BRK-B");
        assert_eq!(Market::Equities.symbol("MSFT"), "MSFT");

        // And the response is keyed by what was asked, decoded: the fetch reads
        // `bars["XRP/USD"]`, so a mismatch here is an empty series.
        assert_eq!(Market::Crypto.symbol("XRP-USD").replace("%2F", "/"), "XRP/USD");
    }

    /// A coin files under its own venue, like every other dataset here.
    #[test]
    fn crypto_is_its_own_source_and_venue_and_serves_bars_only() {
        let crypto = Alpaca::crypto();
        assert_eq!(crypto.id(), CRYPTO_SOURCE_ID);
        assert_eq!(crypto.venue(), CRYPTO_VENUE);
        for equities in [Alpaca::iex(), Alpaca::sip()] {
            assert_ne!(crypto.venue(), equities.venue());
            assert_ne!(crypto.id(), equities.id());
        }
        assert_eq!(crypto.provides(), &["bars"], "no chains on a coin");
        assert_eq!(crypto.basis().feed, Feed::Consolidated);

        // The instrument the source serves is described as a pair, around the
        // clock, which is what the annualisation and the quality gate read.
        let described = crypto.instrument("XRP-USD");
        assert_eq!(described.hours, arvo_data::instrument::Hours::Continuous);
        assert!(matches!(
            described.kind,
            arvo_data::instrument::Kind::Crypto { .. }
        ));
    }

    #[test]
    fn the_two_feeds_file_under_different_venues() {
        // Two datasets with two content hashes. Sharing a venue would let a
        // study silently run on whichever was fetched last.
        assert_ne!(Alpaca::iex().venue(), Alpaca::sip().venue());
        assert_ne!(Alpaca::iex().id(), Alpaca::sip().id());
    }

    #[test]
    fn a_total_return_source_asks_for_it_and_declares_it() {
        // The request and the declaration are one fact stated twice, and a
        // file whose basis says one thing while its prices are the other is
        // the error ADR-0013 exists to stop.
        for source in [Alpaca::iex_total_return(), Alpaca::sip_total_return()] {
            assert_eq!(source.basis().adjustment, Adjustment::TotalReturn);
            assert_eq!(adjustment_parameter(source.adjustment), "all");
        }
        for source in [Alpaca::iex(), Alpaca::sip()] {
            assert_eq!(source.basis().adjustment, Adjustment::Split);
            assert_eq!(adjustment_parameter(source.adjustment), "split");
        }
    }

    #[test]
    fn a_total_return_series_keeps_its_feed() {
        assert_eq!(
            Alpaca::iex_total_return().basis().feed,
            Alpaca::iex().basis().feed
        );
        assert_eq!(
            Alpaca::sip_total_return().basis().feed,
            Alpaca::sip().basis().feed
        );
    }

    /// A source handed its keys never looks past them: with none it is not
    /// connected whatever the keychain or the environment holds, and a fetch
    /// is refused as no session before any request is made.
    #[tokio::test]
    async fn a_source_given_its_keys_uses_only_those() {
        let none = Alpaca::iex().with_keys(None);
        assert!(!none.connected().await.expect("asked"));
        let refused = none
            .bars("SPY", BarInterval::DAILY, chrono::NaiveDate::MIN, chrono::NaiveDate::MIN)
            .await
            .expect_err("no keys, no call");
        assert!(refused.needs_sign_in(), "{refused:?}");

        let given = Alpaca::sip().with_keys(Some(Keys { key_id: "k".into(), secret: "s".into() }));
        assert!(given.connected().await.expect("asked"));
    }

    /// The endpoint really answers, and answers on a weekend.
    ///
    /// Ignored because it reaches the network and needs a key pair, like every
    /// other live check here. Run it with
    /// `cargo test -p arvo-alpaca serves_a_coin -- --ignored` after touching the
    /// crypto request: the spelling, the endpoint and the calendar are three
    /// things no offline test can confirm together.
    #[tokio::test]
    #[ignore = "reaches Alpaca and needs keys"]
    async fn alpaca_really_serves_a_coin_including_its_weekends() {
        let source = Alpaca::crypto();
        let to = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
        let from = to - chrono::Duration::days(20);
        let fetched = source
            .bars("BTC-USD", BarInterval::DAILY, from, to)
            .await
            .expect("Alpaca serves BTC/USD daily");

        assert!(fetched.bars.len() > 10, "got {} bars", fetched.bars.len());
        assert!(
            fetched.bars.iter().all(|bar| bar.close > 0.0),
            "every bar has a price"
        );
        // The thing an equity source could never return: a Saturday or a Sunday.
        assert!(
            fetched.bars.iter().any(|bar| matches!(
                chrono::Datelike::weekday(&bar.at.date()),
                chrono::Weekday::Sat | chrono::Weekday::Sun
            )),
            "a coin trades at the weekend and the bars should show it"
        );
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
