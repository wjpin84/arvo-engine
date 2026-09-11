//! The data commands: which sources exist, what is held, and pulling more in.
//!
//! # What changed here
//!
//! These used to name Robinhood in their bodies. `fetch_bars` called
//! `feed::fetch` directly, so the Yahoo fetcher — written to be the second
//! opinion `arvo_data::agreement` needs — was reachable only from an example.
//! Every command below now takes a source id and resolves it through
//! [`crate::source::by_id`], so adding a vendor is a `Source` impl and no
//! change here at all.

use super::*;
use crate::source::{self, Source};

/// Lists the instruments the workbench can study.
#[tauri::command]
pub async fn list_instruments(
    service: tauri::State<'_, ResearchService>,
) -> Result<DataLibraryView, CommandError> {
    let directory = service.data_dir.display().to_string();
    let ids = service
        .bars
        .instruments()
        .map_err(|err| CommandError::Failed(format!("reading {directory}: {err}")))?;

    let instruments = ids
        .into_iter()
        .map(|id| {
            // A file that fails to parse should not blank the whole library;
            // it shows as an instrument with no coverage, which is visible
            // and recoverable rather than silently missing.
            let coverage = service
                .bars
                .coverage(&id, arvo_data::BarInterval::DAILY)
                .ok()
                .flatten();
            let bars = coverage
                .map(|(from, to)| {
                    service
                        .bars
                        .daily_bars(&id, from, to)
                        .map_or(0, |bars| bars.len())
                })
                .unwrap_or_default();
            let fingerprint = service
                .bars
                .fingerprint(&id, arvo_data::BarInterval::DAILY)
                .ok()
                .flatten();
            InstrumentView {
                id,
                from: coverage.map(|(from, _)| from.to_string()),
                to: coverage.map(|(_, to)| to.to_string()),
                bars,
                fingerprint,
            }
        })
        .collect();

    Ok(DataLibraryView {
        directory,
        instruments,
    })
}

/// Every source the app can fetch from, and whether each can fetch right now.
///
/// Fetched rather than hardcoded in the window, for the same reason the
/// strategy menu is: a menu that drifts from the backend offers vendors it will
/// then refuse.
///
/// # Errors
///
/// Never fails as a whole. A source whose credential store cannot be read
/// reports `connected: false` rather than failing the list — one broken
/// keychain entry must not hide the sources that need no credential at all.
#[tauri::command]
pub async fn list_sources() -> Result<Vec<SourceView>, CommandError> {
    let mut out = Vec::new();
    for source in source::all() {
        out.push(SourceView {
            id: source.id().to_owned(),
            label: source.label().to_owned(),
            venue: source.venue().to_owned(),
            connected: source.connected().await.unwrap_or(false),
            // Whether signing in is even a thing for this source. Yahoo needs
            // no credential, and offering a "Sign in to Yahoo Finance" button
            // would be offering something that cannot be done.
            needs_sign_in: source.id() == source::robinhood::SOURCE_ID,
        });
    }
    Ok(out)
}

/// Whether a broker connection is stored, so the UI knows what to offer.
///
/// Kept beside [`list_sources`] because the status bar asks only about the one
/// source that has a session to lose.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the keychain cannot be read.
#[tauri::command]
pub fn feed_connected() -> Result<bool, CommandError> {
    source::robinhood::is_connected().map_err(|err| CommandError::Failed(err.to_string()))
}

