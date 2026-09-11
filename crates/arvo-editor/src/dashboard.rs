//! The first thing you see.
//!
//! The Welcome tab used to be a wordmark, a tagline and nothing else, and the
//! window opened with no panels at all. That is right for an editor — VS Code
//! genuinely knows nothing until you open a folder — and wrong here, because
//! this app already knows what you hold, whether it can reach a broker, and
//! what the last thing it concluded was. All of it sat one click inside a
//! sidebar that starts closed.
//!
//! Everything here comes from commands that already existed. No new backend,
//! and deliberately so: a home screen that needs its own endpoints is a home
//! screen that can disagree with the views it summarises.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use crate::bridge::{call_typed, open_study_panel, PORTFOLIO_PANEL_ID, WATCHLIST_PANEL_ID};
use crate::chart::MetricCard;
use crate::format::{money, percent, verdict_dot};
use crate::views::*;

/// One thing that is currently wrong.
///
/// Current state rather than a feed of events, for the reason the alerts
/// badge learned the hard way: a plugin that was unreachable on the first
/// probe never *transitioned*, so it never announced itself, and a home
/// screen that listed only announcements would call a broken system healthy.
pub(crate) struct Problem {
    pub(crate) title: String,
    pub(crate) detail: String,
    /// Whether signing in is what fixes it. The one problem the app can act
    /// on directly, so the one that carries a button — an unreachable plugin
    /// is fixed by starting the plugin, which is not this window's to do.
    pub(crate) sign_in: bool,
}

/// What is wrong right now, in the order someone should care.
///
/// A dead broker session first: it stops market data and instrument search
/// outright, where an unreachable plugin stops one capability.
pub(crate) fn problems(plugins: &[PluginView], feed_held: bool) -> Vec<Problem> {
    let mut found = Vec::new();

    if !feed_held {
        found.push(Problem {
            title: "Not signed in to Robinhood".to_owned(),
            detail: "Market data and instrument search need a broker session.".to_owned(),
            sign_in: true,
        });
    }

    found.extend(plugins.iter().filter_map(|plugin| match &plugin.status {
        PluginStatusView::Unreachable { reason } => Some(Problem {
            title: format!("{} unreachable", plugin.id),
            detail: reason.clone(),
            sign_in: false,
        }),
        PluginStatusView::Reachable { .. } => None,
    }));

    found
}

/// Every portfolio added up: value, unrealized gain, holdings.
///
/// `None` where nothing has been imported — distinct from a total of zero,
/// which is a real answer about a real portfolio and must not read the same
/// as having no data at all.
fn totals(library: Option<&PortfolioLibraryView>) -> Option<(f64, Option<f64>, usize)> {
    let library = library?;
    if library.portfolios.is_empty() {
        return None;
    }

    let value = library.portfolios.iter().map(|p| p.total_value).sum();
    let holdings = library.portfolios.iter().map(|p| p.holdings.len()).sum();
    // Summed only when every portfolio reports one. A 401(k) usually reports
    // no cost basis, and adding up the ones that do produces a gain for part
    // of the money that reads as a gain for all of it.
    let unrealized = library
        .portfolios
        .iter()
        .map(|p| p.unrealized)
        .sum::<Option<f64>>();

    Some((value, unrealized, holdings))
}

/// The total paid, when every portfolio says what it was.
fn total_cost(library: Option<&PortfolioLibraryView>) -> Option<f64> {
    library?.portfolios.iter().map(|p| p.total_cost).sum()
}

