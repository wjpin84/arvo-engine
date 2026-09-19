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
//!
//! | module | commands |
//! |---|---|
//! | this one | what the library holds, which sources exist |
//! | `watchlist` | the watchlist and which of its rows may be priced |
//! | `fetch` | searching, fetching bars, and cross-checking two sources |

pub mod fetch;
pub mod watchlist;

use super::*;
use crate::source::{self, Credential, Source};

/// The source a command uses when the window did not name one.
///
/// The broker, because it is the one that has to be signed in to and therefore
/// the one a person has already chosen by signing in.
pub fn resolve(id: Option<&str>) -> Result<Box<dyn Source>, CommandError> {
    source::by_id(id.unwrap_or(source::robinhood::SOURCE_ID))
        .map_err(|err| CommandError::Failed(err.to_string()))
}

/// Reports an event to whoever is listening: the engine broadcasts it, the
/// window emits it. A source failure is the only thing these commands raise.
pub type Report<'a> = &'a (dyn Fn(arvo_views::EventView) + Send + Sync);

/// Turns a source failure into a command error, announcing a dead session on
/// the way past.
pub fn failed(report: Report<'_>, err: &source::SourceError) -> CommandError {
    if let Some(event) = crate::events::disconnected(err) {
        report(event);
    }
    CommandError::Failed(err.to_string())
}

/// Everything the library holds, for the workbench's data view.
///
/// # Errors
///
/// The data directory cannot be listed.
pub fn library(service: &ResearchService) -> Result<DataLibraryView, CommandError> {
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
            let coverage = service.bars.coverage(&id, arvo_data::BarInterval::DAILY).ok().flatten();
            let bars = coverage
                .map(|(from, to)| service.bars.daily_bars(&id, from, to).map_or(0, |bars| bars.len()))
                .unwrap_or_default();
            let fingerprint = service.bars.fingerprint(&id, arvo_data::BarInterval::DAILY).ok().flatten();
            InstrumentView {
                id,
                from: coverage.map(|(from, _)| from.to_string()),
                to: coverage.map(|(_, to)| to.to_string()),
                bars,
                fingerprint,
            }
        })
        .collect();
    Ok(DataLibraryView { directory, instruments })
}

/// Every source that can be fetched from, and whether each can fetch right
/// now.
///
/// Never fails as a whole: a source whose credential store cannot be read
/// reports `connected: false` rather than failing the list, so one broken
/// keychain entry cannot hide the sources that need no credential at all.
pub async fn list_sources() -> Vec<SourceView> {
    let mut out = Vec::new();
    for source in source::all() {
        // Asked of the source rather than compared against a known id, and
        // exhaustive, so a third kind of credential cannot be added without
        // deciding what a window should show for it.
        let (needs_sign_in, needs_keys) = match source.credential() {
            Credential::None => (false, false),
            Credential::SignIn => (true, false),
            Credential::Keys => (false, true),
        };
        out.push(SourceView {
            id: source.id().to_owned(),
            label: source.label().to_owned(),
            venue: source.venue().to_owned(),
            connected: source.connected().await.unwrap_or(false),
            needs_sign_in,
            needs_keys,
        });
    }
    out
}