/// Signs in to the broker: opens the browser and waits for the redirect.
///
/// One command rather than two — begin, then finish — because the flow holds a
/// bound socket and a PKCE verifier between those halves, and parking that in
/// shared state so a second command could find it would mean a half-finished
/// sign-in outliving the window that started it. Here the whole flow lives on
/// one stack and ends when it ends.
///
/// It is therefore a slow command: it returns when someone finishes in their
/// browser, or after [`arvo_oauth::DEFAULT_TIMEOUT`].
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the browser cannot be opened, consent
/// is refused, or nobody completes the sign-in.
#[tauri::command]
pub async fn connect_feed(app: tauri::AppHandle) -> Result<bool, CommandError> {
    use tauri_plugin_opener::OpenerExt as _;

    // Logged at every step. The first attempt at this failed and left no
    // trace anywhere: the error went to the UI and only to the UI, and the UI
    // was showing something else at the time. An authorization flow has five
    // places to fail across two processes and a browser, and "it failed" is
    // not a diagnosis.
    tracing::info!("starting the {} sign-in", source::robinhood::SOURCE_ID);
    let pending = source::robinhood::begin_sign_in().await.map_err(|err| {
        tracing::error!(error = %err, "could not start the sign-in");
        CommandError::Failed(err.to_string())
    })?;
    tracing::info!(
        client_id = %pending.client_id,
        url = %pending.url,
        "registered; waiting for the browser redirect"
    );

    // The system browser, not a window in this app. A sign-in page rendered
    // inside the app cannot be told apart from one the app drew itself, so a
    // user has no way to check what they are typing a password into — which
    // is the whole reason RFC 8252 says to use the external agent.
    app.opener()
        .open_url(pending.url.clone(), None::<&str>)
        .map_err(|err| {
            tracing::error!(error = %err, "could not open a browser");
            CommandError::Failed(format!(
                "could not open a browser for the sign-in ({err}); the address is {}",
                pending.url
            ))
        })?;

    source::robinhood::complete_sign_in(pending).await.map_err(|err| {
        // The server's own words, not a summary of them. An expired grant, a
        // rejected redirect and an unknown parameter are three different
        // problems with three different fixes.
        tracing::error!(error = %err, "the sign-in did not complete");
        CommandError::Failed(err.to_string())
    })?;
    tracing::info!("{} sign-in complete", source::robinhood::SOURCE_ID);
    crate::events::emit(&app, crate::events::feed_connected(source::robinhood::SOURCE_ID));
    Ok(true)
}

/// Forgets the stored broker connection.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the keychain rejects the delete.
#[tauri::command]
pub fn disconnect_feed(app: tauri::AppHandle) -> Result<bool, CommandError> {
    source::robinhood::disconnect().map_err(|err| CommandError::Failed(err.to_string()))?;
    // Announced even though the window already knows — the status bar and the
    // alerts list read the event stream, not the return value, so an
    // un-announced sign-out would leave the status bar claiming a connection
    // that is gone.
    crate::events::emit(
        &app,
        crate::events::feed_disconnected(source::robinhood::SOURCE_ID, "you signed out", true),
    );
    Ok(false)
}

/// How many instruments one watchlist refresh will price.
///
/// A ceiling on the request, not a preference: this is one batch call on a
/// poll, and an unbounded list would grow with the data library until a
/// refresh timed out. Holdings come first, so the cap falls on the library
/// tail rather than on anything you own.
const WATCHLIST_LIMIT: usize = 30;

/// What to price, in the order it matters: what you hold, then what you have
/// data for.
///
/// Deduplicated, because an instrument you both hold and have bars for is one
/// row, and a watchlist that listed it twice would be visibly wrong.
pub(crate) fn watchlist_symbols(held: &BTreeSet<String>, library: Vec<String>) -> Vec<String> {
    let mut chosen: Vec<String> = held.iter().cloned().collect();
    chosen.extend(library.into_iter().filter(|id| !held.contains(id)));

    // By bare ticker, not by instrument id. The rows, the subscription and the
    // arriving tick all key on the ticker, so `AAPL.SCHWAB` from a statement
    // and `AAPL.RH` from a fetch are two ids and one row — and keeping both
    // renders AAPL twice, moving in lockstep, one of them flagged as held.
    let mut seen = std::collections::HashSet::new();
    chosen.retain(|id| seen.insert(symbol_only(id)));

    chosen.truncate(WATCHLIST_LIMIT);
    chosen
}

