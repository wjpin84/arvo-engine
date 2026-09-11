//! Where bars come from, and the one pipeline they all come through.
//!
//! # Why a trait, when there were two working fetchers already
//!
//! There were two, and they were the same function twice. `feed::fetch` and
//! `yahoo::fetch` each did: ask the vendor, bail if nothing came back, inspect
//! the series, read what was already held, compare, write, build a report.
//! Six of those seven steps have nothing to do with which vendor answered.
//!
//! Duplication was the smaller cost. The larger one was that the vendor was
//! not a *parameter* anywhere — `fetch_bars` named `feed::fetch` in its body,
//! so Yahoo was reachable only from an example, and [`arvo_data::agreement`] —
//! written to compare one vendor's bars against another's — had no second
//! vendor to compare against from inside the app. A cross-check that cannot be
//! run is not a cross-check.
//!
//! So: [`Source`] is the four things that genuinely differ (endpoint, auth,
//! interval spelling, reply shape) and [`ingest`] is the seven-eighths that
//! does not.
//!
//! # Still a fetcher, not a `BarProvider`
//!
//! [`Source`] writes files and stops. Nothing downstream of it reads a vendor;
//! everything reads [`arvo_data::CsvBars`]. That is deliberate and is the
//! platform's central claim: an experiment pins its dataset as a **content
//! hash of the bars it ran on**, which is what makes a stored finding
//! reproducible and staleness detectable. A provider that reached over the
//! network would give a different answer whenever a vendor revised a bar, so
//! the hash would describe nothing and every stored verdict would quietly stop
//! being checkable.
//!
//! Two lesser reasons point the same way. A panel is dozens of backtests over
//! the same bars, so a live provider would re-fetch the same series dozens of
//! times against a rate limit. And a network blip would fail a *backtest*,
//! which is a bad place to discover the internet is down.
//!
//! # Optional methods are a promise about honesty, not convenience
//!
//! [`Source::search`], [`Source::quotes`] and [`Source::dividends`] default to
//! [`SourceError::Unoffered`] rather than to an empty vector. An empty list and
//! "this vendor does not offer that" are different facts, and a source that
//! returned the first when it meant the second would report *no dividends* for
//! an instrument that pays them — which is exactly the bias
//! `arvo_research::advice` exists to warn about.

pub mod alpaca;
pub mod robinhood;
pub mod yahoo;

use std::path::Path;

use arvo_data::{Bar, BarInterval, CsvBars};

/// How much of the market's trading a source's prices cover.
///
/// # Why this cannot be detected from the data
///
/// Because the prices agree. A single-venue feed and the consolidated tape
/// report the same trades at the same prices for anything liquid; what differs
/// is *volume*, by a factor of thirty or more — and [`arvo_data::agreement`]
/// deliberately does not compare volume, for a good reason it states itself:
///
/// > Consolidated tape and primary-exchange volume differ by a factor of three
/// > on data whose prices are identical, so a volume check would fire on every
/// > honest pair and say nothing about whether the prices can be trusted.
///
/// So a thin feed passes cross-validation silently. The only thing that knows
/// is the source, which asked for it — hence a declaration rather than a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// Every venue's prints. Volume is the consolidated tape.
    Consolidated,
    /// One venue's prints, named. The prices are real; **volume, VWAP and
    /// anything else derived from size describe a sample of the market rather
    /// than the market.**
    ///
    /// A rule conditioned on volume, and `vwap_reversion` in particular, is
    /// reading a different instrument from the one it will trade.
    SingleVenue(&'static str),
}

/// What corporate actions a source's prices have been adjusted for.
///
/// # Why this is not a detail
///
/// Two series on different bases differ by a smooth, compounding factor, which
/// [`arvo_data::agreement`] correctly classifies as `Rescaled` — neither side
/// wrong, and not usable together until one is restated. Left undeclared, a
/// cross-check between a split-adjusted and a total-return source reports that
/// on every instrument forever, and a check that always fires is a check nobody
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjustment {
    /// Split-adjusted only. Raw prices make a split look like a crash, which a
    /// breakout rule would trade — so this is the floor, not a choice.
    ///
    /// Dividends are absent from the series and nothing receives them, which is
    /// the bias `arvo_research::dividend` measures.
    Split,
    /// Split- and dividend-adjusted: total return, as though every
    /// distribution were reinvested at the ex-date close.
    ///
    /// This is what a brokerage account with dividend reinvestment actually
    /// does, and it handles a rule that moves in and out correctly — hold
    /// through an ex-date and the adjusted return captures the dividend, be
    /// flat and it does not.
    ///
    /// It is *not* what a cash account does, where the dividend arrives as cash
    /// and sits until something buys with it.
    TotalReturn,
}

