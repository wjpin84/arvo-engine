//! One configuration across many instruments.
//!
//! The run that can actually conclude something: a single instrument yields a
//! dozen round trips against a thirty-trade bar, and no amount of history
//! fixes that. What the spread across instruments says is the point — an edge
//! that appears on one name and nowhere else is a property of that name.

use leptos::prelude::*;

use crate::chart::MetricCard;
use crate::format::{percent, ratio, short_hash, verdict_class};
use crate::research::study::Recommendations;
use crate::views::*;

/// The panel tab's content, read back out of the signal so a re-run refreshes
/// the tab that is already open.
#[component]
pub(crate) fn PanelTab(panel: ReadSignal<Option<PanelView>>) -> impl IntoView {
    view! {
        <div class="study-panel">
            {move || panel.get().map(|panel| view! { <PanelReport panel=panel /> })}
        </div>
    }
}

/// A panel study: one configuration, many instruments, and what the spread
/// across them says that any single one could not.
#[component]
pub(crate) fn PanelReport(panel: PanelView) -> impl IntoView {
    let verdict_class = verdict_class(&panel.verdict);
    let params = panel
        .selected_params
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let deflation = panel.expected_best_under_null.map_or_else(
        || "not applicable: every configuration scored alike".to_owned(),
        |bar| {
            format!(
                "best pooled in-sample Sharpe {:.2} against {bar:.2} expected from {} no-skill \
                 trials",
                panel.best_sharpe, panel.trials
            )
        },
    );
    let deflation_class = if panel.survived_deflation {
        ""
    } else {
        "research-flag"
    };
    let consistency = format!(
        "{} of {} instruments beat their own benchmark",
        panel.beat_benchmark, panel.instruments
    );
    let consistent = panel.instruments > 0 && panel.beat_benchmark * 2 > panel.instruments;

    view! {
        <div class="research-report">
            <div class=verdict_class>{panel.verdict.clone()}</div>
            <p class="research-subject">
                {format!("Panel of {} instruments", panel.instruments)}
            </p>

            <ul class="research-reasons">
                {panel.reasons.iter().map(|r| view! { <li>{r.clone()}</li> }).collect_view()}
            </ul>

            <Recommendations items=panel.recommendations.clone() />

            <div class="metric-cards">
                <MetricCard
                    label="Mean excess return"
                    value=percent(panel.mean_excess_return)
                    tone=panel.mean_excess_return
                    note="vs buy and hold".to_owned()
                />
                <MetricCard
                    label="Consistency"
                    value=format!("{}/{}", panel.beat_benchmark, panel.instruments)
                    note="instruments beat their benchmark".to_owned()
                />
                <MetricCard
                    label="Trades (pooled)"
                    value=panel.total_trades.to_string()
                    note=format!("{} configurations tried", panel.trials)
                />
                <MetricCard
                    label="Mean drawdown"
                    value=percent(panel.mean_max_drawdown)
                    note=format!("worst {}", percent(panel.worst_max_drawdown))
                />
            </div>

            <h4>"Pooled out of sample"</h4>
            <table class="research-metrics">
                <tbody>
                    <tr>
                        <td>"Mean excess return"</td>
                        <td>{percent(panel.mean_excess_return)}</td>
                    </tr>
                    <tr>
                        <td>"Consistency"</td>
                        <td class=if consistent { "" } else { "research-flag" }>{consistency}</td>
                    </tr>
                    <tr>
                        <td>"Trades (pooled)"</td>
                        <td>{panel.total_trades}</td>
                    </tr>
                    <tr>
                        <td>"Mean drawdown"</td>
                        <td>{percent(panel.mean_max_drawdown)}</td>
                    </tr>
                    <tr>
                        <td>"Worst drawdown"</td>
                        <td>{percent(panel.worst_max_drawdown)}</td>
                    </tr>
                </tbody>
            </table>

            <h4>"Per instrument"</h4>
            <table class="research-metrics">
                <thead>
                    <tr>
                        <th>"Instrument"</th>
                        <th>"Strategy"</th>
                        <th>"Buy and hold"</th>
                        <th>"Excess"</th>
                        <th>"Drawdown"</th>
                        <th>"Trades"</th>
                    </tr>
                </thead>
                <tbody>
                    {panel
                        .per_instrument
                        .iter()
                        .map(|outcome| {
                            let beat = outcome.excess_return > 0.0;
                            view! {
                                <tr>
                                    <td>{outcome.instrument.clone()}</td>
                                    <td>{percent(outcome.strategy_return)}</td>
                                    <td>{percent(outcome.benchmark_return)}</td>
                                    <td class=if beat { "" } else { "research-flag" }>
                                        {percent(outcome.excess_return)}
                                    </td>
                                    <td>{percent(outcome.max_drawdown)}</td>
                                    <td>{outcome.trades}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>

            {panel
                .book
                .clone()
                .map(|book| {
                    view! {
                        <h4>"What holding all of them would have done"</h4>
                        <PanelBook book=book />
                    }
                })}

            {panel
                .breadth
                .clone()
                .filter(|breadth| breadth.instruments.len() > 1)
                .map(|breadth| {
                    view! {
                        <h4>"How independent these instruments are"</h4>
                        <PanelBreadth breadth=breadth />
                    }
                })}

            <h4>"How this was arrived at"</h4>
            <dl class="research-provenance">
                <dt>"Chosen on"</dt>
                <dd>{panel.in_sample.clone()}</dd>
                <dt>"Judged on"</dt>
                <dd>{panel.out_of_sample.clone()}</dd>
                <dt>"Configurations tried"</dt>
                <dd>{panel.trials}</dd>
                <dt>"Multiple-testing check"</dt>
                <dd class=deflation_class>{deflation}</dd>
                <dt>"One configuration for all"</dt>
                <dd>{params}</dd>
                <dt>"Dataset"</dt>
                <dd class="research-hash">{short_hash(&panel.dataset_version)}</dd>
                <dt>"Strategy"</dt>
                <dd>{panel.strategy_name.clone()}</dd>
                <dt>"Starting cash"</dt>
                <dd>{format!("{:.0} per instrument", panel.starting_cash)}</dd>
                <dt>"Commission"</dt>
                <dd>{format!("{} bps", panel.commission_bps)}</dd>
                <dt>"Slippage"</dt>
                <dd>{format!("{} bps a side", panel.slippage_bps)}</dd>
                <dt>"Engine"</dt>
                <dd>{panel.engine.clone()}</dd>
            </dl>

            {(!panel.failures.is_empty())
                .then(|| {
                    view! {
                        <div>
                            <h4>"Runs that did not complete"</h4>
                            <ul class="research-reasons">
                                {panel
                                    .failures
                                    .iter()
                                    .map(|f| view! { <li>{f.clone()}</li> })
                                    .collect_view()}
                            </ul>
                        </div>
                    }
                })}
        </div>
    }
}

/// How much of a panel's apparent breadth is real.
///
/// A panel's pooled numbers read as evidence in proportion to how many
/// instruments produced them. If those instruments moved together, the count
/// is not the sample size it looks like — three that agree are one observation
/// wearing a three.
///
/// The matrix is here so the number above it can be argued with. An effective
/// breadth of 1.2 out of three is a strong claim about a result, and a reader
/// should be able to see which pair is responsible for it.
#[component]
pub(crate) fn PanelBreadth(breadth: BreadthView) -> impl IntoView {
    let count = breadth.instruments.len();
    let columns = format!("auto repeat({}, minmax(3em, 1fr))", count.max(1));
    let headline = match (breadth.effective, breadth.overstatement) {
        (Some(effective), Some(overstatement)) => format!(
            "These {count} instruments behave like {effective:.1} independent ones. \
             The pooled average is about {overstatement:.1}x less certain than its \
             instrument count suggests."
        ),
        _ => "Too little overlap between these instruments to say how independent they are."
            .to_owned(),
    };
    let labels = breadth.instruments.clone();

    view! {
        <div class="surface">
            <p class="research-hint">{headline}</p>
            <div class="surface-grid" style=format!("grid-template-columns: {columns}")>
                <span class="surface-corner"></span>
                {labels
                    .iter()
                    .map(|name| {
                        // Tickers, not full ids: a matrix of `MSFT.NASDAQ`
                        // headers is unreadable at this width.
                        let short = name.split('.').next().unwrap_or(name).to_owned();
                        view! { <span class="surface-head" title=name.clone()>{short}</span> }
                    })
                    .collect_view()}
                {breadth
                    .correlations
                    .iter()
                    .enumerate()
                    .map(|(row, values)| {
                        let name = labels.get(row).cloned().unwrap_or_default();
                        let short = name.split('.').next().unwrap_or(&name).to_owned();
                        let cells = values
                            .iter()
                            .map(|value| {
                                match value {
                                    // A pair with too little overlap makes no
                                    // claim, which is not the same as a
                                    // correlation of zero.
                                    None => {
                                        view! { <span class="surface-cell empty">"·"</span> }
                                            .into_any()
                                    }
                                    Some(value) => {
                                        // Shaded by magnitude, not by sign:
                                        // strongly negative correlation is
                                        // just as much a departure from
                                        // independence as strongly positive.
                                        let depth = value.abs().clamp(0.0, 1.0);
                                        let style = format!(
                                            "background: color-mix(in srgb, \
                                             var(--color-verdict-inconclusive) {:.0}%, \
                                             transparent)",
                                            depth * 90.0,
                                        );
                                        view! {
                                            <span class="surface-cell" style=style>
                                                {format!("{value:.2}")}
                                            </span>
                                        }
                                            .into_any()
                                    }
                                }
                            })
                            .collect_view();
                        view! {
                            <span class="surface-head" title=name>{short}</span>
                            {cells}
                        }
                    })
                    .collect_view()}
            </div>
        </div>
    }
}

/// What one account holding every member would have done.
///
/// The table above reports the members one at a time and the summary averages
/// them. Neither is a portfolio. An average of drawdowns is the typical fall
/// of a member, not the fall an account holding all of them would have taken —
/// and those differ by exactly the amount the members failed to move together.
/// That difference is the only reason to run more than one instrument, so it
/// is worth a number rather than an inference.
#[component]
pub(crate) fn PanelBook(book: BookView) -> impl IntoView {
    // Stated rather than assumed: this rescales the members' curves, so it
    // cannot show two of them competing for the same capital. A reader who
    // does not know that will read the drawdown as a promise.
    let caveat = concat!(
        "Equal weight, divided once at the start and never rebalanced. ",
        "Each member ran with the whole account behind it, so this rescales their ",
        "results rather than re-running them together \u{2014} it cannot show two ",
        "positions competing for the same capital.",
    );

    // Recovered rather than passed: the panel already knows its mean member
    // drawdown, and the gap is the whole point of the card.
    let mean_member = book.metrics.max_drawdown + book.diversification;
    let diversified = book.diversification > 0.0;
    let verdict = if diversified {
        format!(
            "Holding all of them fell {} less than the average member did. \
             That gap is the diversification.",
            percent(book.diversification),
        )
    } else {
        concat!(
            "Holding all of them fell as hard as the average member did. ",
            "These instruments went down together, so the panel was one bet.",
        )
        .to_owned()
    };
    let tone = if diversified { "research-hint" } else { "research-flag" };

    view! {
        <div class="surface">
            <p class=tone>{verdict}</p>
            <div class="metric-cards">
                <MetricCard
                    label="Book return"
                    value=percent(book.metrics.total_return)
                    tone=book.metrics.total_return
                    note="equal weight, all members".to_owned()
                />
                <MetricCard
                    label="Book drawdown"
                    value=percent(book.metrics.max_drawdown)
                    note=format!("mean member {}", percent(mean_member))
                />
                <MetricCard label="Book Sharpe" value=ratio(book.metrics.sharpe) />
                <MetricCard
                    label="Book CAGR"
                    value=percent(book.metrics.cagr)
                    tone=book.metrics.cagr
                />
            </div>
            <p class="research-hint">{caveat}</p>
        </div>
    }
}