/// Whether Arvo has grounds to believe this id names something with a market.
///
/// Held instruments came from a real statement and fetched ones came from a
/// vendor that resolved the ticker, so both name securities that exist.
/// Anything else in the library is a file somebody put there, and the venue in
/// its name is whatever they typed.
///
/// Every source's venue counts, not just the broker's. This checked one
/// hardcoded venue when the broker was the only source reachable from the
/// window; leaving it that way once Yahoo became reachable would have quietly
/// refused to price anything fetched from Yahoo — the row would appear and
/// never move, which looks exactly like a dead feed.
///
/// This matters because pricing strips the venue: `symbol_only` turns
/// `DRIFT.SIM` into `DRIFT`, and both the quote call and the socket will
/// happily answer for a listed ticker of that name. A synthetic fixture would
/// then display a real market price, moving, under the name of a series that
/// was generated. That is the worst shape a wrong number can take — it is
/// not implausible, it is not flagged, and it is exactly as convincing as a
/// right one.
///
/// The cost of being wrong the other way is a real instrument in a
/// hand-dropped CSV showing its last close instead of a live price. The row is
/// still there and the backtests are unaffected. That is the cheaper mistake,
/// so it is the one this makes.
pub(crate) fn is_priceable(instrument: &str, held: &BTreeSet<String>) -> bool {
    held.contains(instrument)
        || instrument.rsplit_once('.').is_some_and(|(_, venue)| {
            source::all().iter().any(|source| source.venue() == venue)
        })
}

/// The bare ticker in an instrument id: `AAPL.RH` -> `AAPL`.
///
/// One definition because three things have to agree on it — the row's label,
/// what the stream subscribes to, and the symbol a tick arrives under. Two of
/// those splitting the string themselves is a watchlist that silently stops
/// updating for anything filed under a venue.
pub(crate) fn symbol_only(instrument: &str) -> String {
    instrument.split('.').next().unwrap_or(instrument).to_owned()
}

/// The watchlist: what you hold and what you have data for, priced now.
///
/// # Snapshot here, movement over the stream
///
/// This settles the row set — which instruments, which of them you hold, and
/// a price for each so nothing renders blank — and hands the same symbols to
/// [`crate::stream`], which pushes every subsequent move. The window calls
/// this when the panel opens or a session comes back, not on a timer.
///
/// The two halves are split that way on purpose. A stream that also decided
/// which rows exist would make a socket blip look like a portfolio change,
/// and a poll fast enough to look live is a request per row per tick against
/// a rate limit.
///
/// # Why the window does not pass a list
///
/// There is no stored watchlist to pass. The set is derived from what already
/// exists — holdings first, then the data library — which means it is right on
/// first launch with nothing configured, and cannot drift out of step with a
/// portfolio you re-import. A hand-picked list is a real feature and a
/// different one; it needs somewhere to live and a way to edit it.
///
/// # Why a dead broker session is not an error here
///
/// The stream needs no broker — that is the point of it — and the row set is
/// derived from holdings and the data library, neither of which does either.
/// So a failed snapshot returns the rows unpriced and lets the socket fill
/// them in, rather than emptying a panel that is about to start working. The
/// session failure is still announced on the way past, as everywhere else.
#[tauri::command]
pub async fn watchlist(
    app: tauri::AppHandle,
    service: tauri::State<'_, ResearchService>,
    portfolios: tauri::State<'_, crate::portfolio::PortfolioService>,
    stream: tauri::State<'_, crate::stream::Stream>,
) -> Result<Vec<QuoteView>, CommandError> {
    let held = portfolios.held();
    let library = service.bars.instruments().unwrap_or_default();
    let chosen = watchlist_symbols(&held, library);

    // Only the rows there is any reason to think name a real security. The
    // rest still appear — they are what you have data for — they simply are
    // never claimed to have a market price. See `is_priceable`.
    let priceable: Vec<String> = chosen
        .iter()
        .filter(|id| is_priceable(id, &held))
        .cloned()
        .collect();

    // Before the quote call rather than after: if the broker session is dead
    // the snapshot below fails, and the stream — which needs no broker at all
    // — is the only thing that can still price these rows.
    stream.watch(priceable.iter().map(|id| symbol_only(id)).collect());

    // The broker specifically: it is the source that prices, and the only one
    // with a session that can have died. A `quotes` call on a source that does
    // not offer them reports `Unoffered` and lands in the same branch.
    let broker = source::robinhood::Robinhood;
    let priced: std::collections::HashMap<String, source::Quote> =
        match broker.quotes(&priceable).await {
            Ok(quotes) => quotes
                .into_iter()
                .map(|quote| (quote.instrument.clone(), quote))
                .collect(),
            Err(err) => {
                // Announced, then dropped. Every row below still renders, and
                // the stream prices them within a tick.
                let _ = source_failure(&app, &err);
                std::collections::HashMap::new()
            }
        };

    Ok(chosen
        .into_iter()
        .map(|instrument| QuoteView {
            symbol: symbol_only(&instrument),
            held: held.contains(&instrument),
            price: priced.get(&instrument).map(|quote| quote.price),
            change: priced.get(&instrument).and_then(|quote| quote.change),
            instrument,
        })
        .collect())
}