/// What a source's prices actually are.
///
/// Declared by the source rather than inferred, because neither axis is
/// visible in the bars — see [`Feed`] and [`Adjustment`]. Two sources on
/// different bases are two datasets, and comparing them without saying so
/// produces a precise answer to the wrong question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Basis {
    pub feed: Feed,
    pub adjustment: Adjustment,
}

impl Basis {
    /// Whether two sources can be compared bar for bar without restatement.
    #[must_use]
    pub fn comparable_with(self, other: Self) -> bool {
        self.adjustment == other.adjustment && self.feed == other.feed
    }

    /// Why two sources cannot be compared, when they cannot.
    ///
    /// `None` when they are on the same basis, which is the case a comparison
    /// can be read at face value.
    #[must_use]
    pub fn mismatch_with(self, other: Self) -> Option<String> {
        if self.adjustment != other.adjustment {
            return Some(format!(
                "one series is {} and the other {}, so they differ by a compounding factor \
                 that is an adjustment difference rather than a disagreement about price",
                self.adjustment.label(),
                other.adjustment.label(),
            ));
        }
        match (self.feed, other.feed) {
            (Feed::Consolidated, Feed::Consolidated) => None,
            (Feed::SingleVenue(a), Feed::SingleVenue(b)) if a == b => None,
            _ => Some(format!(
                "one series is {} and the other {}; the prices are comparable and the \
                 volumes are not, so anything conditioned on size — VWAP above all — is \
                 reading a different market on one side",
                self.feed.label(),
                other.feed.label(),
            )),
        }
    }
}

impl Feed {
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Consolidated => "the consolidated tape".to_owned(),
            Self::SingleVenue(venue) => format!("{venue} only"),
        }
    }
}

impl Adjustment {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Split => "split-adjusted",
            Self::TotalReturn => "total-return adjusted",
        }
    }
}

/// What a source returned for one instrument, before anything is written.
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub bars: Vec<Bar>,
    /// Gap-fill bars the vendor synthesised and this dropped.
    ///
    /// Counted rather than hidden: a series that is a quarter invented is one
    /// to know about before drawing a conclusion from it, and a fabricated
    /// price is the one thing a breakout rule cannot tell from a real one.
    pub interpolated: usize,
}

/// Re-exported so a [`Source`] impl names one type rather than two.
///
/// It lives in `arvo-data` because that is where the data vocabulary lives and
/// because the library reads and writes the series — see
/// [`arvo_data::Dividend`] for why the platform holds them at all.
pub use arvo_data::Dividend;

/// One instrument a source knows about.
#[derive(Debug, Clone)]
pub struct Match {
    /// The Arvo instrument id this would be filed under.
    pub instrument: String,
    pub symbol: String,
    pub name: String,
    /// Last traded price, when the quote lookup succeeded.
    pub price: Option<f64>,
    /// Move since the previous close, as a fraction.
    pub change: Option<f64>,
}

/// One instrument, priced now.
#[derive(Debug, Clone)]
pub struct Quote {
    /// The Arvo instrument id asked for, echoed back so a caller can match a
    /// reply to a row without re-deriving the ticker.
    pub instrument: String,
    pub price: f64,
    /// Move since the adjusted previous close, as a fraction.
    pub change: Option<f64>,
}

