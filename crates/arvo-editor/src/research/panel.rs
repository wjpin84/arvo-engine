//! One configuration across many instruments.
//!
//! The run that can actually conclude something: a single instrument yields a
//! dozen round trips against a thirty-trade bar, and no amount of history
//! fixes that. What the spread across instruments says is the point — an edge
//! that appears on one name and nowhere else is a property of that name.

use leptos::prelude::*;

use crate::chart::MetricCard;
use crate::format::{percent, short_hash, verdict_class};
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