/// Turns a source failure into a command error, announcing a dead session on
/// the way past.
///
/// Every command that touches a source goes through here rather than mapping
/// the error itself. A session can expire between any two calls, and the
/// command that happens to discover it is not the one a person is looking at —
/// before this, an expired token surfaced as a red line in whichever panel
/// happened to be open, or nowhere at all if the sidebar was closed. Now it
/// reaches the status bar and the alerts list no matter which call found it.
///
/// The source names itself in the error, so this no longer has to be told which
/// vendor it is announcing about — which is what let one hardcoded feed id turn
/// into any number of sources.
pub(crate) fn source_failure(app: &tauri::AppHandle, err: &source::SourceError) -> CommandError {
    let message = err.to_string();
    if let source::SourceError::NoSession { vendor } = err {
        crate::events::emit(
            app,
            crate::events::feed_disconnected(vendor, &message, false),
        );
    }
    CommandError::Failed(message)
}

/// Finds instruments by name or ticker.
///
/// Exists because the alternative was typing `MSFT.NASDAQ` into an empty box
/// and knowing both halves of it — the ticker, and a venue convention that is
/// Arvo's rather than the market's.
///
/// `source` defaults to the broker, which is the only one that offers a search
/// today. A source that does not returns [`source::SourceError::Unoffered`],
/// which says so rather than returning an empty list — "found nothing" and
/// "cannot look" are different answers and a search box must not conflate them.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if there is no connection to the named
/// source or the search call fails.
#[tauri::command]
pub async fn search_instruments(
    app: tauri::AppHandle,
    query: String,
    source: Option<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<Vec<MatchView>, CommandError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let source = resolve(source.as_deref())?;
    let held: std::collections::HashSet<String> =
        service.bars.instruments().unwrap_or_default().into_iter().collect();

    Ok(source
        .search(&service.data_dir, query, 10)
        .await
        .map_err(|err| source_failure(&app, &err))?
        .into_iter()
        .map(|found| MatchView {
            held: held.contains(&found.instrument),
            instrument: found.instrument,
            symbol: found.symbol,
            name: found.name,
            price: found.price,
            change: found.change,
        })
        .collect())
}

/// The source a command uses when the window did not name one.
///
/// The broker, because it is the one that has to be signed in to and therefore
/// the one a person has already chosen by signing in.
pub(crate) fn resolve(id: Option<&str>) -> Result<Box<dyn Source>, CommandError> {
    source::by_id(id.unwrap_or(source::robinhood::SOURCE_ID))
        .map_err(|err| CommandError::Failed(err.to_string()))
}

/// The window's window: how far back a fetch reaches when nobody says.
///
/// Generous for a daily pull and modest for an intraday one, where the same
/// span is two orders of magnitude more bars.
pub(crate) fn default_days(interval: arvo_data::BarInterval) -> u32 {
    if interval.is_intraday() {
        30
    } else {
        3_650
    }
}