/// What is wrong, rendered the one way.
///
/// Shared by the dashboard and the alerts sidebar deliberately. They started
/// as two lists built from two hand-written filters over the same state, and
/// that is the arrangement where one of them quietly stops counting something
/// the other still does.
#[component]
pub(crate) fn ProblemList(
    plugins: ReadSignal<Vec<PluginView>>,
    feed_held: ReadSignal<bool>,
    set_feed_held: WriteSignal<bool>,
) -> impl IntoView {
    let sign_in = move |_| {
        spawn_local(async move {
            match call_typed::<bool>("connect_feed", JsValue::UNDEFINED).await {
                Ok(held) => set_feed_held.set(held),
                Err(reason) => web_sys::console::error_1(&reason.into()),
            }
        });
    };

    view! {
        <ul class="alert-list">
            {move || {
                problems(&plugins.get(), feed_held.get())
                    .into_iter()
                    .map(|problem| {
                        view! {
                            <li class="alert alert-warning">
                                <span class="alert-title">{problem.title}</span>
                                <span class="alert-detail">{problem.detail}</span>
                                {problem
                                    .sign_in
                                    .then(|| {
                                        view! {
                                            <button class="alert-action" on:click=sign_in>
                                                "Sign in"
                                            </button>
                                        }
                                    })}
                            </li>
                        }
                    })
                    .collect_view()
            }}
        </ul>
    }
}