/// Everything that can go wrong reaching a source.
///
/// One enum for every source rather than one per source. The two it replaced
/// had eleven variants between them and differed in nothing a caller acted on:
/// each command mapped the whole thing to a string and asked one question of
/// it, which is [`SourceError::needs_sign_in`].
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Nobody signed in, the refresh was rejected, or the server refused the
    /// token.
    ///
    /// Three distinct failures collapsed into one *at the boundary that knows
    /// which is which*, rather than left separate for every caller to
    /// re-classify. They mean the same thing to a person, and a caller checking
    /// for only the one it had seen would leave the other two looking like
    /// network trouble.
    ///
    /// Narrow on purpose. A refresh that could not *reach* the authorization
    /// server is a network problem wearing an auth error's clothing, and is
    /// [`SourceError::Transport`]; reporting it here would sign someone out
    /// over flaky wifi and make them re-authorize in a browser to fix it.
    #[error("not connected to {vendor}; sign in before fetching")]
    NoSession { vendor: &'static str },

    /// The vendor does not serve what was asked for, and no retry will help.
    #[error("{0}")]
    Unsupported(String),

    /// The vendor does not offer this kind of data at all.
    ///
    /// Distinct from an empty result, which means it looked and found none.
    #[error("{vendor} does not serve {what}")]
    Unoffered {
        vendor: &'static str,
        what: &'static str,
    },

    /// Nothing came back for the window asked for.
    #[error("{vendor} returned no bars for {instrument} at {interval}")]
    Empty {
        vendor: &'static str,
        instrument: String,
        interval: String,
    },

    /// The reply arrived and was not the shape the parser reads.
    #[error("{vendor} sent something this does not understand: {detail}")]
    Malformed {
        vendor: &'static str,
        detail: String,
    },

    /// The call did not complete.
    #[error("talking to {vendor}: {detail}")]
    Transport {
        vendor: &'static str,
        detail: String,
    },

    /// The stored credential could not be read, written or understood.
    ///
    /// Not a dead session: the keychain being unavailable is a machine
    /// problem, and a browser sign-in is not the fix for it.
    #[error("the stored {vendor} connection: {detail}")]
    Credential {
        vendor: &'static str,
        detail: String,
    },

    #[error("writing bars: {0}")]
    Write(#[from] arvo_data::DataError),
}

impl SourceError {
    /// Whether this failure means the stored connection is no good.
    ///
    /// Every command that touches a source asks this one question, and the
    /// classification already happened at the boundary — see
    /// [`SourceError::NoSession`].
    #[must_use]
    pub const fn needs_sign_in(&self) -> bool {
        matches!(self, Self::NoSession { .. })
    }
}

/// A place bars can be fetched from.
///
/// `async_trait` rather than native async-in-trait: [`by_id`] hands back a
/// boxed `dyn Source`, and a native `async fn` in a trait is not
/// dyn-compatible. The crate is already in the dependency graph via tonic, so
/// this costs a line in a manifest and no build time.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// Names the source, for the reproducibility record and the keychain.
    fn id(&self) -> &'static str;

    /// What to call it in a menu.
    fn label(&self) -> &'static str;

    /// The venue an instrument fetched here is filed under.
    ///
    /// # Why this is not the listing exchange
    ///
    /// Because the sources do not say, and Arvo will not invent it. It costs
    /// nothing to be honest, because the venue in an Arvo instrument id is a
    /// *namespace*, not a routing destination: it names the simulated exchange
    /// a backtest runs against and it separates two files with the same ticker.
    /// What it has never done is decide where an order goes — there are no
    /// orders.
    ///
    /// Each source gets its own. Two sources' copies of one instrument are two
    /// datasets with two content hashes, and filing them under one name would
    /// let a study silently run on whichever was fetched last.
    fn venue(&self) -> &'static str;

    /// What this source's prices actually are.
    ///
    /// No default. A default would be "consolidated, split-adjusted", which is
    /// a silent wrong answer for precisely the source this exists to catch —
    /// so a new source has to say, and the compiler makes it.
    fn basis(&self) -> Basis;

    /// Whether a usable credential is held.
    ///
    /// `true` for a source that needs none, which is the honest answer to "can
    /// this fetch right now?" rather than a claim about a session.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError`] if the credential store cannot be read.
    async fn connected(&self) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// The bars themselves, writing nothing.
    ///
    /// Separate from [`ingest`] so [`compare`] can hold two series side by side
    /// without either touching the library.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError`] if the call fails, the resolution is not served,
    /// or the reply cannot be read.
    async fn bars(
        &self,
        symbol: &str,
        interval: BarInterval,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Fetched, SourceError>;

    /// Cash distributions over the same window, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Unoffered`] from the default implementation.
    async fn dividends(
        &self,
        _symbol: &str,
        _from: chrono::NaiveDate,
        _to: chrono::NaiveDate,
    ) -> Result<Vec<Dividend>, SourceError> {
        Err(SourceError::Unoffered {
            vendor: self.id(),
            what: "dividends",
        })
    }

    /// Finds instruments by name or ticker.
    ///
    /// `root` is the data library, so a match already held can be filed under
    /// the venue it is already filed under rather than becoming a near
    /// duplicate.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Unoffered`] from the default implementation.
    async fn search(
        &self,
        _root: &Path,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<Match>, SourceError> {
        Err(SourceError::Unoffered {
            vendor: self.id(),
            what: "search",
        })
    }

    /// Prices several instruments in one call.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Unoffered`] from the default implementation.
    async fn quotes(&self, _instruments: &[String]) -> Result<Vec<Quote>, SourceError> {
        Err(SourceError::Unoffered {
            vendor: self.id(),
            what: "quotes",
        })
    }
}

/// What one fetch did, so the caller can say so rather than just succeeding.
#[derive(Debug, Clone)]
pub struct FetchReport {
    /// Which source answered, for the record and for the UI.
    pub source: &'static str,
    pub instrument: String,
    pub interval: BarInterval,
    pub bars: usize,
    /// Gap-fill bars the vendor synthesised, which were dropped.
    pub interpolated: usize,
    /// Cash distributions stored alongside, when the source offers them.
    ///
    /// `None` means the source does not serve dividends at all, which is a
    /// different fact from `Some(0)` — it looked, and the instrument paid none.
    pub dividends: Option<usize>,
    pub from: Option<chrono::NaiveDateTime>,
    pub to: Option<chrono::NaiveDateTime>,
    pub path: std::path::PathBuf,
    /// What is wrong with what arrived.
    pub quality: arvo_data::quality::Report,
    /// How this fetch compares to what was already on disk for the same
    /// instrument and window.
    ///
    /// `None` when there was nothing there to compare against, which is every
    /// first fetch.
    ///
    /// This is the gap that made a re-fetch ambiguous. A vendor that revises
    /// history changes the content hash, which correctly stales every finding
    /// on that instrument — and nothing could say whether the change was a
    /// re-adjustment after a split, which leaves the underlying facts intact,
    /// or a genuine revision, which does not. Those want different responses
    /// and looked identical.
    pub revision: Option<arvo_data::agreement::Agreement>,
}

/// Fetches one instrument through `source` and writes it into the library.
///
/// The whole of what the two hand-written fetchers had in common, in one place,
/// so a third source is a [`Source`] impl and nothing else.
///
/// # Errors
///
/// Returns [`SourceError`] if the call fails, the resolution is not served, or
/// nothing comes back for the window.
pub async fn ingest(
    root: &Path,
    source: &dyn Source,
    symbol: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<FetchReport, SourceError> {
    let symbol = symbol_of(symbol);
    let Fetched { bars, interpolated } = source.bars(symbol, interval, from, to).await?;
    if bars.is_empty() {
        return Err(SourceError::Empty {
            vendor: source.id(),
            instrument: symbol.to_owned(),
            interval: interval.to_string(),
        });
    }

    let instrument = format!("{symbol}.{}", source.venue());

    // Inspected before it is written, so a series that arrives wrong is said so
    // at the moment it arrives rather than the first time a verdict rests on it.
    let quality = arvo_data::quality::inspect(&bars, interval);

    // Against what is already held, before it is overwritten. Reading after the
    // write would compare the new series against itself, which is the failure
    // this whole comparison exists to avoid.
    let library = CsvBars::new(root);
    let revision = arvo_data::BarProvider::bars(&library, &instrument, interval, from, to)
        .ok()
        .filter(|held| !held.is_empty())
        .map(|held| arvo_data::agreement::compare(&held, &bars).0);

    let path = library.write(&instrument, interval, &bars)?;

    // After the bars are safely down. A source that serves prices but not
    // distributions is the normal case, not a failed fetch — and a dividend
    // call that fell over must not throw away a good price series.
    let dividends = match source.dividends(symbol, from, to).await {
        Ok(paid) => {
            // Written through the library rather than here: it owns the bar
            // format and now owns this one, so there is one writer and one
            // reader to keep in step rather than two of each.
            library.write_dividends(&instrument, &paid)?;
            Some(paid.len())
        }
        Err(SourceError::Unoffered { .. }) => None,
        Err(err) => {
            tracing::warn!(
                source = source.id(),
                instrument = %instrument,
                error = %err,
                "bars were written but dividends could not be fetched"
            );
            None
        }
    };

    Ok(FetchReport {
        source: source.id(),
        instrument,
        interval,
        bars: bars.len(),
        interpolated,
        dividends,
        from: bars.first().map(|bar| bar.at),
        to: bars.last().map(|bar| bar.at),
        path,
        quality,
        revision,
    })
}

/// Two vendors' answers to the same question, and how far apart they are.
#[derive(Debug, Clone)]
pub struct Comparison {
    pub symbol: String,
    pub interval: BarInterval,
    pub first: &'static str,
    pub second: &'static str,
    pub first_bars: usize,
    pub second_bars: usize,
    /// Why the two cannot be compared at face value, when they cannot.
    ///
    /// `None` means the two are on the same basis and the agreement below says
    /// what it appears to say.
    pub basis_mismatch: Option<String>,
    pub agreement: arvo_data::agreement::Agreement,
    pub coverage: arvo_data::agreement::Coverage,
}

/// What two sources say about the same instrument over the same window.
///
/// The check [`arvo_data::agreement`] was written for and has never been able
/// to run from inside the app. Every quality check elsewhere inspects a series
/// against *itself*, which catches what is impossible and cannot catch what is
/// merely wrong: a close that is off by forty cents is a perfectly well-formed
/// bar and six internal checks pass it every time. The only independent version
/// of a price is somebody else's.
///
/// Neither series is written. This asks a question about two vendors; it does
/// not change what the library holds.
///
/// # Errors
///
/// Returns [`SourceError`] if either source fails.
pub async fn compare(
    first: &dyn Source,
    second: &dyn Source,
    symbol: &str,
    interval: BarInterval,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<Comparison, SourceError> {
    let symbol = symbol_of(symbol);
    let left = first.bars(symbol, interval, from, to).await?;
    let right = second.bars(symbol, interval, from, to).await?;
    let (agreement, coverage) = arvo_data::agreement::compare(&left.bars, &right.bars);
    Ok(Comparison {
        symbol: symbol.to_owned(),
        interval,
        first: first.id(),
        second: second.id(),
        first_bars: left.bars.len(),
        second_bars: right.bars.len(),
        // Read from what the sources declare, never from the bars: neither axis
        // is visible in them. That is the whole reason this is a declaration.
        basis_mismatch: first.basis().mismatch_with(second.basis()),
        agreement,
        coverage,
    })
}

/// Every source the app can fetch from, in the order a menu should list them.
///
/// A function rather than a registry struct: there are two, they are known at
/// compile time, and a registry with a map and a registration call would be
/// ceremony around a `vec!`. It becomes a real registry when a source arrives
/// that this crate does not own — see `arvo_plugin_host`.
#[must_use]
pub fn all() -> Vec<Box<dyn Source>> {
    vec![
        Box::new(robinhood::Robinhood),
        Box::new(yahoo::Yahoo),
        // Both Alpaca feeds, because which one a key is entitled to is not
        // knowable without asking, and the two are different datasets rather
        // than one source configured two ways.
        Box::new(alpaca::Alpaca::iex()),
        Box::new(alpaca::Alpaca::sip()),
    ]
}

/// One source by the id it reports.
///
/// # Errors
///
/// Returns [`SourceError::Unsupported`] naming what is available, rather than
/// `None`: a caller that got `None` would have to invent that message itself,
/// and the call sites would drift.
pub fn by_id(id: &str) -> Result<Box<dyn Source>, SourceError> {
    all()
        .into_iter()
        .find(|source| source.id() == id)
        .ok_or_else(|| {
            let known: Vec<&str> = all().iter().map(|source| source.id()).collect();
            SourceError::Unsupported(format!(
                "{id:?} is not a source this knows; try one of {}",
                known.join(", ")
            ))
        })
}

/// The ticker out of an Arvo instrument id: `MSFT.NASDAQ` is `MSFT`.
///
/// Shared rather than one copy per source, which is what it was. Applied by
/// [`ingest`] and [`compare`] to whatever they are handed, so a caller may pass
/// either spelling and a re-fetch of `MSFT.YF` from Robinhood lands on
/// `MSFT.RH` rather than on `MSFT.YF.RH`.
#[must_use]
pub fn symbol_of(instrument: &str) -> &str {
    instrument.split('.').next().unwrap_or(instrument)
}

/// Which venue an already-held symbol is filed under.
///
/// So a second fetch of something already in the library lands beside the first
/// rather than becoming a near-duplicate under a different name — a library
/// holding both `MSFT.NASDAQ` and `MSFT.RH` is two datasets whose difference
/// nobody can see and every comparison between them is wrong.
pub(crate) fn existing_venues(root: &Path) -> std::collections::HashMap<String, String> {
    CsvBars::new(root)
        .instruments()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|instrument| {
            let (symbol, venue) = instrument.split_once('.')?;
            Some((symbol.to_owned(), venue.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that answers from a fixture, so `ingest` can be tested without
    /// a network.
    struct Canned {
        id: &'static str,
        venue: &'static str,
        basis: Basis,
        bars: Vec<Bar>,
        dividends: Option<Vec<Dividend>>,
    }

    #[async_trait::async_trait]
    impl Source for Canned {
        fn id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            "Canned"
        }
        fn venue(&self) -> &'static str {
            self.venue
        }
        fn basis(&self) -> Basis {
            self.basis
        }
        async fn bars(
            &self,
            _symbol: &str,
            _interval: BarInterval,
            _from: chrono::NaiveDate,
            _to: chrono::NaiveDate,
        ) -> Result<Fetched, SourceError> {
            Ok(Fetched {
                bars: self.bars.clone(),
                interpolated: 2,
            })
        }
        async fn dividends(
            &self,
            _symbol: &str,
            _from: chrono::NaiveDate,
            _to: chrono::NaiveDate,
        ) -> Result<Vec<Dividend>, SourceError> {
            self.dividends.clone().ok_or(SourceError::Unoffered {
                vendor: self.id,
                what: "dividends",
            })
        }
    }

    fn series(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(index, close)| Bar {
                at: chrono::NaiveDate::from_ymd_opt(2024, 1, u32::try_from(index).unwrap() + 1)
                    .unwrap()
                    .and_time(chrono::NaiveTime::MIN),
                open: *close,
                high: *close,
                low: *close,
                close: *close,
                volume: 1_000.0,
            })
            .collect()
    }

    /// What both shipped sources declare, so a fixture is comparable by
    /// default and only says otherwise when a test means it to.
    fn consolidated_split() -> Basis {
        Basis {
            feed: Feed::Consolidated,
            adjustment: Adjustment::Split,
        }
    }

    fn canned(id: &'static str, venue: &'static str, closes: &[f64]) -> Canned {
        Canned {
            id,
            venue,
            basis: consolidated_split(),
            bars: series(closes),
            dividends: None,
        }
    }

    fn window() -> (chrono::NaiveDate, chrono::NaiveDate) {
        (
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
        )
    }

    #[tokio::test]
    async fn ingest_files_under_the_sources_own_venue() {
        // The property that keeps two vendors' copies of one ticker apart. If
        // both landed on one name, a study would silently run on whichever was
        // fetched last.
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let report = ingest(
            root.path(),
            &canned("acme", "AC", &[1.0, 2.0, 3.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();

        assert_eq!(report.instrument, "MSFT.AC");
        assert_eq!(report.source, "acme");
        assert_eq!(report.bars, 3);
        assert_eq!(report.interpolated, 2, "carried through, not swallowed");
        assert!(report.path.exists());
    }

    #[tokio::test]
    async fn an_id_that_already_carries_a_venue_is_refiled_not_double_suffixed() {
        // Re-fetching `MSFT.YF` from another source must land on that source's
        // own venue, not produce `MSFT.YF.AC`.
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let report = ingest(
            root.path(),
            &canned("acme", "AC", &[1.0, 2.0]),
            "MSFT.YF",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();
        assert_eq!(report.instrument, "MSFT.AC");
    }

    #[tokio::test]
    async fn a_first_fetch_has_nothing_to_compare_against() {
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let report = ingest(
            root.path(),
            &canned("acme", "AC", &[1.0, 2.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();
        assert!(report.revision.is_none());
    }

    #[tokio::test]
    async fn a_refetch_compares_against_what_was_held_not_against_itself() {
        // The ordering bug this exists to prevent: reading after the write
        // compares the new series against a copy of itself and always agrees.
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        ingest(
            root.path(),
            &canned("acme", "AC", &[10.0, 20.0, 30.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();

        let report = ingest(
            root.path(),
            &canned("acme", "AC", &[10.0, 20.0, 44.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();

        assert!(
            matches!(
                report.revision,
                Some(arvo_data::agreement::Agreement::Diverged { .. })
            ),
            "a rewritten history must read as a revision, got {:?}",
            report.revision
        );
    }

    #[tokio::test]
    async fn an_empty_series_is_an_error_rather_than_an_empty_file() {
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let err = ingest(
            root.path(),
            &canned("acme", "AC", &[]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SourceError::Empty { .. }));
    }

    #[tokio::test]
    async fn a_source_with_no_dividends_reports_absence_not_zero() {
        // `None` means the vendor does not serve them; `Some(0)` means it
        // looked and the instrument paid none. Collapsing the two would report
        // "no dividends" for an instrument that pays them.
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let report = ingest(
            root.path(),
            &canned("acme", "AC", &[1.0, 2.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();
        assert_eq!(report.dividends, None);
    }

    #[tokio::test]
    async fn dividends_are_written_beside_the_bars_not_into_them() {
        // A column on the bar file would change the content hash of every
        // series in the library and stale every stored finding at once — and
        // a sibling file in the root would be listed as an instrument, which
        // is why `arvo_data` puts them in their own directory.
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let source = Canned {
            id: "acme",
            venue: "AC",
            basis: consolidated_split(),
            bars: series(&[1.0, 2.0]),
            dividends: Some(vec![
                Dividend {
                    ex_date: chrono::NaiveDate::from_ymd_opt(2024, 2, 9).unwrap(),
                    amount: 0.24,
                },
                Dividend {
                    ex_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 10).unwrap(),
                    amount: 0.25,
                },
            ]),
        };

        let report = ingest(root.path(), &source, "MSFT", BarInterval::DAILY, from, to)
            .await
            .unwrap();

        assert_eq!(report.dividends, Some(2));
        let text =
            std::fs::read_to_string(root.path().join("dividends").join("MSFT.AC.csv")).unwrap();
        assert_eq!(text, "ex_date,amount\n2024-02-09,0.24\n2024-05-10,0.25\n");

        let bars = std::fs::read_to_string(&report.path).unwrap();
        assert!(
            !bars.contains("dividend"),
            "the bar file must be untouched by this"
        );
        assert_eq!(
            arvo_data::CsvBars::new(root.path()).instruments().unwrap(),
            vec!["MSFT.AC"],
            "the distribution file must not read as an instrument"
        );
    }

    #[tokio::test]
    async fn comparing_two_sources_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (from, to) = window();
        let outcome = compare(
            &canned("acme", "AC", &[1.0, 2.0, 3.0]),
            &canned("beta", "BT", &[1.0, 2.0, 3.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome.agreement,
            arvo_data::agreement::Agreement::Aligned { compared: 3 }
        ));
        assert_eq!(outcome.first, "acme");
        assert_eq!(outcome.second, "beta");
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            0,
            "a comparison asks a question; it does not change the library"
        );
    }

    #[tokio::test]
    async fn two_vendors_that_disagree_are_reported_as_disagreeing() {
        // The whole point of a second source: an internal check cannot catch a
        // close that is merely wrong.
        let (from, to) = window();
        let outcome = compare(
            &canned("acme", "AC", &[10.0, 20.0, 30.0]),
            &canned("beta", "BT", &[10.0, 20.0, 44.0]),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome.agreement,
            arvo_data::agreement::Agreement::Diverged { .. }
        ));
    }


    fn on(feed: Feed, adjustment: Adjustment) -> Basis {
        Basis { feed, adjustment }
    }

    /// A source declaring whatever basis a test needs.
    fn declaring(id: &'static str, venue: &'static str, basis: Basis) -> Canned {
        Canned {
            id,
            venue,
            basis,
            bars: series(&[10.0, 20.0, 30.0]),
            dividends: None,
        }
    }

    #[test]
    fn the_two_shipped_sources_are_comparable_with_each_other() {
        // The property that keeps `compare_sources` meaningful. If either
        // source ever changes basis — asking Yahoo for `adjclose`, or Alpaca
        // for the free IEX feed — this fails, which is the point: the change
        // would otherwise make every cross-check report a rescaling forever
        // and nobody would know why.
        let broker = robinhood::Robinhood.basis();
        let second = yahoo::Yahoo.basis();
        assert!(
            broker.comparable_with(second),
            "{broker:?} against {second:?}"
        );
        assert_eq!(broker.mismatch_with(second), None);
    }

    #[tokio::test]
    async fn two_sources_on_the_same_basis_report_no_mismatch() {
        let (from, to) = window();
        let outcome = compare(
            &declaring("a", "A", on(Feed::Consolidated, Adjustment::Split)),
            &declaring("b", "B", on(Feed::Consolidated, Adjustment::Split)),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();
        assert_eq!(outcome.basis_mismatch, None);
    }

    #[tokio::test]
    async fn a_thin_feed_is_named_even_though_the_prices_agree() {
        // The finding this whole type exists for. The prices are identical, so
        // `agreement` says Aligned and means it — and one side is a fraction of
        // the tape, which nothing in the bars can reveal.
        let (from, to) = window();
        let outcome = compare(
            &declaring("broker", "BR", on(Feed::Consolidated, Adjustment::Split)),
            &declaring("thin", "TH", on(Feed::SingleVenue("IEX"), Adjustment::Split)),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();

        assert!(
            matches!(
                outcome.agreement,
                arvo_data::agreement::Agreement::Aligned { .. }
            ),
            "the prices genuinely agree, which is exactly the trap"
        );
        let why = outcome
            .basis_mismatch
            .expect("one side is a single venue and that has to be said");
        assert!(why.contains("IEX"), "{why}");
        assert!(
            why.contains("VWAP"),
            "it should name what breaks, not just that something does: {why}"
        );
    }

    #[tokio::test]
    async fn a_different_adjustment_is_named_as_an_adjustment() {
        // Two bases differ by a compounding factor, which `agreement` correctly
        // calls a rescaling — neither side wrong. Undeclared, that fires on
        // every instrument forever, and a check that always fires is a check
        // nobody reads.
        let (from, to) = window();
        let outcome = compare(
            &declaring("split", "SP", on(Feed::Consolidated, Adjustment::Split)),
            &declaring("total", "TR", on(Feed::Consolidated, Adjustment::TotalReturn)),
            "MSFT",
            BarInterval::DAILY,
            from,
            to,
        )
        .await
        .unwrap();

        let why = outcome.basis_mismatch.expect("different bases");
        assert!(why.contains("split-adjusted"), "{why}");
        assert!(why.contains("total-return adjusted"), "{why}");
    }

    #[test]
    fn the_adjustment_is_reported_ahead_of_the_feed() {
        // Both wrong is possible. The adjustment is the one that makes every
        // bar differ, so it is the one to fix first — reporting the feed
        // instead would send a reader after the smaller problem.
        let split_thin = on(Feed::SingleVenue("IEX"), Adjustment::Split);
        let total_wide = on(Feed::Consolidated, Adjustment::TotalReturn);
        let why = split_thin.mismatch_with(total_wide).expect("both differ");
        assert!(why.contains("total-return adjusted"), "{why}");
    }

    #[test]
    fn the_same_single_venue_on_both_sides_is_comparable() {
        // Two sources reading the same thin feed agree about volume as well as
        // price. The comparison is narrow, and it is not mismatched.
        let iex = on(Feed::SingleVenue("IEX"), Adjustment::Split);
        assert!(iex.comparable_with(iex));
        assert_eq!(iex.mismatch_with(iex), None);
    }

    #[test]
    fn an_unknown_source_names_the_ones_that_exist() {
        let Err(SourceError::Unsupported(message)) = by_id("bloomberg") else {
            panic!("an unknown id is unsupported");
        };
        assert!(message.contains("robinhood"), "{message}");
        assert!(message.contains("yahoo"), "{message}");
    }

    #[test]
    fn every_source_has_its_own_venue_and_id() {
        // Two sources sharing a venue would file two datasets under one name.
        let sources = all();
        let ids: std::collections::BTreeSet<_> = sources.iter().map(|s| s.id()).collect();
        let venues: std::collections::BTreeSet<_> = sources.iter().map(|s| s.venue()).collect();
        assert_eq!(ids.len(), sources.len(), "ids collide");
        assert_eq!(venues.len(), sources.len(), "venues collide");
    }

    #[test]
    fn only_a_dead_session_asks_for_a_sign_in() {
        // The three failures that all mean "sign in again" are collapsed at
        // the boundary; a refresh that could not reach the network is not one
        // of them and must not sign anyone out.
        assert!(SourceError::NoSession { vendor: "rh" }.needs_sign_in());
        assert!(!SourceError::Unsupported("3-minute bars".into()).needs_sign_in());
        assert!(!SourceError::Transport {
            vendor: "rh",
            detail: "connection reset".into(),
        }
        .needs_sign_in());
    }
}