/// Pulls one instrument's bars into the data library.
///
/// Fetching is a separate act from running, deliberately. An experiment pins
/// its dataset as a content hash of the bars it ran on, so a provider that went
/// to the network mid-backtest would give a different answer whenever the
/// vendor revised a bar and every stored verdict would quietly stop being
/// checkable. See [`crate::source`].
///
/// `source` is a parameter rather than a constant in this body, which is the
/// whole point of the change that produced it: the second vendor existed for
/// months and could not be reached from the window.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if the source is unknown, there is no
/// connection to it, the resolution is one it does not serve, or nothing comes
/// back.
#[tauri::command]
pub async fn fetch_bars(
    app: tauri::AppHandle,
    instrument: String,
    interval: String,
    days: Option<u32>,
    source: Option<String>,
    service: tauri::State<'_, ResearchService>,
) -> Result<FetchView, CommandError> {
    let parsed: arvo_data::BarInterval = interval
        .parse()
        .map_err(|err| CommandError::Failed(format!("{interval:?}: {err}")))?;

    let source = resolve(source.as_deref())?;
    let days = days.unwrap_or_else(|| default_days(parsed));
    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(i64::from(days));

    let report = source::ingest(&service.data_dir, source.as_ref(), &instrument, parsed, from, to)
        .await
        .map_err(|err| source_failure(&app, &err))?;

    Ok(FetchView {
        instrument: report.instrument,
        source: report.source.to_owned(),
        interval: report.interval.to_string(),
        bars: report.bars,
        interpolated: report.interpolated,
        dividends: report.dividends,
        revision: report.revision.as_ref().map(describe_revision),
        revised: matches!(
            report.revision,
            Some(arvo_data::agreement::Agreement::Diverged { .. })
        ),
        from: report.from.map(|at| at.to_string()),
        to: report.to.map(|at| at.to_string()),
        data_findings: report
            .quality
            .findings
            .into_iter()
            .map(|finding| DataFindingView {
                severity: match finding.severity {
                    arvo_data::quality::Severity::Fault => "fault",
                    arvo_data::quality::Severity::Suspect => "suspect",
                }
                .to_owned(),
                kind: finding.kind.to_owned(),
                at: finding.at.map(|at| at.format("%Y-%m-%d %H:%M").to_string()),
                detail: finding.detail,
            })
            .collect(),
    })
}

/// What two sources say about the same instrument, side by side.
///
/// # The check this platform was missing
///
/// Every data check elsewhere inspects a series against *itself* — a low above
/// a high, a gap where a session should be, a range too wide to be real. Those
/// catch what is impossible. They cannot catch what is merely wrong: a close
/// that is off by forty cents is a perfectly well-formed bar and six internal
/// checks pass it every time.
///
/// It is the same structural weakness the reconciliation invariants were
/// written for. A number checked against a restatement of itself catches
/// nothing; the only independent version of a price is somebody else's.
/// `arvo_data::agreement` was written for exactly this and, until now, had no
/// second vendor to run against from inside the app.
///
/// Neither series is written. This asks a question about two vendors; it does
/// not change what the library holds, and it is deliberately not part of a
/// fetch — a disagreement is something to look at, not something to resolve
/// automatically by preferring whichever source answered second.
///
/// # Errors
///
/// Returns [`CommandError::Failed`] if either source is unknown, cannot be
/// reached, or does not serve the resolution.
#[tauri::command]
pub async fn compare_sources(
    app: tauri::AppHandle,
    instrument: String,
    interval: String,
    first: Option<String>,
    second: Option<String>,
    days: Option<u32>,
) -> Result<SourceComparisonView, CommandError> {
    let parsed: arvo_data::BarInterval = interval
        .parse()
        .map_err(|err| CommandError::Failed(format!("{interval:?}: {err}")))?;

    let first = resolve(first.as_deref())?;
    let second = resolve(second.as_deref().or(Some(source::yahoo::SOURCE_ID)))?;
    if first.id() == second.id() {
        return Err(CommandError::Failed(format!(
            "{} cannot be its own second opinion — a series compared against              itself agrees by construction",
            first.id()
        )));
    }

    let days = days.unwrap_or_else(|| default_days(parsed));
    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(i64::from(days));

    let outcome = source::compare(
        first.as_ref(),
        second.as_ref(),
        &instrument,
        parsed,
        from,
        to,
    )
    .await
    .map_err(|err| source_failure(&app, &err))?;

    Ok(source_comparison_view(&outcome))
}

