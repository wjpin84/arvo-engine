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

/// Every configuration the search tried, not just the one it picked.
///
/// # What the colour means, and why it is not the usual thing
///
/// A heatmap normalised to its own maximum always has one brilliant cell,
/// whatever the numbers behind it — which is exactly the impression this
/// platform exists to resist. So the scale is anchored to the **no-skill bar**
/// instead: the score the best of a search this size would be expected to
/// reach with no edge at all. A cell below the bar is drawn as flat grey,
/// because it is not a weak result, it is *not a result*. Only what clears the
/// bar takes colour, and how much colour is how far past it went.
///
/// The two pictures this makes distinguishable, which a single reported
/// maximum cannot:
///
/// * a **coloured region** — many neighbouring configurations all clearing the
///   bar, so the finding does not depend on the exact parameters;
/// * a **lone bright cell in a grey field** — the shape of a search that found
///   noise, and the shape a report showing only its winner would hide.
#[component]
pub(crate) fn ParameterSurface(surface: SurfaceView) -> impl IntoView {
    let SurfaceView {
        x_axis,
        y_axis,
        x_values,
        y_values,
        cells,
        null_bar,
        best,
        collapsed,
    } = surface;

    // Depth of colour is distance past the bar, as a share of how far the very
    // best cell got past it. With no bar to clear — too few trials to say —
    // nothing is shaded, because there is nothing to shade against.
    let bar = null_bar.unwrap_or(f64::INFINITY);
    let headroom = (best - bar).max(f64::EPSILON);
    let columns = format!(
        "auto repeat({}, minmax(2.4em, 1fr))",
        x_values.len().max(1)
    );

    let lookup = move |x: f64, y: f64| {
        cells
            .iter()
            .find(|cell| {
                (cell.x - x).abs() < f64::EPSILON && (cell.y - y).abs() < f64::EPSILON
            })
            .cloned()
    };

    view! {
        <div class="surface">
            <div class="surface-grid" style=format!("grid-template-columns: {columns}")>
                <span class="surface-corner">{format!("{y_axis} \\ {x_axis}")}</span>
                {x_values
                    .iter()
                    .map(|x| view! { <span class="surface-head">{format!("{x}")}</span> })
                    .collect_view()}
                {y_values
                    .iter()
                    .map(|y| {
                        let y = *y;
                        let row = x_values
                            .iter()
                            .map(|x| {
                                match lookup(*x, y) {
                                    // A configuration that failed to run leaves
                                    // a hole rather than a zero: it did not
                                    // score badly, it did not score.
                                    None => {
                                        view! { <span class="surface-cell empty">"·"</span> }
                                            .into_any()
                                    }
                                    Some(cell) => {
                                        let depth = if cell.above_null {
                                            ((cell.sharpe - bar) / headroom).clamp(0.08, 1.0)
                                        } else {
                                            0.0
                                        };
                                        let style = if depth > 0.0 {
                                            format!(
                                                "background: color-mix(in srgb, \
                                                 var(--color-verdict-supported) {:.0}%, \
                                                 transparent)",
                                                depth * 100.0,
                                            )
                                        } else {
                                            String::new()
                                        };
                                        let class = if cell.selected {
                                            "surface-cell chosen"
                                        } else {
                                            "surface-cell"
                                        };
                                        view! {
                                            <span
                                                class=class
                                                style=style
                                                title=format!(
                                                    "{} {} · {} {} · Sharpe {:.3}{}",
                                                    x_axis, cell.x, y_axis, cell.y, cell.sharpe,
                                                    if cell.above_null {
                                                        ""
                                                    } else {
                                                        " — within what a search this size \
                                                         would reach with no edge"
                                                    },
                                                )
                                            >
                                                {format!("{:.2}", cell.sharpe)}
                                            </span>
                                        }
                                            .into_any()
                                    }
                                }
                            })
                            .collect_view();
                        view! {
                            <span class="surface-head">{format!("{y}")}</span>
                            {row}
                        }
                    })
                    .collect_view()}
            </div>
            <p class="research-hint">
                {match null_bar {
                    Some(bar) => {
                        format!(
                            "Shaded by how far past {bar:.2} a configuration got — the score the \
                             best of {} tries would be expected to reach with no edge at all. \
                             Unshaded cells did not clear it.",
                            x_values.len() * y_values.len(),
                        )
                    }
                    None => "Too few configurations to say what a no-skill search would have \
                             produced, so nothing here is shaded."
                        .to_owned(),
                }}
                {(!collapsed.is_empty())
                    .then(|| {
                        format!(
                            " {} also varied; each cell shows the best score over it.",
                            collapsed.join(", "),
                        )
                    })}
            </p>
        </div>
    }
}

/// What is wrong with the bars a result was produced from.
///
/// Directly under the verdict, because that is what it qualifies. A backtest
/// cannot tell an unadjusted split from a crash, or a stalled feed from a
/// quiet market — it trades both and reports a number either way, and the
/// number looks exactly as convincing as any other.
///
/// A `fault` is something that cannot legitimately be true of a price series,
/// so it is coloured as a failure. A `suspect` is unusual and might be real,
/// which is why nothing here refuses to show the result: whether a 43% fall is
/// a bad print or March 2020 is a judgement about the world that the check
/// cannot make and the reader can.
#[component]
pub(crate) fn DataQuality(findings: Vec<DataFindingView>) -> impl IntoView {
    (!findings.is_empty()).then(|| {
        let faults = findings.iter().filter(|f| f.severity == "fault").count();
        // Hoisted: the `view!` macro will not parse a bare `if` inside an
        // attribute position.
        let headline = if faults > 0 { "research-flag" } else { "research-hint" };
        view! {
            <div class="data-quality">
                <p class=headline>
                    {format!(
                        "{} thing{} to know about the data behind this{}",
                        findings.len(),
                        if findings.len() == 1 { "" } else { "s" },
                        if faults > 0 {
                            format!(" — {faults} cannot be true of a price series")
                        } else {
                            String::new()
                        },
                    )}
                </p>
                <ul class="research-reasons">
                    {findings
                        .into_iter()
                        .map(|finding| {
                            let tone = if finding.severity == "fault" {
                                "research-bad"
                            } else {
                                ""
                            };
                            view! {
                                <li class=tone>
                                    <strong>{finding.kind.clone()}</strong>
                                    {finding.at.map(|at| format!(" {at}")).unwrap_or_default()}
                                    ": "
                                    {finding.detail}
                                </li>
                            }
                        })
                        .collect_view()}
                </ul>
            </div>
        }
    })
}
