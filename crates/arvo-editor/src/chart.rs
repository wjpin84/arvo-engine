//! Drawing: cards, equity curves and the monthly grid.
//!
//! Shared by research and portfolio, which is the only reason they are
//! together — a metric card rendered two ways in one window looks like two
//! different measurements.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

use crate::bridge::{render_equity_chart, render_price_chart, render_underwater_chart};
use crate::format::percent;
use crate::views::*;

/// A headline number with its label, and a sign-coloured variant for the ones
/// where up and down mean good and bad.
#[component]
pub(crate) fn MetricCard(
    label: &'static str,
    value: String,
    #[prop(optional)] tone: Option<f64>,
    #[prop(optional)] note: Option<String>,
) -> impl IntoView {
    // Only where a sign genuinely carries meaning. Colouring a drawdown or a
    // trade count red would be decoration pretending to be information.
    let tone_class = match tone {
        Some(v) if v > 0.0 => "metric-card-value up",
        Some(v) if v < 0.0 => "metric-card-value down",
        _ => "metric-card-value",
    };
    view! {
        <div class="metric-card">
            <div class="metric-card-label">{label}</div>
            <div class=tone_class>{value}</div>
            {note.map(|note| view! { <div class="metric-card-note">{note}</div> })}
        </div>
    }
}

/// The equity curve, strategy against benchmark.
///
/// Mounts into a real element and hands it to the charting library, rather
/// than trying to describe a chart in `view!`. The effect re-runs when the
/// data changes, so an open tab redraws when its study is re-run.
#[component]
pub(crate) fn EquityChart(strategy: Vec<CurvePoint>, benchmark: Vec<CurvePoint>) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        let to_js = |points: &Vec<CurvePoint>| {
            serde_wasm_bindgen::to_value(points).unwrap_or(JsValue::NULL)
        };
        render_equity_chart(&el, to_js(&strategy), to_js(&benchmark));
    });

    view! {
        <div class="equity-chart">
            <div class="equity-chart-legend">
                <span class="equity-chart-key strategy">"Strategy"</span>
                <span class="equity-chart-key benchmark">"Buy and hold"</span>
            </div>
            <div class="equity-chart-canvas" node_ref=holder></div>
        </div>
    }
}

/// A total return says what was earned; this says whether it arrived steadily
/// or in one quarter that will not repeat. Cells are shaded by magnitude
/// relative to the largest move in the table, so a quiet strategy is not
/// rendered as a wall of colour and a violent one is not washed out.
#[component]
pub(crate) fn MonthlyReturns(months: Vec<MonthlyReturnView>) -> impl IntoView {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let mut years: Vec<i32> = months.iter().map(|m| m.year).collect();
    years.sort_unstable();
    years.dedup();

    // Scale to the biggest absolute move present, with a floor so a table of
    // near-zero months does not get amplified into apparent drama.
    let peak = months
        .iter()
        .map(|m| m.value.abs())
        .fold(0.0_f64, f64::max)
        .max(0.01);

    view! {
        <div class="monthly-scroll">
            <table class="monthly">
                <thead>
                    <tr>
                        <th></th>
                        {NAMES.iter().map(|name| view! { <th>{*name}</th> }).collect_view()}
                    </tr>
                </thead>
                <tbody>
                    {years
                        .into_iter()
                        .map(|year| {
                            let cells = (1..=12u32)
                                .map(|month| {
                                    let found = months
                                        .iter()
                                        .find(|m| m.year == year && m.month == month);
                                    match found {
                                        None => view! { <td class="monthly-empty"></td> }.into_any(),
                                        Some(entry) => {
                                            // Opacity carries magnitude, hue carries sign. Two
                                            // channels for two facts, rather than a colour ramp
                                            // that has to be looked up in a legend.
                                            let weight = (entry.value.abs() / peak).clamp(0.12, 1.0);
                                            let class = if entry.value >= 0.0 {
                                                "monthly-cell up"
                                            } else {
                                                "monthly-cell down"
                                            };
                                            view! {
                                                <td
                                                    class=class
                                                    style=format!("--weight:{weight:.3}")
                                                    title=format!(
                                                        "{} {year}: {}",
                                                        NAMES[(month - 1) as usize],
                                                        percent(entry.value),
                                                    )
                                                >
                                                    {format!("{:.1}", entry.value * 100.0)}
                                                </td>
                                            }
                                                .into_any()
                                        }
                                    }
                                })
                                .collect_view();
                            view! {
                                <tr>
                                    <th class="monthly-year">{year}</th>
                                    {cells}
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </div>
    }
}

/// Portfolio value over time.
///
/// Reuses the equity-chart glue with an empty second series rather than adding
/// a second charting path: one series is the degenerate case of two, and a
/// parallel implementation would be a second place for the theme handling and
/// resize behaviour to drift.
#[component]
pub(crate) fn ValueChart(points: Vec<CurvePoint>) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        let series = serde_wasm_bindgen::to_value(&points).unwrap_or(JsValue::NULL);
        let empty =
            serde_wasm_bindgen::to_value(&Vec::<CurvePoint>::new()).unwrap_or(JsValue::NULL);
        render_equity_chart(&el, series, empty);
    });

    view! {
        <div class="equity-chart">
            <div class="equity-chart-legend">
                <span class="equity-chart-key strategy">"Portfolio value"</span>
            </div>
            <div class="equity-chart-canvas" node_ref=holder></div>
        </div>
    }
}

/// The instrument's own bars, with every entry and exit marked on them.
///
/// The chart a trading platform is expected to have and that this one did not:
/// until now Arvo drew equity curves and never once showed a price. It is also
/// the most honest view in the application. A summary can say a rule returned
/// 20%; only this can show that every entry landed in one week, or that the
/// winners came out of a single gap, or that the stop was hit on the wick of
/// bars that closed green.
#[component]
pub(crate) fn PriceChart(candles: Vec<CandlePoint>, markers: Vec<TradeMarkerView>) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();
    let entries = markers.iter().filter(|m| m.kind == "entry").count();
    let stops = markers.iter().filter(|m| m.reason == "stop").count();
    let empty = candles.is_empty();

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        render_price_chart(
            &el,
            serde_wasm_bindgen::to_value(&candles).unwrap_or(JsValue::NULL),
            serde_wasm_bindgen::to_value(&markers).unwrap_or(JsValue::NULL),
        );
    });

    view! {
        <div class="equity-chart">
            <div class="equity-chart-legend">
                <span class="equity-chart-key entry">{format!("{entries} entries")}</span>
                <span class="equity-chart-key stop">{format!("{stops} stopped out")}</span>
            </div>
            // Said rather than left as an empty rectangle. A chart with no
            // data and no explanation is the failure shape this project has
            // already lost time to more than once.
            {empty
                .then(|| {
                    view! {
                        <p class="research-hint">
                            "No bars for this window in the data library."
                        </p>
                    }
                })}
            <div class="equity-chart-canvas" node_ref=holder></div>
        </div>
    }
}

/// How far below its own running peak the account was, at every moment.
///
/// Drawn because one worst-drawdown number cannot distinguish a single deep
/// hole from a decade spent underwater, and those are very different things to
/// have had to sit through — which is the question the drawdown ceiling in the
/// evaluation criteria is really asking.
#[component]
pub(crate) fn UnderwaterChart(points: Vec<CurvePoint>) -> impl IntoView {
    let holder = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let Some(el) = holder.get() else {
            return;
        };
        render_underwater_chart(
            &el,
            serde_wasm_bindgen::to_value(&points).unwrap_or(JsValue::NULL),
        );
    });

    view! { <div class="underwater-chart" node_ref=holder></div> }
}