/// A two-source comparison in words, with the verdict separated from the count.
///
/// Two vendors disagreeing is the normal case, and most of the ways they
/// disagree are not faults — a different adjustment basis, a different session,
/// a different volume basis. Reporting "4,812 bars disagree" across any of those
/// is true, useless, and the kind of thing that gets a check switched off. So
/// the disagreement is classified first and counted second, and only a genuine
/// divergence is flagged.
pub(crate) fn source_comparison_view(outcome: &source::Comparison) -> SourceComparisonView {
    use arvo_data::agreement::Agreement;

    let (summary, diverged) = match &outcome.agreement {
        Agreement::NoOverlap => (
            "no bar instant appears in both series, so there is nothing to compare"
                .to_owned(),
            false,
        ),
        Agreement::Aligned { compared } => (
            format!("all {compared} shared bars agree within tolerance"),
            false,
        ),
        Agreement::Rescaled { factor, compared } => (
            format!(
                "the {compared} shared bars differ by a near-constant factor of {factor:.4} —                  almost certainly a different adjustment basis rather than bad data on either                  side, and they cannot be used together until one is restated"
            ),
            false,
        ),
        Agreement::Diverged {
            disagreeing,
            compared,
            worst,
            at,
        } => (
            format!(
                "{disagreeing} of {compared} shared bars genuinely disagree; the worst is                  {:.2}% on {} — at least one of these sources has prices nobody traded at",
                worst * 100.0,
                at.format("%Y-%m-%d")
            ),
            true,
        ),
    };

    SourceComparisonView {
        symbol: outcome.symbol.clone(),
        interval: outcome.interval.to_string(),
        first: outcome.first.to_owned(),
        second: outcome.second.to_owned(),
        first_bars: outcome.first_bars,
        second_bars: outcome.second_bars,
        shared: outcome.coverage.shared,
        only_first: outcome.coverage.only_first,
        only_second: outcome.coverage.only_second,
        summary,
        diverged,
        basis_mismatch: outcome.basis_mismatch.clone(),
    }
}

/// What a re-fetch changed, in words.
///
/// The distinction that matters is between a *rescaling* and a *revision*. A
/// rescaling is what a corporate action does to a whole series at once: every
/// price moves by the same factor, nothing that happened has been contradicted,
/// and a stored finding is stale only in the sense that its numbers are now
/// quoted in different units. A revision is a source changing its mind about
/// individual prices, and a finding drawn from the old ones rested on
/// something that source no longer stands behind.
pub(crate) fn describe_revision(agreement: &arvo_data::agreement::Agreement) -> String {
    use arvo_data::agreement::Agreement;
    match agreement {
        Agreement::NoOverlap => {
            "covers a different period from the copy already held".to_owned()
        }
        Agreement::Aligned { compared } => {
            format!("matches the {compared} bars already held")
        }
        Agreement::Rescaled { factor, compared } => format!(
            "every one of {compared} bars moved by the same factor of {factor:.4} \u{2014} a \
             corporate action re-adjustment, not a change of mind about any price"
        ),
        Agreement::Diverged {
            disagreeing,
            compared,
            worst,
            at,
        } => format!(
            "{disagreeing} of {compared} bars now hold different prices, the worst by \
             {:.2}% on {at} \u{2014} the source has revised history rather than re-adjusted it",
            worst * 100.0,
        ),
    }
}

