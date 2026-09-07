//! Live prices for what you hold and what you have data for.
//!
//! The one place in this app where a number moves on its own. Everything else
//! is a valuation off a file or a backtest over bars that have already
//! happened; this is the market as it is right now, which is a different kind
//! of claim and is kept in its own panel for that reason.
//!
//! # What it does not do
//!
//! It does not value your portfolio. A holding is priced from the statement
//! or the last close on disk, deliberately (see `arvo_runtime::portfolio`),
//! and a live price multiplied by a quantity would be a second, disagreeing
//! answer to a question that already has one. This panel marks the rows you
//! hold and stops there.
//!
//! # Two sources, one row
//!
//! The `watchlist` command settles which rows exist and prices them once; the
//! stream moves them after that. A row therefore renders the tick if one has
//! arrived for its symbol and the snapshot otherwise, which is what makes the
//! panel correct in the first frame *and* while the socket is down — a
//! reconnect never blanks a row, it just stops changing it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use crate::bridge::call_typed;
use crate::format::{money, percent};
use crate::views::*;

/// A price move, as something to read.
///
/// Colour alone would leave the sign to be inferred from a palette, which is
/// exactly the inference someone gets wrong on a red-green display.
fn tone(change: Option<f64>) -> &'static str {
    match change {
        Some(value) if value > 0.0 => "watchlist-change up",
        Some(value) if value < 0.0 => "watchlist-change down",
        _ => "watchlist-change",
    }
}

#[component]
pub(crate) fn WatchlistTab(connected: ReadSignal<bool>) -> impl IntoView {
    let (rows, set_rows) = signal(Vec::<QuoteView>::new());
    // Distinguishes "nothing came back" from "we have not asked yet". The
    // first is an empty watchlist, the second is a panel that has only just
    // opened, and an empty-state message shown during the first request reads
    // as an answer when it is not one.
    let (asked, set_asked) = signal(false);
    let (error, set_error) = signal(None::<String>);

    // Filled by the shell's one socket listener, for the life of the window
    // rather than the life of this panel. Reopening the tab therefore shows
    // live prices immediately instead of waiting for the next tick.
    let quotes = expect_context::<RwSignal<std::collections::HashMap<String, QuoteTick>>>();

    let refresh = move || {
        // Nothing to ask with. The status bar and the alerts panel already
        // say why, so this does not repeat the whole explanation.
        if !connected.get_untracked() {
            return;
        }
        spawn_local(async move {
            match call_typed::<Vec<QuoteView>>("watchlist", JsValue::UNDEFINED).await {
                Ok(quotes) => {
                    set_rows.set(quotes);
                    set_error.set(None);
                }
                // Kept, not cleared. A poll that fails on one tick and
                // succeeds on the next should not blank the panel — the last
                // prices are stale, not wrong, and an empty table is a worse
                // answer than an old one with a note on it.
                Err(reason) => set_error.set(Some(reason)),
            }
            set_asked.set(true);
        });
    };

    // Re-runs when the session comes back, so signing in fills the panel
    // rather than leaving it empty until someone thinks to reopen the tab.
    Effect::new(move |_| {
        connected.track();
        refresh();
    });

    view! {
        <div class="watchlist">
            <div class="watchlist-head">
                <h3>"Watchlist"</h3>
                <span class="watchlist-note">
                    {move || {
                        if connected.get() {
                            "What you hold, then what you have data for".to_owned()
                        } else {
                            "Sign in to see live prices".to_owned()
                        }
                    }}
                </span>
            </div>

            // Stale rather than gone: the table below still shows the last
            // prices that arrived, and this says why they stopped moving.
            {move || {
                error
                    .get()
                    .map(|reason| view! { <p class="watchlist-error">{reason}</p> })
            }}

            {move || {
                let live = quotes.get();
                // The snapshot decides the rows; the stream decides the
                // numbers. Merged here, at the point of render, so neither
                // source can be left holding a half-updated copy of the other.
                // The bool is "printed outside the regular session". Carried
                // beside the row rather than in it: it is a property of the
                // tick that produced the number, and the snapshot has no
                // equivalent to claim.
                let rows: Vec<(QuoteView, bool)> = rows
                    .get()
                    .into_iter()
                    .map(|row| match live.get(&row.symbol) {
                        Some(tick) => (
                            QuoteView {
                                price: Some(tick.price),
                                change: tick.change,
                                ..row
                            },
                            !tick.regular,
                        ),
                        None => (row, false),
                    })
                    .collect();
                if rows.is_empty() {
                    let message = if !connected.get() {
                        "No broker session"
                    } else if asked.get() {
                        "Nothing to watch yet — import holdings or fetch an instrument"
                    } else {
                        "Pricing…"
                    };
                    return view! { <p class="sidebar-empty">{message}</p> }.into_any();
                }

                view! {
                    <table class="watchlist-table">
                        <thead>
                            <tr>
                                <th>"Symbol"</th>
                                <th class="watchlist-numeric">"Last"</th>
                                <th class="watchlist-numeric">"Change"</th>
                            </tr>
                        </thead>
                        <tbody>
                            {rows
                                .into_iter()
                                .map(|(quote, extended)| {
                                    view! {
                                        <tr>
                                            <td>
                                                <span class="watchlist-symbol">{quote.symbol}</span>
                                                // Says which rows are yours.
                                                // The same move means something
                                                // different when you own it.
                                                {quote
                                                    .held
                                                    .then(|| {
                                                        view! { <span class="watchlist-held">"held"</span> }
                                                    })}
                                                // A pre- or post-market print,
                                                // on thin volume and a wide
                                                // spread. Marked rather than
                                                // hidden: it is a real trade,
                                                // it is just not the same claim
                                                // as a regular-session price.
                                                {extended
                                                    .then(|| {
                                                        view! { <span class="watchlist-extended">"ext"</span> }
                                                    })}
                                            </td>
                                            <td class="watchlist-numeric">
                                                {quote.price.map_or_else(|| "—".to_owned(), money)}
                                            </td>
                                            <td class=format!("watchlist-numeric {}", tone(quote.change))>
                                                {quote
                                                    .change
                                                    .map_or_else(|| "—".to_owned(), percent)}
                                            </td>
                                        </tr>
                                    }
                                })
                                .collect_view()}
                        </tbody>
                    </table>
                }
                    .into_any()
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::tone;

    #[test]
    fn a_move_carries_its_direction() {
        assert_eq!(tone(Some(0.01)), "watchlist-change up");
        assert_eq!(tone(Some(-0.01)), "watchlist-change down");
    }

    /// Flat and unknown are both "no direction". Colouring an unchanged price
    /// green would be decoration pretending to be information, and colouring
    /// a missing one anything at all would be inventing a move.
    #[test]
    fn flat_and_unknown_are_not_coloured() {
        assert_eq!(tone(Some(0.0)), "watchlist-change");
        assert_eq!(tone(None), "watchlist-change");
    }
}
