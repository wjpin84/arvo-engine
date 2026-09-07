//! One instrument, one grid, one split — and what to do about the answer.
//!
//! [`Recommendations`] sits directly under the verdict and above the charts,
//! because a blocking item means the charts below it should not be read yet,
//! and a reader who has already looked at a rising equity curve has formed the
//! view the blocking item exists to prevent.

use leptos::prelude::*;

use crate::chart::{
    DataQuality, EquityChart, MetricCard, MonthlyReturns, ParameterSurface, PriceChart,
    UnderwaterChart,
};
use crate::format::{percent, ratio, short_hash, verdict_class};
use crate::trades::TradesTable;
use crate::views::*;

/// Month-by-month returns as a grid of years against months.
///
/// What to do about a finding.
///
/// Directly under the verdict, above the charts, because a blocking item
/// means the charts below it should not be read yet — and a reader who has
/// already looked at a rising equity curve has formed the view the blocking
/// item exists to prevent.
///
/// Every item carries the figures it came from. That is not decoration: these
/// are mechanical consequences of stated thresholds, not judgements, and a
/// reader has to be able to disagree with one.
#[component]
pub(crate) fn Recommendations(items: Vec<RecommendationView>) -> impl IntoView {
    (!items.is_empty()).then(|| {
        view! {
            <ul class="research-advice">
                {items
                    .into_iter()
                    .map(|item| {
                        let class = format!("research-advice-item {}", item.severity);
                        view! {
                            <li class=class>
                                <span class="research-advice-severity">{item.severity.clone()}</span>
                                <strong>{item.finding.clone()}</strong>
                                <p>{item.action.clone()}</p>
                                <p class="research-advice-evidence">{item.evidence.clone()}</p>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        }
    })
}

/// What the round trips looked like, and what they cost.
///
/// A return says a rule made money. This says whether it did so the way it
/// would have to keep doing so: a high win rate with negative expectancy is
/// the most common shape of a strategy that looks good and loses, and no
/// summary statistic drawn from the equity curve can show it.
///
/// Fees are shown as money *and* as a fraction of capital, because that is
/// the comparison that matters — 2% of fees against a 3% return is the
/// finding, and neither number says it alone. They are fees and not the total
/// cost of trading: slippage is charged inside the fill prices, so it is
/// already subtracted from the return and never appears as a line item.
#[component]
pub(crate) fn TradeDetail(trades: TradesView) -> impl IntoView {
    // "—" rather than a zero throughout: a statistic that has no value
    // because nothing closed is not the same as one that measured zero, and
    // the whole point of these being `Option` upstream is to keep them apart.
    let pct = |value: Option<f64>| value.map_or_else(|| "—".to_owned(), |v| format!("{:.0}%", v * 100.0));
    let ratio = |value: Option<f64>| value.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"));
    let money = |value: Option<f64>| value.map_or_else(|| "—".to_owned(), |v| format!("{v:+.0}"));
    let days = trades
        .average_holding_days
        .map_or_else(|| "—".to_owned(), |d| format!("{d:.1} days"));
    let open_note = (trades.still_open > 0)
        .then(|| format!(" ({} still open)", trades.still_open));
    let expectancy_class = match trades.expectancy {
        Some(value) if value > 0.0 => "research-good",
        Some(_) => "research-bad",
        None => "",
    };

    view! {
        <dl class="research-provenance">
            <dt>"Closed round trips"</dt>
            <dd>{format!("{}{}", trades.closed, open_note.unwrap_or_default())}</dd>
            <dt>"Win rate"</dt>
            <dd>{pct(trades.win_rate)}</dd>
            <dt>"Expectancy per trade"</dt>
            <dd class=expectancy_class>{money(trades.expectancy)}</dd>
            <dt>"Profit factor"</dt>
            <dd>{ratio(trades.profit_factor)}</dd>
            <dt>"Average win / loss"</dt>
            <dd>{format!("{} / {}", money(trades.average_win), money(trades.average_loss.map(|l| -l)))}</dd>
            <dt>"Average hold"</dt>
            <dd>{days}</dd>
            <dt>"Exits"</dt>
            <dd>{format!("{} on signal, {} on stop", trades.signal_exits, trades.stop_exits)}</dd>
            <dt>"Fees and commission"</dt>
            <dd title="Slippage is charged in the fill prices and is already in the return">
                {format!("{:.0} ({:.2}% of capital)", trades.fees_paid, trades.fees_fraction * 100.0)}
            </dd>
        </dl>
    }
}

/// One study, rendered with its caveats attached rather than beside it.
#[component]
pub(crate) fn StudyReport(study: StudyView) -> impl IntoView {
    let verdict_class = verdict_class(&study.verdict);

    let params = study
        .selected_params
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(", ");

    // A book must say so on its own report. Read as an ordinary study of its
    // head instrument, every number here would be attributed to one name when
    // several produced it — and the charts below genuinely do show only the
    // head, which is a thing to state rather than let a reader assume away.
    let book = (study.instruments.len() > 1).then(|| study.instruments.clone());

    let deflation = study.expected_best_under_null.map_or_else(
        || "not applicable: every configuration scored alike".to_owned(),
        |bar| {
            format!(
                "best in-sample Sharpe {:.2} against {bar:.2} expected from {} no-skill trials",
                study.best_sharpe, study.trials
            )
        },
    );
    let deflation_class = if study.survived_deflation {
        ""
    } else {
        "research-flag"
    };

    view! {
        <div class="research-report">
            <div class=verdict_class>{study.verdict.clone()}</div>
            <p class="research-subject">
                {book
                    .as_ref()
                    .map_or_else(|| study.instrument.clone(), |all| all.join(" + "))}
            </p>
            {book
                .as_ref()
                .map(|all| {
                    view! {
                        <p class="research-hint">
                            {format!(
                                "One account across {} instruments. A position any of them                                  takes is capital the others cannot have, so these numbers                                  are not what running them separately would give. The price                                  chart and trade markers below show {} only.",
                                all.len(),
                                study.instrument.clone(),
                            )}
                        </p>
                    }
                })}

            <ul class="research-reasons">
                {study.reasons.iter().map(|r| view! { <li>{r.clone()}</li> }).collect_view()}
            </ul>

            <Recommendations items=study.recommendations.clone() />
            <DataQuality findings=study.data_findings.clone() />

            <div class="metric-cards">
                <MetricCard
                    label="Excess return"
                    value=percent(study.excess_return)
                    tone=study.excess_return
                    note="vs buy and hold".to_owned()
                />
                <MetricCard
                    label="Strategy"
                    value=percent(study.strategy.total_return)
                    tone=study.strategy.total_return
                />
                <MetricCard
                    label="Buy and hold"
                    value=percent(study.benchmark.total_return)
                    tone=study.benchmark.total_return
                />
                <MetricCard label="Sharpe" value=ratio(study.strategy.sharpe) />
                <MetricCard
                    label="Max drawdown"
                    value=percent(study.strategy.max_drawdown)
                />
                <MetricCard
                    label="Trades"
                    value=study.strategy.trades.to_string()
                    note=format!("{} configurations tried", study.trials)
                />
            </div>

            {(!study.strategy_curve.is_empty())
                .then({
                    let strategy = study.strategy_curve.clone();
                    let benchmark = study.benchmark_curve.clone();
                    move || view! { <EquityChart strategy=strategy benchmark=benchmark /> }
                })}

            <h4>"Out of sample"</h4>
            <table class="research-metrics">
                <thead>
                    <tr>
                        <th></th>
                        <th>"Strategy"</th>
                        <th>"Buy and hold"</th>
                    </tr>
                </thead>
                <tbody>
                    <tr>
                        <td>"Return"</td>
                        <td>{percent(study.strategy.total_return)}</td>
                        <td>{percent(study.benchmark.total_return)}</td>
                    </tr>
                    <tr>
                        <td>"CAGR"</td>
                        <td>{percent(study.strategy.cagr)}</td>
                        <td>{percent(study.benchmark.cagr)}</td>
                    </tr>
                    <tr>
                        <td>"Max drawdown"</td>
                        <td>{percent(study.strategy.max_drawdown)}</td>
                        <td>{percent(study.benchmark.max_drawdown)}</td>
                    </tr>
                    <tr>
                        <td>"Volatility"</td>
                        <td>{percent(study.strategy.volatility)}</td>
                        <td>{percent(study.benchmark.volatility)}</td>
                    </tr>
                    <tr>
                        <td>"Sharpe"</td>
                        <td>{ratio(study.strategy.sharpe)}</td>
                        <td>{ratio(study.benchmark.sharpe)}</td>
                    </tr>
                    <tr>
                        <td>"Sortino"</td>
                        <td>{ratio(study.strategy.sortino)}</td>
                        <td>{ratio(study.benchmark.sortino)}</td>
                    </tr>
                    <tr>
                        <td>"Calmar"</td>
                        <td>{ratio(study.strategy.calmar)}</td>
                        <td>{ratio(study.benchmark.calmar)}</td>
                    </tr>
                    <tr>
                        <td>"Trades"</td>
                        <td>{study.strategy.trades}</td>
                        <td>{study.benchmark.trades}</td>
                    </tr>
                    <tr class="research-excess">
                        <td>"Excess return"</td>
                        <td colspan="2">{percent(study.excess_return)}</td>
                    </tr>
                </tbody>
            </table>

            {(!study.monthly.is_empty())
                .then({
                    let months = study.monthly.clone();
                    move || {
                        view! {
                            <h4>"Monthly returns"</h4>
                            <MonthlyReturns months=months />
                        }
                    }
                })}

            // Directly under the equity curve, because the curve says how
            // much and this says how. A reader who has seen a rising line and
            // not seen where the trades landed has only half the finding.
            // Above the price, because it answers a prior question: whether
            // there was anything to find before asking what the winner did.
            {study
                .surface
                .clone()
                .map(|surface| {
                    view! {
                        <h4>"The whole search"</h4>
                        <ParameterSurface surface=surface />
                    }
                })}

            <h4>"Where it traded"</h4>
            <PriceChart candles=study.price.clone() markers=study.markers.clone() />

            <h4>"Underwater"</h4>
            <UnderwaterChart points=study.underwater.clone() />

            <h4>"The trades behind it"</h4>
            <TradeDetail trades=study.trades_detail.clone() />
            <TradesTable rows=study.trades.clone() name=study.instrument.clone() />

            <h4>"How this was arrived at"</h4>
            <dl class="research-provenance">
                <dt>"Chosen on"</dt>
                <dd>{study.in_sample.clone()}</dd>
                <dt>"Judged on"</dt>
                <dd>{study.out_of_sample.clone()}</dd>
                <dt>"Configurations tried"</dt>
                <dd>{study.trials}</dd>
                <dt>"Multiple-testing check"</dt>
                <dd class=deflation_class>{deflation}</dd>
                <dt>"Winning parameters"</dt>
                <dd>{params}</dd>
                <dt>"Dataset"</dt>
                <dd class="research-hash">{short_hash(&study.dataset_version)}</dd>
                <dt>"Strategy"</dt>
                <dd>{study.strategy_name.clone()}</dd>
                <dt>"Starting cash"</dt>
                <dd>{format!("{:.0}", study.starting_cash)}</dd>
                <dt>"Commission"</dt>
                <dd>{format!("{} bps", study.commission_bps)}</dd>
                <dt>"Slippage"</dt>
                <dd>{format!("{} bps a side", study.slippage_bps)}</dd>
                <dt>"Engine"</dt>
                <dd>{study.engine.clone()}</dd>
            </dl>
        </div>
    }
}