#[cfg(test)]
mod watchlist_tests {
    use super::{is_priceable, symbol_only, watchlist_symbols, WATCHLIST_LIMIT};
    use std::collections::BTreeSet;

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn list(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn one_ticker_under_two_venues_is_one_row() {
        // The rows, the subscription and the arriving tick all key on the bare
        // ticker. A statement filed under one venue and a fetch filed under
        // another are two ids and one security, and keeping both renders AAPL
        // twice, moving in lockstep, one of them flagged as held.
        let chosen = watchlist_symbols(
            &set(&["AAPL.SCHWAB"]),
            list(&["AAPL.RH", "MSFT.RH"]),
        );
        let tickers: Vec<String> = chosen.iter().map(|id| symbol_only(id)).collect();
        assert_eq!(tickers, vec!["AAPL", "MSFT"], "{chosen:?}");
        // The held one wins, because holdings are listed first.
        assert_eq!(chosen[0], "AAPL.SCHWAB");
    }

    #[test]
    fn a_synthetic_fixture_is_never_asked_for_a_market_price() {
        // The worst shape a wrong number can take. Pricing strips the venue,
        // so `DRIFT.SIM` is asked for as `DRIFT` — and there is nothing
        // stopping a quote service answering for a listed ticker of that name.
        // The row would then show a real, moving, market price under the name
        // of a series that was generated, as convincing as a right one.
        let held = set(&["MSFT.RH"]);
        assert!(!is_priceable("DRIFT.SIM", &held));
        assert!(!is_priceable("TREND.SIM", &held));
        assert!(!is_priceable("NOISE.SIM", &held));
    }

    #[test]
    fn what_a_vendor_supplied_or_you_actually_hold_is_priceable() {
        // Held came from a real statement; fetched came from a vendor that
        // resolved the ticker. Both name securities that exist.
        let held = set(&["VTSAX.VANGUARD"]);
        assert!(is_priceable("VTSAX.VANGUARD", &held), "a real holding");
        assert!(is_priceable("MSFT.RH", &held), "fetched from the broker");
    }

    #[test]
    fn every_sources_venue_is_priceable_not_just_the_brokers() {
        // This checked one hardcoded venue while the broker was the only source
        // the window could reach. Once a second became reachable, that would
        // have refused to price anything fetched from it — the row appears and
        // never moves, which looks exactly like a dead feed.
        let held = BTreeSet::new();
        for source in crate::source::all() {
            let id = format!("MSFT.{}", source.venue());
            assert!(is_priceable(&id, &held), "{id} came from a real vendor");
        }
    }

    #[test]
    fn a_hand_dropped_csv_still_gets_a_row_even_though_it_gets_no_live_price() {
        // The cheaper mistake, made deliberately. A real instrument in a
        // hand-dropped CSV shows its last close rather than a live price; the
        // row is still there and every backtest is unaffected.
        let chosen = watchlist_symbols(&BTreeSet::new(), list(&["DRIFT.SIM", "MSFT.RH"]));
        assert!(chosen.contains(&"DRIFT.SIM".to_owned()), "{chosen:?}");
        assert!(!is_priceable("DRIFT.SIM", &BTreeSet::new()));
    }

    #[test]
    fn an_instrument_with_no_venue_at_all_is_not_assumed_real() {
        assert!(!is_priceable("AAPL", &BTreeSet::new()));
    }

    #[test]
    fn what_you_hold_comes_first() {
        let chosen = watchlist_symbols(&set(&["MSFT.RH"]), list(&["AAPL.RH", "TSLA.RH"]));
        assert_eq!(chosen.first().map(String::as_str), Some("MSFT.RH"));
        assert_eq!(chosen.len(), 3);
    }

    /// An instrument both held and backed by bars is one row. Listing it
    /// twice would be visibly wrong, and would waste a slot under the cap.
    #[test]
    fn an_instrument_held_and_downloaded_appears_once() {
        let chosen = watchlist_symbols(&set(&["MSFT.RH"]), list(&["MSFT.RH", "AAPL.RH"]));
        assert_eq!(chosen, list(&["MSFT.RH", "AAPL.RH"]));
    }

    /// The cap has to fall on the library tail, never on a position. Someone
    /// with more downloaded instruments than the limit must still see
    /// everything they own.
    #[test]
    fn the_cap_falls_on_the_library_not_on_your_positions() {
        let held = set(&["OWNED1.RH", "OWNED2.RH"]);
        let library: Vec<String> = (0..WATCHLIST_LIMIT + 20)
            .map(|i| format!("LIB{i}.RH"))
            .collect();

        let chosen = watchlist_symbols(&held, library);

        assert_eq!(chosen.len(), WATCHLIST_LIMIT);
        for owned in &held {
            assert!(chosen.contains(owned), "{owned} was dropped for library rows");
        }
    }

    #[test]
    fn nothing_held_and_nothing_downloaded_asks_for_no_quotes() {
        assert!(watchlist_symbols(&BTreeSet::new(), Vec::new()).is_empty());
    }
}
