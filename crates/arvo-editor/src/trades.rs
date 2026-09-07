//! Every round trip, sortable, and exportable as it stands.
//!
//! The aggregate statistics answer "how did it do". This answers "what did it
//! actually do", and those have different failure modes: an expectancy of
//! +300 built from one +9,000 trade and nineteen losses is a fact only the
//! rows show, and no summary above them will ever say it.
//!
//! Sorting is the point rather than a nicety. The questions a reader has about
//! a ledger are all orderings — what were the worst losses, what was held
//! longest, did the stops cluster — and a fixed chronological table answers
//! none of them.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

use crate::bridge::call_typed;
use crate::views::TradeRowView;

/// What the table is ordered by.
///
/// A key rather than a comparator so the sort survives a re-render: the
/// component holds the choice, not a closure over the rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Instrument,
    Opened,
    Held,
    Quantity,
    Pnl,
    Reason,
}

impl SortBy {
    const fn label(self) -> &'static str {
        match self {
            Self::Instrument => "Instrument",
            Self::Opened => "Opened",
            Self::Held => "Held",
            Self::Quantity => "Size",
            Self::Pnl => "P&L",
            Self::Reason => "Exit",
        }
    }
}

/// ponytail: renders every row. Ledgers run to hundreds of trades, which a
/// browser handles without noticing; windowing is worth it if a strategy ever
/// produces tens of thousands, and not before.
#[component]
pub(crate) fn TradesTable(rows: Vec<TradeRowView>, name: String) -> impl IntoView {
    // Only when the rows come from more than one instrument. On an ordinary
    // study the column would repeat the subject line on every row; on a book,
    // leaving it out means listing round trips from three instruments with no
    // way to tell which is which.
    let per_instrument = {
        let mut seen: Vec<&str> = rows
            .iter()
            .map(|row| row.instrument.as_str())
            .filter(|instrument| !instrument.is_empty())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len() > 1
    };
    // Newest-worst-first is not a default anyone can defend, so it opens in
    // the order the trades happened — the one ordering that is a fact about
    // the run rather than an opinion about it.
    let (sort_by, set_sort_by) = signal(SortBy::Opened);
    let (descending, set_descending) = signal(false);
    let (exported, set_exported) = signal(None::<String>);
    let (error, set_error) = signal(None::<String>);
    let total = rows.len();

    let sorted = {
        let rows = rows.clone();
        move || {
            let mut rows = rows.clone();
            match sort_by.get() {
                SortBy::Opened => rows.sort_by(|a, b| a.opened.cmp(&b.opened)),
                // `total_cmp`, not `partial_cmp().unwrap()`: a NaN anywhere in
                // the column would panic the whole view rather than sort oddly.
                SortBy::Held => rows.sort_by(|a, b| {
                    a.held_days
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&b.held_days.unwrap_or(f64::INFINITY))
                }),
                SortBy::Quantity => rows.sort_by(|a, b| a.quantity.total_cmp(&b.quantity)),
                SortBy::Pnl => rows.sort_by(|a, b| a.pnl.total_cmp(&b.pnl)),
                SortBy::Reason => rows.sort_by(|a, b| a.exit_reason.cmp(&b.exit_reason)),
                SortBy::Instrument => rows.sort_by(|a, b| a.instrument.cmp(&b.instrument)),
            }
            if descending.get() {
                rows.reverse();
            }
            rows
        }
    };

    let header = move |column: SortBy| {
        let active = move || sort_by.get() == column;
        view! {
            <th
                class="research-sortable"
                class:sorted=active
                on:click=move |_| {
                    // Clicking the column already sorted reverses it, which is
                    // what every table does and what a reader will try first.
                    if sort_by.get_untracked() == column {
                        set_descending.update(|value| *value = !*value);
                    } else {
                        set_sort_by.set(column);
                        set_descending.set(matches!(column, SortBy::Pnl | SortBy::Held));
                    }
                }
            >
                {column.label()}
                {move || {
                    active()
                        .then(|| if descending.get() { " \u{25be}" } else { " \u{25b4}" })
                }}
            </th>
        }
    };

    let export = {
        let sorted = sorted.clone();
        let name = name.clone();
        move |_| {
            let rows = sorted();
            let name = name.clone();
            set_error.set(None);
            spawn_local(async move {
                let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                    "name": name,
                    "rows": rows,
                }))
                .unwrap_or(JsValue::UNDEFINED);
                match call_typed::<String>("export_trades", args).await {
                    Ok(path) => set_exported.set(Some(path)),
                    Err(reason) => set_error.set(Some(reason)),
                }
            });
        }
    };

    view! {
        <div class="trades">
            <div class="trades-actions">
                <span class="research-hint">{format!("{total} round trips")}</span>
                <button class="research-linkish" on:click=export>
                    "Export CSV"
                </button>
            </div>
            // Says where it went. A file written somewhere the reader cannot
            // name is a file they will not find again.
            {move || {
                exported
                    .get()
                    .map(|path| view! { <p class="research-hint">{format!("Saved to {path}")}</p> })
            }}
            {move || {
                error.get().map(|reason| view! { <p class="research-flag">{reason}</p> })
            }}

            <div class="research-scroll">
                <table class="research-metrics">
                    <thead>
                        <tr>
                            {per_instrument.then(|| header(SortBy::Instrument))}
                            {header(SortBy::Opened)}
                            <th class="research-left">"Closed"</th>
                            {header(SortBy::Held)}
                            {header(SortBy::Quantity)}
                            <th>"Entry"</th>
                            <th>"Exit"</th>
                            {header(SortBy::Pnl)}
                            <th>"Fees"</th>
                            {header(SortBy::Reason)}
                        </tr>
                    </thead>
                    <tbody>
                        {move || {
                            sorted()
                                .into_iter()
                                .map(|row| {
                                    let tone = if row.pnl > 0.0 {
                                        "research-good"
                                    } else if row.pnl < 0.0 {
                                        "research-bad"
                                    } else {
                                        ""
                                    };
                                    // An open position has no exit and no
                                    // realised result. Dashes rather than
                                    // zeroes: a zero reads as a real
                                    // break-even trade.
                                    let exit = row
                                        .exit
                                        .map_or_else(|| "\u{2014}".to_owned(), |v| format!("{v:.2}"));
                                    let held = row
                                        .held_days
                                        .map_or_else(
                                            || "\u{2014}".to_owned(),
                                            |days| {
                                                if days < 1.0 {
                                                    format!("{:.1}h", days * 24.0)
                                                } else {
                                                    format!("{days:.0}d")
                                                }
                                            },
                                        );
                                    let closed = if row.closed.is_empty() {
                                        "still open".to_owned()
                                    } else {
                                        row.closed.clone()
                                    };
                                    view! {
                                        <tr>
                                            {per_instrument
                                                .then(|| {
                                                    view! {
                                                        <td class="research-left">
                                                            {row.instrument.clone()}
                                                        </td>
                                                    }
                                                })}
                                            <td class="research-left">{row.opened.clone()}</td>
                                            <td class="research-left">{closed}</td>
                                            <td>{held}</td>
                                            <td>{format!("{:.0}", row.quantity)}</td>
                                            <td>{format!("{:.2}", row.entry)}</td>
                                            <td>{exit}</td>
                                            <td class=tone>{format!("{:+.2}", row.pnl)}</td>
                                            <td>{format!("{:.2}", row.commission)}</td>
                                            <td class="research-left">{row.exit_reason.clone()}</td>
                                        </tr>
                                    }
                                })
                                .collect_view()
                        }}
                    </tbody>
                </table>
            </div>
        </div>
    }
}
