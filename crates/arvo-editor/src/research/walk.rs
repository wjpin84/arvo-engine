//! A rolling re-selection, fold by fold.
//!
//! The fold table is the centre of this report rather than a detail. A single
//! study shows one winner and asks you to believe the search found it; this
//! shows every winner the search found, in order, marks the folds whose winner
//! was noise, and lets a reader see for themselves whether the selection
//! settled or wandered.

use leptos::prelude::*;

use crate::chart::{DataQuality, EquityChart, MetricCard, PriceChart, UnderwaterChart};
use crate::format::{percent, ratio, short_hash, verdict_class};
use crate::research::study::TradeDetail;
use crate::trades::TradesTable;
use crate::research::study::Recommendations;
use crate::views::*;

/// A rolling re-selection: what the procedure did, fold by fold.
///
/// The fold table is the centre of this report rather than a detail. A single
/// study shows one winner and asks you to believe the search found it; this
/// shows every winner the search found, in order, and lets the reader see for
/// themselves whether the selection settled or wandered.
#[component]
pub(crate) fn WalkForwardReport(walk: WalkForwardView) -> impl IntoView {
    let verdict_class = verdict_class(&walk.verdict);
    let folds = walk.folds.len();
    let cadence = format!(
        "{} on {} days, judged {} at a time",
        if walk.anchored { "expanding" } else { "sliding" },
        walk.in_sample_days,
        walk.step_days,
    );

    view! {
        <div class="research-report">
            <div class=verdict_class>{walk.verdict.clone()}</div>
            <p class="research-subject">
                {format!("{} — walk-forward", walk.instrument)}
            </p>

            <ul class="research-reasons">
                {walk.reasons.iter().map(|r| view! { <li>{r.clone()}</li> }).collect_view()}
            </ul>

            <Recommendations items=walk.recommendations.clone() />

            <DataQuality findings=walk.data_findings.clone() />

            <div class="metric-cards">
                <MetricCard
                    label="Excess return"
                    value=percent(walk.excess_return)
                    tone=walk.excess_return
                    note="stitched, vs buy and hold".to_owned()
                />
                <MetricCard
                    label="Strategy"
                    value=percent(walk.strategy.total_return)
                    tone=walk.strategy.total_return
                />
                <MetricCard
                    label="Buy and hold"
                    value=percent(walk.benchmark.total_return)
                    tone=walk.benchmark.total_return
                />
                <MetricCard label="Sharpe" value=ratio(walk.strategy.sharpe) />
                <MetricCard
                    label="Max drawdown"
                    value=percent(walk.strategy.max_drawdown)
                />
                // The number a single split cannot produce: how often the
                // search found something better than luck. Zero here is the
                // finding, whatever the return says.
                <MetricCard
                    label="Folds beating chance"
                    value=format!("{} / {folds}", walk.folds_surviving_deflation)
                    note="per-fold deflation".to_owned()
                />
            </div>

            <h4>"Out-of-sample record, stitched"</h4>
            <EquityChart
                strategy=walk.strategy_curve.clone()
                benchmark=walk.benchmark_curve.clone()
            />

            // Across the whole judged span, with every fold's trades on it.
            // Seeing the re-selections land on one price series is the only
            // way to notice that a "new" winner traded identically to the old
            // one — which a table of parameters cannot show.
            <h4>"Where it traded"</h4>
            <PriceChart candles=walk.price.clone() markers=walk.markers.clone() />

            <h4>"Underwater"</h4>
            <UnderwaterChart points=walk.underwater.clone() />

            <h4>"How the selection moved"</h4>
            {(walk.stability.is_empty())
                .then(|| {
                    view! {
                        <p class="research-hint">
                            "The grid varies nothing, so there was no selection to watch."
                        </p>
                    }
                })}
            <dl class="research-provenance">
                {walk
                    .stability
                    .iter()
                    .map(|axis| {
                        // Under half means the search landed somewhere
                        // different more often than not — which is what
                        // fitting noise looks like from the outside.
                        let unstable = axis.modal_share < 0.5;
                        view! {
                            <dt>{axis.axis.clone()}</dt>
                            <dd class=if unstable { "research-bad" } else { "" }>
                                {format!(
                                    "{} in {:.0}% of folds, {} values tried",
                                    axis.modal,
                                    axis.modal_share * 100.0,
                                    axis.distinct,
                                )}
                            </dd>
                        }
                    })
                    .collect_view()}
            </dl>

            <h4>"Every fold"</h4>
            <div class="research-scroll">
                <table class="research-metrics">
                    <thead>
                        <tr>
                            <th>"Chose on"</th>
                            <th>"Judged on"</th>
                            <th>"Winner"</th>
                            <th>"Strategy"</th>
                            <th>"Buy and hold"</th>
                            <th>"Trades"</th>
                        </tr>
                    </thead>
                    <tbody>
                        {walk
                            .folds
                            .iter()
                            .map(|fold| {
                                let params = fold
                                    .params
                                    .iter()
                                    .map(|(name, value)| format!("{name}={value}"))
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                // A fold whose winner was noise is marked, not
                                // dropped: the run still happened and its
                                // return still counts toward the stitch.
                                let noise = (!fold.survived_deflation)
                                    .then_some("research-flag");
                                view! {
                                    <tr>
                                        <td class="research-left">{fold.chose_on.clone()}</td>
                                        <td class="research-left">{fold.judged_on.clone()}</td>
                                        <td class=noise>{params}</td>
                                        <td>{percent(fold.strategy_return)}</td>
                                        <td>{percent(fold.benchmark_return)}</td>
                                        <td>{fold.trades}</td>
                                    </tr>
                                }
                            })
                            .collect_view()}
                    </tbody>
                </table>
            </div>

            <h4>"The trades behind it"</h4>
            <TradeDetail trades=walk.trades_detail.clone() />
            <TradesTable
                rows=walk.trades.clone()
                name=format!("{}-walk-forward", walk.instrument)
            />

            <h4>"How this was arrived at"</h4>
            <dl class="research-provenance">
                <dt>"Cadence"</dt>
                <dd>{cadence}</dd>
                <dt>"Folds"</dt>
                <dd>
                    {format!(
                        "{folds}{}",
                        if walk.folds_without_trades > 0 {
                            format!(", {} of which never opened a position", walk.folds_without_trades)
                        } else {
                            String::new()
                        },
                    )}
                </dd>
                <dt>"Dataset"</dt>
                <dd class="research-hash">{short_hash(&walk.dataset_version)}</dd>
                <dt>"Strategy"</dt>
                <dd>{walk.strategy_name.clone()}</dd>
                <dt>"Starting cash"</dt>
                <dd>{format!("{:.0}", walk.starting_cash)}</dd>
                <dt>"Commission"</dt>
                <dd>{format!("{} bps", walk.commission_bps)}</dd>
                <dt>"Slippage"</dt>
                <dd>{format!("{} bps a side", walk.slippage_bps)}</dd>
                <dt>"Engine"</dt>
                <dd>{walk.engine.clone()}</dd>
            </dl>
        </div>
    }
}
