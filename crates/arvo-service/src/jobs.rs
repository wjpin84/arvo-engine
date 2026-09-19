//! The jobs that belong to whatever process holds the data.
//!
//! Two of them, and they are here rather than in the window because that is
//! where their work is: one hashes the bar library to find findings that went
//! stale, the other records an option chain from a venue. Both want to keep
//! running when the window is closed (ADR-0018), and the second needs a
//! credential the engine holds (ADR-0028).
//!
//! The window keeps its own jobs — the script runs a person scheduled, whose
//! output goes to its terminal panel — and the Jobs view is the two lists
//! together.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arvo_views::EventView;

use crate::research::ResearchService;
use crate::scheduler::Jobs;

/// Under the data root: one file of option quotes per day.
pub const OPTION_QUOTES_SUBDIR: &str = "option-quotes";

/// Registers the staleness check and the option-chain recorder on `jobs`.
///
/// `raise` is how a job tells someone: the engine broadcasts, so a stale
/// finding reaches every front end that is listening.
pub fn register(jobs: &Jobs, root: &Path, research: Arc<ResearchService>, raise: impl Fn(EventView) + Send + Sync + 'static) {
    // Tell someone when a stored finding goes stale while they are not
    // looking. On a blocking thread: it hashes bar files. The first check runs
    // at startup, which is when "while you were away" is true.
    let reported = root.join(crate::research::staleness::FILE);
    let raise = Arc::new(raise);
    let checking = research.clone();
    jobs.every("staleness", "Check findings for staleness", crate::research::staleness::EVERY, move || {
        let reported = reported.clone();
        let research = checking.clone();
        let raise = raise.clone();
        async move {
            let raised = tokio::task::spawn_blocking(move || {
                crate::research::staleness::check(&research, &reported).map(|event| {
                    let said = event.title.clone();
                    raise(event);
                    said
                })
            })
            .await
            .map_err(|err| format!("the check did not finish: {err}"))?;
            Ok(raised.unwrap_or_else(|| "nothing newly stale".to_owned()))
        }
    });

    // SPY option quotes, kept because no vendor serves them afterwards (#83).
    // Only in the regular session, and only with Alpaca keys: without them
    // there is nothing to record and nothing to say.
    let quotes: PathBuf = root.join(OPTION_QUOTES_SUBDIR);
    jobs.every(
        "option-quotes",
        "Record SPY option quotes",
        std::time::Duration::from_secs(15 * 60),
        move || {
            let dir = quotes.clone();
            async move {
                let now = chrono::Utc::now();
                if !arvo_data::session::in_regular_session(now.naive_utc()) {
                    return Ok("outside the regular session".to_owned());
                }
                match crate::source::alpaca::options::record_chain("SPY", &dir, now).await {
                    Ok(recorded) => Ok(format!("{} contracts recorded", recorded.contracts)),
                    Err(arvo_data::source::SourceError::NoSession { .. }) => Ok("no Alpaca session".to_owned()),
                    Err(err) => Err(format!("could not record option quotes: {err}")),
                }
            }
        },
    );
}
