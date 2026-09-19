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
use crate::source::{self, Source};

/// The source a command uses when the window did not name one.
///
/// The broker, because it is the one that has to be signed in to and therefore
/// the one a person has already chosen by signing in.
pub fn resolve(id: Option<&str>) -> Result<Box<dyn Source>, CommandError> {
    source::by_id(id.unwrap_or(source::robinhood::SOURCE_ID))
        .map_err(|err| CommandError::Failed(err.to_string()))
}
