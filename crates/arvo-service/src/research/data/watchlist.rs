//! The watchlist: which rows exist, and which of them may be given a price.

use super::*;

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
pub fn watchlist_symbols(held: &BTreeSet<String>, library: Vec<String>) -> Vec<String> {
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
pub fn is_priceable(instrument: &str, held: &BTreeSet<String>) -> bool {
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
pub fn symbol_only(instrument: &str) -> String {
    instrument.split('.').next().unwrap_or(instrument).to_owned()
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
