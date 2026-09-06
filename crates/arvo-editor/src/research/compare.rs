//! Several findings, read against each other.
//!
//! # Why the caveat is at the top rather than the bottom
//!
//! Because this screen is where the mistake happens. Each finding already
//! deflates the grid inside it; none of them knows it is one of six a person
//! is about to pick a winner from. Choosing the best of six is a search of
//! size six, and the best of six no-skill results still looks better than the
//! average of them.
//!
//! A comparison table that simply sorted by return would be the most
//! persuasive way this application could mislead someone, which is why the
//! sentence about it comes before the numbers rather than after.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

use crate::bridge::render_curves;
use crate::format::{percent, ratio, verdict_class};
use crate::views::*;

#[component]
pub(crate) fn ComparisonReport(comparison: ComparisonView) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();
    let curves = comparison.curves.clone();
    let count = comparison.rows.len();
    let survived = comparison.survived_deflation;
    let bar = comparison.expected_best_under_null;
    let best = comparison.best_sharpe;

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        render_curves(
            &el,
            serde_wasm_bindgen::to_value(&curves).unwrap_or(JsValue::NULL),
        );
    });

    let verdict = if survived {
        "research-verdict supported"
    } else {
        "research-verdict refuted"
    };

    view! {
        <div class="research-report">
            <div class=verdict>
                {if survived { "Worth reading" } else { "Selection noise" }}
            </div>
            <p class="research-subject">{format!("{count} findings compared")}</p>

            <p class="research-hint">
                {match (best, bar) {
                    (Some(best), Some(bar)) => {
                        format!(
                            "Best Sharpe here is {best:.2}. The best of {count} results with no \
                             skill at all would be expected to reach {bar:.2}. Picking the winner \
                             of a comparison is a search of the size of the comparison.",
                        )
                    }
                    _ => {
                        "Too few findings to say what the best of a comparison this size would \
                         reach by chance."
                            .to_owned()
                    }
                }}
            </p>

            {(!comparison.notes.is_empty())
                .then(|| {
                    view! {
                        <ul class="research-reasons">
                            {comparison
                                .notes
                                .iter()
                                .map(|note| view! { <li>{note.clone()}</li> })
                                .collect_view()}
                        </ul>
                    }
                })}

            <h4>"Out-of-sample equity"</h4>
            <div class="equity-chart">
                <div class="equity-chart-canvas" node_ref=holder></div>
            </div>

            <h4>"Side by side"</h4>
            <div class="research-scroll">
                <table class="research-metrics">
                    <thead>
                        <tr>
                            <th class="research-left">"Subject"</th>
                            <th class="research-left">"Strategy"</th>
                            <th class="research-left">"Verdict"</th>
                            <th>"Return"</th>
                            <th>"Excess"</th>
                            <th>"Sharpe"</th>
                            <th>"Drawdown"</th>
                            <th>"Trades"</th>
                            <th>"Win rate"</th>
                            <th>"Profit factor"</th>
                        </tr>
                    </thead>
                    <tbody>
                        {comparison
                            .rows
                            .iter()
                            .map(|row| {
                                // Marked, not hidden. A finding measured on
                                // data that has since changed still belongs in
                                // the comparison; it just is not measuring the
                                // same thing as the others any more.
                                let stale = (row.stale == Some(true))
                                    .then_some("research-flag");
                                view! {
                                    <tr>
                                        <td class="research-left">
                                            {row.subject.clone()}
                                            <span class=stale>
                                                {(row.stale == Some(true)).then_some(" stale")}
                                            </span>
                                        </td>
                                        <td class="research-left">{row.strategy_name.clone()}</td>
                                        <td class=verdict_class(&row.verdict)>
                                            {row.verdict.clone()}
                                        </td>
                                        <td>{percent(row.total_return)}</td>
                                        <td>{percent(row.excess_return)}</td>
                                        <td>{ratio(row.sharpe)}</td>
                                        <td>{percent(row.max_drawdown)}</td>
                                        <td>{row.trades}</td>
                                        <td>
                                            {row
                                                .win_rate
                                                .map_or_else(
                                                    || "\u{2014}".to_owned(),
                                                    |value| format!("{:.0}%", value * 100.0),
                                                )}
                                        </td>
                                        <td>{ratio(row.profit_factor)}</td>
                                    </tr>
                                }
                            })
                            .collect_view()}
                    </tbody>
                </table>
            </div>
        </div>
    }
}