#[component]
pub(crate) fn Dashboard(
    portfolios: ReadSignal<Option<PortfolioLibraryView>>,
    /// Why the library could not be read, when it could not.
    ///
    /// Separate from `portfolios` because `None` there means "not loaded",
    /// and the empty state below is a sentence about what the person has
    /// done. Telling someone they have imported nothing when the read simply
    /// failed is worse than saying nothing: it is wrong, it is specific, and
    /// it suggests they repeat work they have already done.
    portfolio_error: ReadSignal<Option<String>>,
    set_open_portfolio: WriteSignal<Option<PortfolioView>>,
    plugins: ReadSignal<Vec<PluginView>>,
    feed_held: ReadSignal<bool>,
    set_feed_held: WriteSignal<bool>,
) -> impl IntoView {
    // Fetched here rather than hoisted into `App`: nothing else reads it, and
    // state in the shell that one view uses is state two things can disagree
    // about later.
    let (history, set_history) = signal(Vec::<HistoryEntryView>::new());
    // Three states, not two. "Nothing has been run yet" is a claim about the
    // evidence store, and it was being made while the request was still in
    // flight and again when the request had failed — so a store that could
    // not be read told someone they had run nothing, which is the one message
    // most likely to make them re-run work they already have.
    //
    // The watchlist keeps the same distinction for the same reason. This is
    // the pattern, one file over.
    let (asked, set_asked) = signal(false);
    let (error, set_error) = signal(None::<String>);
    spawn_local(async move {
        match call_typed::<HistoryView>("list_history", JsValue::UNDEFINED).await {
            Ok(found) => set_history.set(found.entries),
            Err(reason) => set_error.set(Some(reason)),
        }
        set_asked.set(true);
    });

    view! {
        <div class="dashboard">
            <header class="dashboard-head">
                <svg class="dashboard-mark" viewBox="0 0 100 100" aria-hidden="true">
                    <path d="M28 82 L50 16 L72 82" />
                    <path class="dashboard-mark-sweep" d="M30 54 Q50 76 70 66" />
                </svg>
                <div>
                    <h1 class="dashboard-title">"ARVO"</h1>
                    <p class="dashboard-subtitle">"Financial Intelligence Platform"</p>
                </div>
            </header>

            // Only when there is something. A permanent "all clear" panel is
            // one people stop reading, and then miss the day it fills up.
            {move || {
                let any = !problems(&plugins.get(), feed_held.get()).is_empty();
                any
                    .then(|| {
                        view! {
                            <section class="dashboard-section">
                                <h2 class="dashboard-heading">"Needs attention"</h2>
                                <ProblemList
                                    plugins=plugins
                                    feed_held=feed_held
                                    set_feed_held=set_feed_held
                                />
                            </section>
                        }
                    })
            }}

            <section class="dashboard-section">
                <h2 class="dashboard-heading">"Market"</h2>
                <ul class="dashboard-list">
                    <li>
                        <button
                            class="dashboard-row"
                            on:click=move |_| open_study_panel(WATCHLIST_PANEL_ID, "Watchlist")
                        >
                            <span class="dashboard-row-id">"Watchlist"</span>
                            <span class="dashboard-row-meta">
                                "Live prices for what you hold and what you have data for"
                            </span>
                        </button>
                    </li>
                </ul>
            </section>

            <section class="dashboard-section">
                <h2 class="dashboard-heading">"Portfolio"</h2>
                {move || {
                    if let Some(reason) = portfolio_error.get() {
                        return view! {
                            <p class="research-flag">
                                {format!("Could not read your portfolios: {reason}")}
                            </p>
                        }
                            .into_any();
                    }
                    let library = portfolios.get();
                    let Some((value, unrealized, holdings)) = totals(library.as_ref()) else {
                        return view! {
                            <p class="sidebar-empty">
                                "No holdings imported yet — the Portfolio sidebar says where a CSV goes."
                            </p>
                        }
                            .into_any();
                    };

                    // A percentage only where the cost is known: a percentage
                    // of an unknown base is an invented number.
                    let note = unrealized
                        .zip(total_cost(library.as_ref()))
                        .filter(|(_, cost)| *cost != 0.0)
                        .map_or_else(
                            || "cost basis not reported".to_owned(),
                            |(gain, cost)| percent(gain / cost),
                        );

                    view! {
                        <div>
                            <div class="metric-cards">
                                <MetricCard label="Total value" value=money(value) />
                                <MetricCard
                                    label="Unrealised"
                                    value=unrealized.map_or_else(|| "—".to_owned(), money)
                                    tone=unrealized.unwrap_or_default()
                                    note=note
                                />
                                <MetricCard label="Holdings" value=holdings.to_string() />
                            </div>
                            <ul class="dashboard-list">
                                {library
                                    .map(|library| library.portfolios)
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|portfolio| {
                                        let name = portfolio.name.clone();
                                        let meta = format!(
                                            "{} · {} holdings · as of {}",
                                            money(portfolio.total_value),
                                            portfolio.holdings.len(),
                                            portfolio.as_of,
                                        );
                                        // The same two lines the sidebar uses,
                                        // reused rather than made a second way
                                        // in — so a tab already open is
                                        // focused instead of duplicated.
                                        let open = move |_| {
                                            set_open_portfolio.set(Some(portfolio.clone()));
                                            open_study_panel(PORTFOLIO_PANEL_ID, "Portfolio");
                                        };
                                        view! {
                                            <li>
                                                <button class="dashboard-row" on:click=open>
                                                    <span class="dashboard-row-id">{name}</span>
                                                    <span class="dashboard-row-meta">{meta}</span>
                                                </button>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                        </div>
                    }
                        .into_any()
                }}
            </section>

            <section class="dashboard-section">
                <h2 class="dashboard-heading">"Recent findings"</h2>
                {move || {
                    if let Some(reason) = error.get() {
                        // Named, because the alternative reads as an answer.
                        return view! {
                            <p class="research-flag">
                                {format!("Could not read the findings store: {reason}")}
                            </p>
                        }
                            .into_any();
                    }
                    let entries = history.get();
                    if entries.is_empty() {
                        let message = if asked.get() {
                            "Nothing has been run yet"
                        } else {
                            "Looking for findings\u{2026}"
                        };
                        return view! { <p class="sidebar-empty">{message}</p> }.into_any();
                    }
                    view! {
                        <ul class="dashboard-list">
                            {entries
                                .into_iter()
                                // Five: enough to see what you were doing,
                                // short enough that the home screen still
                                // fits without scrolling.
                                .take(5)
                                .map(|entry| {
                                    let stale = entry.stale.unwrap_or_default();
                                    view! {
                                        <li class="dashboard-finding">
                                            <span class=verdict_dot(&entry.verdict) />
                                            <span class="dashboard-row-id">{entry.subject}</span>
                                            <span class="dashboard-row-meta">
                                                {format!("{} · {}", entry.kind, entry.recorded_at)}
                                            </span>
                                            // Said, not hidden: a verdict drawn
                                            // from bars that have since changed
                                            // is the one most likely to be
                                            // quoted and least likely to hold.
                                            {stale
                                                .then(|| {
                                                    view! {
                                                        <span class="dashboard-stale">
                                                            "data changed"
                                                        </span>
                                                    }
                                                })}
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                }}
            </section>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(id: &str, reachable: bool) -> PluginView {
        PluginView {
            id: id.to_owned(),
            address: "http://127.0.0.1:50051".to_owned(),
            status: if reachable {
                PluginStatusView::Reachable {
                    name: id.to_owned(),
                    version: "1".to_owned(),
                    capabilities: vec![],
                }
            } else {
                PluginStatusView::Unreachable {
                    reason: "transport error".to_owned(),
                }
            },
        }
    }

    #[test]
    fn a_healthy_system_has_nothing_to_say() {
        assert!(problems(&[plugin("stub", true)], true).is_empty());
    }

    /// The order matters: a dead session stops market data outright, where an
    /// unreachable plugin stops one capability.
    #[test]
    fn a_dead_session_outranks_an_unreachable_plugin() {
        let found = problems(&[plugin("stub", false)], false);
        assert_eq!(found.len(), 2);
        assert!(found[0].title.contains("Robinhood"));
        assert!(found[1].title.contains("stub"));
        assert!(
            found[1].detail.contains("transport error"),
            "the reason the plugin gave has to survive"
        );
    }

    fn portfolio(name: &str, value: f64, cost: Option<f64>, holdings: usize) -> PortfolioView {
        PortfolioView {
            name: name.to_owned(),
            as_of: "2026-01-01".to_owned(),
            total_value: value,
            total_cost: cost,
            unrealized: cost.map(|cost| value - cost),
            unrealized_pct: None,
            without_cost_basis: 0,
            cash: 0.0,
            holdings: (0..holdings)
                .map(|i| HoldingView {
                    instrument: format!("H{i}"),
                    quantity: None,
                    price: None,
                    market_value: 0.0,
                    cost_basis: None,
                    unrealized: None,
                    unrealized_pct: None,
                    weight: 0.0,
                    priced_by: "statement".to_owned(),
                })
                .collect(),
            unpriced: vec![],
            value_history: vec![],
            change: None,
            import: ImportView {
                columns: vec![],
                ignored: vec![],
                rows_imported: holdings,
                rows_skipped: vec![],
                cost_basis_derived: false,
            },
        }
    }

    fn library(portfolios: Vec<PortfolioView>) -> PortfolioLibraryView {
        PortfolioLibraryView {
            directory: "/tmp".to_owned(),
            portfolios,
        }
    }

    #[test]
    fn nothing_imported_is_not_a_total_of_zero() {
        assert!(totals(None).is_none());
        assert!(
            totals(Some(&library(vec![]))).is_none(),
            "an empty library is not a portfolio worth nothing"
        );
    }

    #[test]
    fn totals_add_up_across_portfolios() {
        let both = library(vec![
            portfolio("brokerage", 1_000.0, Some(800.0), 3),
            portfolio("ira", 500.0, Some(400.0), 2),
        ]);
        let (value, unrealized, holdings) = totals(Some(&both)).expect("two portfolios");
        assert!((value - 1_500.0).abs() < f64::EPSILON);
        assert_eq!(holdings, 5);
        assert!((unrealized.expect("both report a cost") - 300.0).abs() < f64::EPSILON);
    }

    /// The trap: adding the gains that *are* reported produces a number for
    /// part of the money that reads as a number for all of it. A 401(k)
    /// reporting no cost basis is the ordinary case, not an edge one.
    #[test]
    fn one_portfolio_without_a_cost_basis_withholds_the_whole_gain() {
        let mixed = library(vec![
            portfolio("brokerage", 1_000.0, Some(800.0), 3),
            portfolio("401k", 500.0, None, 2),
        ]);
        let (value, unrealized, holdings) = totals(Some(&mixed)).expect("two portfolios");
        assert!((value - 1_500.0).abs() < f64::EPSILON, "value still adds up");
        assert_eq!(holdings, 5);
        assert_eq!(
            unrealized, None,
            "a gain for part of the money must not look like a gain for all of it"
        );
        assert_eq!(total_cost(Some(&mixed)), None, "and neither must the cost");
    }
}
