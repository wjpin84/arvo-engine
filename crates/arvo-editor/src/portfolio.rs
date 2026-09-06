//! What you hold, and what it is worth.
//!
//! Read-only, and there is no credential anywhere in this path — holdings come
//! from a file you export yourself. See `arvo_portfolio::csv` for why that is
//! a deliberate choice rather than a placeholder.

use leptos::prelude::*;

use crate::bridge::{open_study_panel, PORTFOLIO_PANEL_ID};
use crate::chart::{MetricCard, ValueChart};
use crate::format::{money, percent};
use crate::views::*;

/// What you hold: the sidebar picker, and the overview it opens.
///
/// Read-only, and there is no credential anywhere in this path. Holdings come
/// from a file you export yourself — see `arvo_portfolio::csv` for why that is
/// the deliberate choice rather than a placeholder.
#[component]
pub(crate) fn PortfolioSidebar(
    portfolios: ReadSignal<Option<PortfolioLibraryView>>,
    set_open_portfolio: WriteSignal<Option<PortfolioView>>,
) -> impl IntoView {
    view! {
        <div class="sidebar-view">
            <h3>"Portfolio"</h3>
            {move || match portfolios.get() {
                None => view! { <p class="sidebar-empty">"Looking for holdings…"</p> }.into_any(),
                Some(library) if library.portfolios.is_empty() => {
                    view! {
                        <div>
                            <p class="sidebar-empty">"No holdings yet"</p>
                            <p class="research-hint">
                                "Drop a CSV here, one per portfolio, with the header "
                                <code>"instrument,quantity,cost_basis,price"</code>
                                ". Cost basis is the total paid, not per share; price may be blank \
                                 to use the last close from your data. "
                                <code>"CASH"</code>
                                " is worth face value."
                            </p>
                            <p class="research-path">{library.directory.clone()}</p>
                        </div>
                    }
                        .into_any()
                }
                Some(library) => {
                    view! {
                        <ul class="research-instruments">
                            {library
                                .portfolios
                                .into_iter()
                                .map(|portfolio| {
                                    let name = portfolio.name.clone();
                                    let summary = format!(
                                        "{} · {} holdings",
                                        money(portfolio.total_value),
                                        portfolio.holdings.len(),
                                    );
                                    // A 401(k) usually reports no cost basis,
                                    // so there may be no gain to show at all.
                                    let gain = portfolio.unrealized;
                                    let gain_class = match gain {
                                        Some(gain) if gain < 0.0 => "portfolio-delta down",
                                        Some(_) => "portfolio-delta up",
                                        None => "portfolio-delta",
                                    };
                                    let gain_text = gain.map_or_else(
                                        || "cost basis not reported".to_owned(),
                                        money,
                                    );
                                    view! {
                                        <li>
                                            <button
                                                class="research-instrument"
                                                on:click=move |_| {
                                                    set_open_portfolio.set(Some(portfolio.clone()));
                                                    open_study_panel(
                                                        PORTFOLIO_PANEL_ID,
                                                        "Portfolio",
                                                    );
                                                }
                                            >
                                                <span class="research-instrument-id">{name}</span>
                                                <span class="research-instrument-meta">
                                                    {summary}
                                                </span>
                                                <span class=gain_class>{gain_text}</span>
                                            </button>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// The portfolio tab, read back out of the signal so it refreshes in place.
#[component]
pub(crate) fn PortfolioTab(portfolio: ReadSignal<Option<PortfolioView>>) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || {
                portfolio.get().map(|portfolio| view! { <PortfolioReport portfolio=portfolio /> })
            }}
        </div>
    }
}

/// Holdings, what they are worth, and how the money is distributed.
#[component]
pub(crate) fn PortfolioReport(portfolio: PortfolioView) -> impl IntoView {
    let invested = portfolio.total_value - portfolio.cash;
    // Hoisted out of the view: the macro wants a value or a closure in an
    // attribute, never a bare `if`.
    let unrealized_value = portfolio.unrealized.map_or_else(|| "—".to_owned(), money);
    let unrealized_note = portfolio.unrealized_pct.map_or_else(
        || {
            format!(
                "no cost basis on {} holding(s)",
                portfolio.without_cost_basis
            )
        },
        percent,
    );
    let cost_value = portfolio.total_cost.map_or_else(|| "—".to_owned(), money);
    let cost_note = if portfolio.without_cost_basis > 0 {
        "not reported by the source".to_owned()
    } else {
        String::new()
    };
    let cash_note = if portfolio.total_value == 0.0 {
        String::new()
    } else {
        format!(
            "{} of total",
            percent(portfolio.cash / portfolio.total_value)
        )
    };

    // A single observation has nothing to have changed from, and an em dash
    // says that where a "+0.00" would claim a flat day nobody measured.
    let (change_value, change_note, change_tone) = match &portfolio.change {
        None => (
            "—".to_owned(),
            "needs a second day to compare".to_owned(),
            0.0,
        ),
        // Both dates, not just "since X". Snapshots are taken when the
        // portfolio is looked at, so two consecutive ones can be weeks apart —
        // calling that "today's change" would be wrong.
        Some(change) => (
            money(change.absolute),
            format!(
                "{} · {} → {}",
                change.percent.map_or_else(|| "no base".to_owned(), percent),
                change.from.clone(),
                change.to.clone(),
            ),
            change.absolute,
        ),
    };
    let chart_points: Vec<CurvePoint> = portfolio
        .value_history
        .iter()
        .map(|point| CurvePoint {
            time: point.time,
            value: point.value,
        })
        .collect();
    // One point draws a chart with nothing to see; the card already says so.
    let show_chart = chart_points.len() > 1;
    view! {
        <div class="research-report">
            <p class="research-subject">{portfolio.name.clone()}</p>
            <p class="research-instrument-meta">
                {format!("Valued {}", portfolio.as_of.clone())}
            </p>

            <div class="metric-cards">
                <MetricCard label="Total value" value=money(portfolio.total_value) />
                <MetricCard
                    label="Unrealised"
                    value=unrealized_value
                    tone=portfolio.unrealized.unwrap_or(0.0)
                    note=unrealized_note
                />
                <MetricCard label="Cost basis" value=cost_value note=cost_note />
                <MetricCard
                    label="Cash"
                    value=money(portfolio.cash)
                    note=cash_note
                />
                <MetricCard
                    label="Change"
                    value=change_value
                    tone=change_tone
                    note=change_note
                />
                <MetricCard label="Invested" value=money(invested) />
                <MetricCard
                    label="Positions"
                    value=portfolio.holdings.len().to_string()
                />
            </div>

            {show_chart
                .then({
                    let chart_points = chart_points.clone();
                    move || {
                        view! {
                            <h4>"Value over time"</h4>
                            <ValueChart points=chart_points />
                        }
                    }
                })}

            <h4>"Allocation"</h4>
            <div class="allocation">
                {portfolio
                    .holdings
                    .iter()
                    .map(|holding| {
                        let width = format!("{:.2}%", holding.weight * 100.0);
                        view! {
                            <div class="allocation-row">
                                <span class="allocation-label">{holding.instrument.clone()}</span>
                                <span class="allocation-track">
                                    <span class="allocation-bar" style=format!("width:{width}") />
                                </span>
                                <span class="allocation-weight">{percent(holding.weight)}</span>
                            </div>
                        }
                    })
                    .collect_view()}
            </div>

            <h4>"Holdings"</h4>
            <table class="research-metrics">
                <thead>
                    <tr>
                        <th>"Instrument"</th>
                        <th>"Qty"</th>
                        <th>"Price"</th>
                        <th>"Value"</th>
                        <th>"Cost"</th>
                        <th>"Unrealised"</th>
                        <th>"Priced by"</th>
                    </tr>
                </thead>
                <tbody>
                    {portfolio
                        .holdings
                        .iter()
                        .map(|holding| {
                            let gain_class = match holding.unrealized {
                                Some(gain) if gain < 0.0 => "gain-down",
                                Some(_) => "gain-up",
                                None => "",
                            };
                            let gain_text = match holding.unrealized {
                                None => "—".to_owned(),
                                Some(gain) => format!(
                                    "{} {}",
                                    money(gain),
                                    holding
                                        .unrealized_pct
                                        .map_or_else(String::new, |pct| format!("({})", percent(pct))),
                                ),
                            };
                            let cost_text = holding
                                .cost_basis
                                .map_or_else(|| "—".to_owned(), money);
                            // A collective trust reports a balance and no unit
                            // count; an em dash says "not reported" where a 0
                            // would claim the position is empty.
                            let qty_text = holding
                                .quantity
                                .map_or_else(|| "—".to_owned(), |q| format!("{q:.4}"));
                            let price_text = holding
                                .price
                                .map_or_else(|| "—".to_owned(), money);
                            view! {
                                <tr>
                                    <td>{holding.instrument.clone()}</td>
                                    <td>{qty_text}</td>
                                    <td>{price_text}</td>
                                    <td>{money(holding.market_value)}</td>
                                    <td>{cost_text}</td>
                                    <td class=gain_class>{gain_text}</td>
                                    <td class="priced-by">{holding.priced_by.clone()}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>

            <h4>"How this file was read"</h4>
            <p class="research-hint">
                "Column names vary by broker, so they are matched by name and the mapping is \
                 shown here. A wrong guess produces a portfolio that looks entirely plausible; \
                 this is what makes it visible."
            </p>
            <dl class="research-provenance">
                {portfolio
                    .import
                    .columns
                    .iter()
                    .map(|(role, column)| {
                        view! {
                            <dt>{role.clone()}</dt>
                            <dd class="research-hash">{column.clone()}</dd>
                        }
                    })
                    .collect_view()}
                <dt>"Rows imported"</dt>
                <dd>{portfolio.import.rows_imported}</dd>
            </dl>
            {portfolio
                .import
                .cost_basis_derived
                .then(|| {
                    view! {
                        <p class="research-stale">
                            "Cost basis was multiplied up from a per-share column. Check one \
                             holding against your statement before trusting the totals."
                        </p>
                    }
                })}
            {(!portfolio.import.ignored.is_empty())
                .then({
                    let ignored = portfolio.import.ignored.join(", ");
                    move || {
                        view! {
                            <p class="research-hint">{format!("Columns not used: {ignored}")}</p>
                        }
                    }
                })}
            {(!portfolio.import.rows_skipped.is_empty())
                .then({
                    let skipped = portfolio.import.rows_skipped.clone();
                    move || {
                        view! {
                            <div>
                                <p class="research-hint">
                                    "Lines that were not holdings — usually a disclaimer footer:"
                                </p>
                                <ul class="research-reasons">
                                    {skipped
                                        .iter()
                                        .map(|line| view! { <li>{line.clone()}</li> })
                                        .collect_view()}
                                </ul>
                            </div>
                        }
                    }
                })}

            {(!portfolio.unpriced.is_empty())
                .then({
                    let unpriced = portfolio.unpriced.clone();
                    move || {
                        view! {
                            <div>
                                <h4>"Not included"</h4>
                                <p class="research-hint">
                                    "No price was available for these, so they are excluded from \
                                     every total above rather than counted as zero. Add a price \
                                     column, or add their daily bars to the data library."
                                </p>
                                <ul class="research-reasons">
                                    {unpriced
                                        .iter()
                                        .map(|id| view! { <li>{id.clone()}</li> })
                                        .collect_view()}
                                </ul>
                            </div>
                        }
                    }
                })}
        </div>
    }
}
