//! Engine types as the window sees them.
//!
//! Free functions rather than `From` impls throughout: the view shapes live in
//! `arvo-api` and the engine types in `arvo-research`, so neither is local
//! here and the orphan rule forbids the impl. That is the rule doing its job —
//! the conversion is this crate's business and belongs in this crate.

use super::*;

/// A metrics summary, as the window sees it.
///
/// A free function rather than a `From` impl: [`MetricsView`] lives in
/// `arvo-api` and `Metrics` in `arvo-research`, so neither is local here and
/// the orphan rule forbids the impl.
pub fn metrics_view(metrics: &Metrics) -> MetricsView {

MetricsView {
        total_return: metrics.total_return,
        cagr: metrics.cagr,
        max_drawdown: metrics.max_drawdown,
        volatility: metrics.volatility,
        sharpe: metrics.sharpe,
        sortino: metrics.sortino,
        calmar: metrics.calmar,
        psr: metrics.psr,
        trades: metrics.trades,
    }
}

/// Flattens an equity curve for charting, collapsing any repeated day.
///
/// Charting libraries reject non-ascending or duplicated times, and typically
/// by throwing — which in a webview means a blank panel and no explanation.
/// An equity curve as the chart wants it.
///
/// One point per bar, in epoch seconds. Nothing is collapsed or deduplicated:
/// the curve is already one point per bar by construction, and thinning it
/// here would draw a different line from the one the metrics were computed on.
pub fn curve_points(curve: &[arvo_research::EquityPoint]) -> Vec<CurvePoint> {
    curve
        .iter()
        .map(|point| CurvePoint {
            time: point.at.and_utc().timestamp(),
            value: point.equity,
        })
        .collect()
}

/// What is wrong with the bars a result was produced from.
///
/// Run at report time rather than at fetch time, and attached to the *result*.
/// A verdict is only as good as the series under it, and the place a person
/// will actually read "these bars have a hole in them" is next to the number
/// it undermines — not in a data screen they would have to think to open.
pub fn data_findings(
    bars: &dyn arvo_data::BarProvider,
    instrument: &str,
    interval: arvo_data::BarInterval,
    window: &DateRange,
) -> Vec<DataFindingView> {
    let series = bars
        .bars(instrument, interval, window.from, window.to)
        .unwrap_or_default();

    arvo_data::quality::inspect(
        &series,
        interval,
        arvo_data::Instrument::of(instrument).hours,
    )
        .findings
        .into_iter()
        .map(|finding| DataFindingView {
            severity: match finding.severity {
                arvo_data::quality::Severity::Fault => "fault",
                arvo_data::quality::Severity::Suspect => "suspect",
            }
            .to_owned(),
            kind: finding.kind.to_owned(),
            at: finding.at.map(|at| at.format("%Y-%m-%d %H:%M").to_string()),
            detail: finding.detail,
        })
        .collect()
}

/// What is wrong with the bars under a stored finding, for a reader who opens
/// the finding and not a window (arvo-engine#16).
///
/// The window shows these beside the chart. An agent or a script reads the
/// verdict, the reasons and the advice, and had no word about the series
/// underneath: it is the reader most likely to take a number at face value
/// and the one that could not see a suspected unadjusted split.
///
/// A study and a walk-forward get the list their own views show, over the
/// same window, so the two readers are told the same thing. A panel has many
/// series: every fault is listed with its member named, and the merely
/// unusual is counted per member, because a hundred members' worth of them is
/// a list nobody reads. A reported finding has none: its bars are in
/// somebody else's library.
#[must_use]
pub fn finding_data_findings(bars: &dyn arvo_data::BarProvider, record: &Record) -> Vec<DataFindingView> {
    match record {
        Record::Study(found) => {
            data_findings(bars, &found.selected.instrument, found.selected.interval, &found.out_of_sample)
        }
        Record::WalkForward(found) => found
            .folds
            .first()
            .zip(found.folds.last())
            .and_then(|(first, last)| DateRange::new(first.out_of_sample.from, last.out_of_sample.to).ok())
            .map(|window| data_findings(bars, &found.template.instrument, found.template.interval, &window))
            .unwrap_or_default(),
        Record::Panel(found) => {
            let interval = found.study.as_ref().map(|study| study.template.interval).or_else(|| {
                found.per_instrument.iter().find_map(|member| member.kept.as_ref().map(|kept| kept.experiment.interval))
            });
            let Some(interval) = interval else { return Vec::new() };
            let mut said = Vec::new();
            for member in &found.per_instrument {
                let all = data_findings(bars, &member.instrument, interval, &found.out_of_sample);
                let (faults, suspects): (Vec<_>, Vec<_>) = all.into_iter().partition(|one| one.severity == "fault");
                said.extend(faults.into_iter().map(|fault| DataFindingView {
                    detail: format!("{}: {}", member.instrument, fault.detail),
                    ..fault
                }));
                if !suspects.is_empty() {
                    let mut kinds: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
                    for suspect in &suspects {
                        *kinds.entry(suspect.kind.as_str()).or_default() += 1;
                    }
                    let by_kind: Vec<String> = kinds.iter().map(|(kind, count)| format!("{count} {kind}")).collect();
                    said.push(DataFindingView {
                        severity: "suspect".to_owned(),
                        kind: "summary".to_owned(),
                        at: None,
                        detail: format!("{}: {} worth a look ({})", member.instrument, suspects.len(), by_kind.join(", ")),
                    });
                }
            }
            said
        }
        Record::Reported(_) => Vec::new(),
    }
}

/// The instrument's own bars over a window.
///
/// Read from the library at render time rather than stored in the finding.
/// The bars are the *input* to an experiment and are already identified by a
/// content hash; copying them into every stored result would duplicate
/// megabytes to say something the hash already says. If the file has changed
/// since, the finding is marked stale by the machinery that exists for it.
pub fn candles(
    bars: &dyn arvo_data::BarProvider,
    instrument: &str,
    interval: arvo_data::BarInterval,
    window: &DateRange,
) -> Vec<CandlePoint> {
    bars.bars(instrument, interval, window.from, window.to)
        .unwrap_or_default()
        .into_iter()
        .map(|bar| CandlePoint {
            time: bar.at.and_utc().timestamp(),
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
        })
        .collect()
}

/// One indicator as a chart asks for it: the rule language's own declaration
/// (the `indicators` entry of a rule file, as JSON), resolved with `params`.
/// Computed by [`indicator_curve`] with the code a data rule runs, so the
/// curve is what a rule would have seen; points begin at the first bar the
/// indicator has a value for.
///
/// # Errors
///
/// A declaration that does not parse, or one that does not resolve: a period
/// naming a parameter `params` does not hold, or a period that is not a
/// whole number of bars.
pub fn resolve_indicator(
    declaration: &str,
    params: &std::collections::BTreeMap<String, f64>,
) -> Result<arvo_research::rule::ResolvedIndicator, String> {
    let indicator: arvo_research::rule::Indicator =
        serde_json::from_str(declaration).map_err(|err| format!("the indicator does not parse: {err}"))?;
    indicator.resolve("chart", "indicator", params).map_err(|err| err.to_string())
}

/// The resolved indicator over `bars`, named as a legend names it.
#[must_use]
pub fn indicator_curve(bars: &[arvo_data::Bar], resolved: arvo_research::rule::ResolvedIndicator) -> NamedCurveView {
    let points = arvo_nautilus::indicator_series(resolved, bars)
        .into_iter()
        .zip(bars)
        .filter_map(|(value, bar)| value.map(|value| CurvePoint { time: bar.at.and_utc().timestamp(), value }))
        .collect();
    NamedCurveView { name: resolved.label(), points }
}

/// Every entry and exit in a ledger, as chart markers on the bars that caused
/// them.
///
/// # The interval is not decoration
///
/// A trade's timestamp is the instant it *filled*, and a fill happens at the
/// close of the bar the signal was read from — that is the whole look-ahead
/// convention this platform is built on. A candle, meanwhile, is stamped at
/// the instant it *opens*. So a fill on the bar opening at 09:30 carries the
/// time 09:35, and drawing it there puts every marker one bar to the right of
/// the bar that actually produced it.
///
/// Shifting back by one interval is what lines them up. It is invisible if you
/// do not look for it: the chart would render, the markers would sit on
/// plausible candles, and every entry would appear to have been taken one bar
/// after the rule fired.
/// The round trips that happened in one instrument.
///
/// A book's ledger holds every member's trades, and a chart of one member must
/// show only its own — otherwise entries appear on days that instrument never
/// traded, which is not a small error on a price chart.
///
/// A trade with no instrument comes from a ledger recorded before they were
/// named. Those runs were all single-instrument, so it belongs to the head,
/// and dropping it would empty the chart of every stored finding at once.
pub fn trades_in(
    ledger: &[arvo_research::Trade],
    instrument: &str,
    head: &str,
) -> Vec<arvo_research::Trade> {
    ledger
        .iter()
        .filter(|trade| {
            if trade.instrument.is_empty() {
                instrument == head
            } else {
                trade.instrument == instrument
            }
        })
        .cloned()
        .collect()
}

pub fn markers(
    ledger: &[arvo_research::Trade],
    interval: arvo_data::BarInterval,
) -> Vec<TradeMarkerView> {
    let step = interval.duration();
    let on_bar = |at: chrono::NaiveDateTime| (at - step).and_utc().timestamp();

    let mut out = Vec::with_capacity(ledger.len() * 2);
    for trade in ledger {
        out.push(TradeMarkerView {
            time: on_bar(trade.opened),
            kind: "entry".to_owned(),
            reason: String::new(),
            label: format!("{:.0} @ {:.2}", trade.quantity, trade.entry),
        });
        if let (Some(closed), Some(exit)) = (trade.closed, trade.exit) {
            out.push(TradeMarkerView {
                time: on_bar(closed),
                kind: "exit".to_owned(),
                // A stop-out, a halt and a signal exit look identical in a
                // summary and could not be more different in what they say
                // about the rule: the first says the trade failed, the second
                // says the *account* did, and the third says the rule chose to
                // leave. Matched exhaustively so a fourth reason cannot
                // silently join the third.
                reason: match trade.exit_reason {
                    arvo_research::ExitReason::Stop => "stop",
                    arvo_research::ExitReason::Halted => "halt",
                    arvo_research::ExitReason::Expired => "expired",
                    arvo_research::ExitReason::Signal
                    | arvo_research::ExitReason::StillOpen => "signal",
                }
                .to_owned(),
                label: format!("{exit:.2} ({:+.0})", trade.pnl),
            });
        }
    }
    // The chart requires markers in time order and throws on anything else,
    // and an exception crossing back into wasm takes the calling future with
    // it — so this is not a tidiness sort.
    out.sort_by_key(|marker| marker.time);
    out
}

/// The equity curve expressed as depth below its own running peak.
///
/// Drawn rather than summarised because a single worst-drawdown number cannot
/// distinguish one deep hole from a decade spent underwater, and those are
/// different things to have lived through.
pub fn underwater(curve: &[arvo_research::EquityPoint]) -> Vec<CurvePoint> {
    let mut peak = f64::NEG_INFINITY;
    curve
        .iter()
        .map(|point| {
            peak = peak.max(point.equity);
            CurvePoint {
                time: point.at.and_utc().timestamp(),
                // Negative, so the series hangs below zero the way every
                // underwater plot in the literature does.
                value: if peak > 0.0 {
                    (point.equity - peak) / peak * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect()
}

/// What the round trips looked like, as the window sees it.
///
/// A free function for the same reason as the two above.
pub fn trades_view(stats: &arvo_research::TradeStats, starting_cash: f64) -> TradesView {

    TradesView {
            closed: stats.closed,
            still_open: stats.still_open,
            win_rate: stats.win_rate,
            profit_factor: stats.profit_factor,
            expectancy: stats.expectancy(),
            average_win: stats.average_win,
            average_loss: stats.average_loss,
            average_holding_days: stats.average_holding_secs.map(|secs| secs / 86_400.0),
            fees_paid: stats.total_commission,
            fees_fraction: if starting_cash > 0.0 {
                stats.total_commission / starting_cash
            } else {
                0.0
            },
            signal_exits: stats.signal_exits,
            stop_exits: stats.stop_exits,
        }
}

/// Flattens a stored study for display.
///
/// Separate from the command so a finding read back from memory renders
/// identically to one just produced. Two projections would drift, and a
/// history that showed something subtly different from the live run would be
/// worse than no history.
/// The ledger as table rows.
/// What each instrument in a book contributed, in the order the book names
/// them.
///
/// Empty for anything but a book: a single study's whole ledger is its one
/// instrument, and a table saying so would be a column of the same name.
///
/// A member with no trades is reported rather than omitted. It is the failure
/// mode a shared account introduces — asked for, funded by nothing, absent
/// from every other number — and a row that is simply missing looks like an
/// instrument nobody chose.
pub fn members(experiment: &arvo_research::Experiment, ledger: &[arvo_research::Trade]) -> Vec<MemberView> {
    let all = experiment.instruments();
    if all.len() < 2 {
        return Vec::new();
    }
    // Signed, and summed before any share is taken: a book whose winners and
    // losers cancel has no meaningful denominator, and dividing by a total near
    // zero would hand out shares in the hundreds.
    let total: f64 = ledger.iter().map(|trade| trade.pnl).sum();

    all.into_iter()
        .map(|instrument| {
            let mine: Vec<&arvo_research::Trade> = ledger
                .iter()
                .filter(|trade| trade.instrument == instrument)
                .collect();
            let pnl: f64 = mine.iter().map(|trade| trade.pnl).sum();
            MemberView {
                instrument,
                trades: u32::try_from(mine.len()).unwrap_or(u32::MAX),
                pnl,
                share: (total.abs() > f64::EPSILON).then(|| pnl / total),
                silent: mine.is_empty(),
            }
        })
        .collect()
}

pub fn trade_rows(ledger: &[arvo_research::Trade]) -> Vec<TradeRowView> {
    ledger
        .iter()
        .map(|trade| TradeRowView {
            instrument: trade.instrument.clone(),
            opened: trade.opened.format("%Y-%m-%d %H:%M").to_string(),
            closed: trade
                .closed
                .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default(),
            direction: match trade.direction {
                arvo_research::Direction::Long => "long",
                arvo_research::Direction::Short => "short",
            }
            .to_owned(),
            quantity: trade.quantity,
            entry: trade.entry,
            exit: trade.exit,
            pnl: trade.pnl,
            commission: trade.commission,
            held_days: trade
                .holding_period()
                .map(|held| held.num_seconds() as f64 / 86_400.0),
            exit_reason: match trade.exit_reason {
                arvo_research::ExitReason::Signal => "signal",
                arvo_research::ExitReason::Stop => "stop",
                arvo_research::ExitReason::Halted => "halted",
                arvo_research::ExitReason::Expired => "expired",
                arvo_research::ExitReason::StillOpen => "open",
            }
            .to_owned(),
            rule: trade.journal.as_ref().map(|journal| journal.rule.clone()),
            signal: trade.journal.as_ref().map(|journal| journal.signal),
            regime: trade.journal.as_ref().and_then(|journal| journal.regime.clone()),
            asked: trade.journal.as_ref().map(|journal| journal.asked),
        })
        .collect()
}

/// Turns the search surface into something drawable.
///
/// `None` when the grid varies fewer than two parameters — a surface needs two
/// dimensions, and a single axis is a list, which the fold table and the
/// winning-parameters line already say.
///
/// # Choosing which two axes
///
/// The two with the most distinct values, because those are the ones the
/// search actually explored. Any others are *collapsed by taking the best*
/// score over them, and named in [`SurfaceView::collapsed`] so the chart is
/// never mistaken for the whole search. Taking the best rather than the mean
/// is deliberate: this chart answers "was there a good region", and averaging
/// a good configuration together with a bad one on a hidden axis would hide
/// exactly the region being looked for.
///
/// Axes tied on distinct-value count break by name. Arbitrary, and
/// deterministic — a surface that redrew itself differently between runs
/// would be worse than one that picked oddly.
pub fn surface(selection: &arvo_research::Selection) -> Option<SurfaceView> {
    use std::collections::BTreeMap;

    let mut axes: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for trial in &selection.scored {
        for (name, value) in &trial.params {
            let values = axes.entry(name.clone()).or_default();
            if !values.iter().any(|held| (held - value).abs() < f64::EPSILON) {
                values.push(*value);
            }
        }
    }
    axes.retain(|_, values| values.len() > 1);
    if axes.len() < 2 {
        return None;
    }

    let mut ranked: Vec<(String, Vec<f64>)> = axes.into_iter().collect();
    ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    let collapsed = ranked
        .iter()
        .skip(2)
        .map(|(name, _)| name.clone())
        .collect();
    let (y_axis, mut y_values) = ranked.remove(1);
    let (x_axis, mut x_values) = ranked.remove(0);
    x_values.sort_by(f64::total_cmp);
    y_values.sort_by(f64::total_cmp);

    let best_params = selection
        .scored
        .iter()
        .max_by(|a, b| a.sharpe.total_cmp(&b.sharpe))
        .map(|trial| trial.params.clone())
        .unwrap_or_default();

    let mut cells: BTreeMap<(String, String), SurfaceCell> = BTreeMap::new();
    for trial in &selection.scored {
        let (Some(x), Some(y)) = (trial.params.get(&x_axis), trial.params.get(&y_axis)) else {
            continue;
        };
        let key = (x.to_string(), y.to_string());
        let selected = trial.params == best_params;
        let cell = cells.entry(key).or_insert(SurfaceCell {
            x: *x,
            y: *y,
            sharpe: f64::NEG_INFINITY,
            selected: false,
            above_null: false,
        });
        // Best over the collapsed axes, not mean. See the note above.
        if trial.sharpe > cell.sharpe {
            cell.sharpe = trial.sharpe;
        }
        cell.selected |= selected;
    }

    let mut cells: Vec<SurfaceCell> = cells.into_values().collect();
    for cell in &mut cells {
        cell.above_null = selection
            .expected_best_under_null
            .is_none_or(|bar| cell.sharpe > bar);
    }

    Some(SurfaceView {
        x_axis,
        y_axis,
        x_values,
        y_values,
        best: selection.best_sharpe,
        null_bar: selection.expected_best_under_null,
        cells,
        collapsed,
    })
}

/// Flattens a study for display.
///
/// Public because the projection *is* what this crate does, and because the
/// test that proves trade markers land on real candles has to run in its own
/// process — a backtest installs Nautilus's logger, and there is a guard test
/// in this crate asserting nothing has claimed that global.
pub fn study_view(
    found: &arvo_research::FamilyEvidence,
    bars: &dyn arvo_data::BarProvider,
    engine: &str,
) -> StudyView {
    let evaluation = &found.out_of_sample_evidence.evaluation;
    StudyView {
        // Filled in when the finding is stored, or read back from it.
        id: String::new(),
        instrument: found.selected.instrument.clone(),
        instruments: found.selected.instruments(),
        members: members(&found.selected, &evaluation.strategy_ledger),
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        trials: arvo_api::count(found.selection.trials),
        prior_trials: arvo_api::count(found.selection.prior_trials),
        best_sharpe: found.selection.best_sharpe,
        expected_best_under_null: found.selection.expected_best_under_null,
        survived_deflation: found.selection.survived_deflation,
        surface: surface(&found.selection),
        in_sample: format!("{} → {}", found.in_sample.from, found.in_sample.to),
        out_of_sample: format!("{} → {}", found.out_of_sample.from, found.out_of_sample.to),
        selected_params: found
            .selected
            .strategy
            .params
            .iter()
            .map(|(name, value)| arvo_api::NamedNumber { name: name.clone(), value: *value })
            .collect(),
        strategy: Some(metrics_view(&evaluation.strategy)),
        benchmark: Some(metrics_view(&evaluation.benchmark)),
        excess_return: evaluation.excess_return,
        dividend_gap: dividend_gap_view(
            evaluation.dividend_gap.as_ref(),
            evaluation.excess_return,
        ),
        after_tax: after_tax_view(evaluation),
        strategy_curve: curve_points(&evaluation.strategy_curve),
        benchmark_curve: curve_points(&evaluation.benchmark_curve),
        price: candles(
            bars,
            &found.selected.instrument,
            found.selected.interval,
            &found.out_of_sample,
        ),
        markers: markers(
            &trades_in(
                &evaluation.strategy_ledger,
                &found.selected.instrument,
                &found.selected.instrument,
            ),
            found.selected.interval,
        ),
        // Every member but the head, which is `price`/`markers` above.
        alongside_charts: found
            .selected
            .alongside
            .iter()
            .map(|instrument| InstrumentChartView {
                instrument: instrument.clone(),
                price: candles(
                    bars,
                    instrument,
                    found.selected.interval,
                    &found.out_of_sample,
                ),
                markers: markers(
                    &trades_in(
                        &evaluation.strategy_ledger,
                        instrument,
                        &found.selected.instrument,
                    ),
                    found.selected.interval,
                ),
            })
            .collect(),
        trades: trade_rows(&evaluation.strategy_ledger),
        data_findings: data_findings(
            bars,
            &found.selected.instrument,
            found.selected.interval,
            &found.out_of_sample,
        ),
        underwater: underwater(&evaluation.strategy_curve),
        monthly: arvo_research::evaluation::monthly_returns(&evaluation.strategy_curve)
            .into_iter()
            .map(|month| MonthlyReturnView {
                year: month.year,
                month: month.month,
                value: month.value,
            })
            .collect(),
        trades_detail: Some(trades_view(
            &evaluation.strategy_trades,
            found.selected.starting_cash,
        )),
        recommendations: advice(arvo_research::recommend(found)),
        dataset_version: found.selected.dataset.version.clone(),
        strategy_name: found.selected.strategy.name.clone(),
        starting_cash: found.selected.starting_cash,
        commission_bps: found.selected.costs.commission_bps,
        slippage_bps: found.selected.costs.slippage_bps,
        engine: engine.to_owned(),
    }
}

/// Flattens a walk-forward run for display. Same reasoning as [`study_view`]:
/// one projection, so a finding read back from memory renders exactly as the
/// run that produced it.
pub fn walk_forward_view(
    found: &arvo_research::WalkForwardEvidence,
    bars: &dyn arvo_data::BarProvider,
    engine: &str,
) -> WalkForwardView {
    let template = &found.template;
    WalkForwardView {
        // Filled in when the finding is stored, or read back from it.
        id: String::new(),
        instrument: template.instrument.clone(),
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        recommendations: advice(arvo_research::recommend_walk_forward(found)),
        folds: found
            .folds
            .iter()
            .map(|fold| {
                let evaluation = &fold.out_of_sample_evidence.evaluation;
                FoldView {
                    chose_on: format!("{} → {}", fold.in_sample.from, fold.in_sample.to),
                    judged_on: format!("{} → {}", fold.out_of_sample.from, fold.out_of_sample.to),
                    // Only what the grid varied. Carrying the fixed parameters
                    // into every row would bury the one thing this table is
                    // for, which is watching the selection move.
                    params: fold
                        .selected
                        .strategy
                        .params
                        .iter()
                        .filter(|(name, _)| {
                            found.stability.iter().any(|axis| &axis.axis == *name)
                        })
                        .map(|(name, value)| arvo_api::NamedNumber { name: name.clone(), value: *value })
                        .collect(),
                    strategy_return: evaluation.strategy.total_return,
                    benchmark_return: evaluation.benchmark.total_return,
                    trades: evaluation.strategy.trades,
                    survived_deflation: fold.selection.survived_deflation,
                }
            })
            .collect(),
        folds_surviving_deflation: arvo_api::count(found.folds_surviving_deflation),
        folds_without_trades: arvo_api::count(found.folds_without_trades),
        stability: found
            .stability
            .iter()
            .map(|axis| StabilityView {
                axis: axis.axis.clone(),
                distinct: arvo_api::count(axis.distinct),
                modal: axis.modal,
                modal_share: axis.modal_share,
            })
            .collect(),
        strategy: Some(metrics_view(&found.combined)),
        benchmark: Some(metrics_view(&found.benchmark)),
        excess_return: found.excess_return,
        strategy_curve: curve_points(&found.combined_curve),
        benchmark_curve: curve_points(&found.benchmark_curve),
        // Every fold's judged period, end to end — which is the whole span
        // after the first selection window, so the price chart covers exactly
        // what the stitched record covers.
        price: found
            .folds
            .first()
            .zip(found.folds.last())
            .and_then(|(first, last)| {
                DateRange::new(first.out_of_sample.from, last.out_of_sample.to).ok()
            })
            .map(|window| {
                candles(
                    bars,
                    &template.instrument,
                    template.interval,
                    &window,
                )
            })
            .unwrap_or_default(),
        markers: markers(
            &found
                .folds
                .iter()
                .flat_map(|fold| {
                    fold.out_of_sample_evidence
                        .evaluation
                        .strategy_ledger
                        .iter()
                        .cloned()
                })
                .collect::<Vec<_>>(),
            template.interval,
        ),
        underwater: underwater(&found.combined_curve),
        data_findings: found
            .folds
            .first()
            .zip(found.folds.last())
            .and_then(|(first, last)| {
                DateRange::new(first.out_of_sample.from, last.out_of_sample.to).ok()
            })
            .map(|window| {
                data_findings(bars, &template.instrument, template.interval, &window)
            })
            .unwrap_or_default(),
        trades: trade_rows(
            &found
                .folds
                .iter()
                .flat_map(|fold| {
                    fold.out_of_sample_evidence
                        .evaluation
                        .strategy_ledger
                        .iter()
                        .cloned()
                })
                .collect::<Vec<_>>(),
        ),
        trades_detail: Some(trades_view(&found.combined_trades, template.starting_cash)),
        in_sample_days: found.in_sample_days,
        step_days: found.step_days,
        anchored: found.anchored,
        dataset_version: template.dataset.version.clone(),
        strategy_name: template.strategy.name.clone(),
        starting_cash: template.starting_cash,
        commission_bps: template.costs.commission_bps,
        slippage_bps: template.costs.slippage_bps,
        engine: engine.to_owned(),
    }
}

/// Recommendations as the window shows them.
/// The measured dividend bias, as the window sees it.
///
/// The corrected margin is computed here rather than left to the front end:
/// it must be the same subtraction the recommendation text did, not a second
/// one that can drift from it — including the case where there is no valid
/// subtraction to make, which a front end doing its own arithmetic would have
/// no way to know.
/// Both sides of a study after tax, computed when shown rather than stored, so
/// every finding on disk gets it and a change of rates changes none of them.
pub fn after_tax_view(evaluation: &arvo_research::Evaluation) -> Option<AfterTaxView> {
    use arvo_research::tax;
    let rates = tax::ASSUMED;
    let strategy = tax::strategy(&evaluation.strategy_ledger, &evaluation.strategy_curve, rates)?;
    let benchmark = tax::buy_and_hold(&evaluation.benchmark_curve, rates)?;
    Some(AfterTaxView {
        strategy_return: strategy.after_tax_return,
        benchmark_return: benchmark.after_tax_return,
        excess: strategy.after_tax_return - benchmark.after_tax_return,
        strategy_tax: strategy.tax,
        benchmark_tax: benchmark.tax,
        short_term_rate: rates.short_term,
        long_term_rate: rates.long_term,
    })
}

pub fn dividend_gap_view(
    gap: Option<&arvo_research::DividendGap>,
    excess_return: f64,
) -> Option<DividendGapView> {
    gap.map(|gap| DividendGapView {
        events: arvo_api::count(gap.events),
        strategy_income: gap.strategy_income,
        benchmark_income: gap.benchmark_income,
        overstatement: gap.overstatement,
        corrected_excess: gap.corrected_excess(excess_return),
        complete: gap.complete(),
        covered: arvo_api::count(gap.covered),
        instruments: arvo_api::count(gap.instruments),
    })
}

pub fn advice(items: Vec<arvo_research::Recommendation>) -> Vec<RecommendationView> {
    items
        .into_iter()
        .map(|item| RecommendationView {
            severity: item.severity.label().to_owned(),
            finding: item.finding,
            action: item.action,
            evidence: item.evidence,
        })
        .collect()
}

pub fn panel_view(found: &arvo_research::PanelEvidence, engine: &str) -> PanelView {
    PanelView {
        // Filled in when the finding is stored, or read back from it.
        id: String::new(),
        universe: found.universe.as_ref().map(|universe| universe.name.clone()).unwrap_or_default(),
        notes: found.universe.as_ref().map_or_else(Vec::new, |universe| {
            let mut notes = vec![
                format!("Universe {}: {}", universe.name, universe.reason),
                format!(
                    "{} member(s); keeping the best of them would be a search of {}, and the deflation here is over the grid, not the members",
                    universe.size, universe.size
                ),
                "Membership is as listed today, not point-in-time (#9)".to_owned(),
            ];
            if !universe.missing.is_empty() {
                notes.push(format!("{} member(s) had no series at this interval and were left out: {}", universe.missing.len(), universe.missing.join(", ")));
            }
            notes
        }),
        verdict: verdict_label(found.verdict).to_owned(),
        reasons: found.reasons.clone(),
        // The same criteria the panel was judged against. Passed rather than
        // read off the evidence because a panel does not store its own bar.
        recommendations: advice(arvo_research::recommend_panel(
            found,
            &arvo_research::EvaluationCriteria::default(),
        )),
        instruments: arvo_api::count(found.pooled.instruments),
        total_trades: found.pooled.total_trades,
        mean_excess_return: found.pooled.mean_excess_return,
        beat_benchmark: arvo_api::count(found.pooled.beat_benchmark),
        mean_max_drawdown: found.pooled.mean_max_drawdown,
        book: found.book.as_ref().map(|metrics| BookView {
            metrics: Some(metrics_view(metrics)),
            diversification: arvo_research::book::diversification(
                metrics.max_drawdown,
                found.pooled.mean_max_drawdown,
            ),
        }),
        breadth: found.breadth.as_ref().map(|breadth| BreadthView {
            instruments: breadth.instruments.clone(),
            correlations: breadth
                .correlations
                .iter()
                .map(|row| arvo_api::CorrelationRow {
                    cells: row.iter().map(|cell| arvo_api::OptionalDouble { value: *cell }).collect(),
                })
                .collect(),
            mean_correlation: breadth.mean_correlation,
            effective: breadth.effective,
            overstatement: breadth.overstatement(),
        }),
        worst_max_drawdown: found.pooled.worst_max_drawdown,
        trials: arvo_api::count(found.selection.trials),
        best_sharpe: found.selection.best_sharpe,
        expected_best_under_null: found.selection.expected_best_under_null,
        survived_deflation: found.selection.survived_deflation,
        in_sample: format!("{} → {}", found.in_sample.from, found.in_sample.to),
        out_of_sample: format!("{} → {}", found.out_of_sample.from, found.out_of_sample.to),
        selected_params: found
            .selected_params
            .iter()
            .map(|(name, value)| arvo_api::NamedNumber { name: name.clone(), value: *value })
            .collect(),
        per_instrument: found
            .per_instrument
            .iter()
            .map(|outcome| OutcomeView {
                instrument: outcome.instrument.clone(),
                strategy_return: outcome.strategy.total_return,
                benchmark_return: outcome.benchmark.total_return,
                excess_return: outcome.excess_return,
                max_drawdown: outcome.strategy.max_drawdown,
                trades: outcome.strategy.trades,
            })
            .collect(),
        failures: found.failures.clone(),
        dataset_version: found.dataset.version.clone(),
        strategy_name: STRATEGY.to_owned(),
        starting_cash: STARTING_CASH,
        commission_bps: COMMISSION_BPS,
        slippage_bps: SLIPPAGE_BPS,
        engine: engine.to_owned(),
    }
}

pub const fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Supported => "Supported",
        Verdict::NotSupported => "Not supported",
        Verdict::Inconclusive => "Inconclusive",
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;
    use arvo_research::{ScoredTrial, Selection};

    fn trial(pairs: &[(&str, f64)], sharpe: f64) -> ScoredTrial {
        ScoredTrial {
            params: pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value))
                .collect(),
            sharpe,
        }
    }

    fn selection(scored: Vec<ScoredTrial>, bar: Option<f64>) -> Selection {
        let best = scored
            .iter()
            .map(|trial| trial.sharpe)
            .fold(f64::NEG_INFINITY, f64::max);
        Selection {
            trials: scored.len(),
            best_sharpe: best,
            expected_best_under_null: bar,
            survived_deflation: bar.is_none_or(|bar| best > bar),
            prior_trials: 0,
            scored,
        }
    }

    #[test]
    fn a_grid_that_varies_one_thing_has_no_surface_to_draw() {
        // A surface has two dimensions. One axis is a list, and the winning
        // parameters line already says what it would say.
        let scored = vec![
            trial(&[("fast", 5.0), ("trade_size", 100.0)], 0.4),
            trial(&[("fast", 10.0), ("trade_size", 100.0)], 0.6),
        ];
        assert!(surface(&selection(scored, Some(0.5))).is_none());
    }

    #[test]
    fn two_axes_become_the_two_axes() {
        let mut scored = Vec::new();
        for fast in [5.0, 10.0] {
            for slow in [30.0, 60.0, 120.0] {
                scored.push(trial(&[("fast", fast), ("slow", slow)], fast + slow));
            }
        }
        let drawn = surface(&selection(scored, Some(1.0))).expect("two axes");

        // The one with more distinct values goes on x, so the grid is wider
        // than it is tall rather than the other way round.
        assert_eq!(drawn.x_axis, "slow");
        assert_eq!(drawn.y_axis, "fast");
        assert_eq!(drawn.x_values, vec![30.0, 60.0, 120.0]);
        assert_eq!(drawn.y_values, vec![5.0, 10.0]);
        assert_eq!(drawn.cells.len(), 6);
        assert!(drawn.collapsed.is_empty());
    }

    #[test]
    fn a_third_axis_is_collapsed_by_taking_the_best_and_is_named() {
        // Averaging over a hidden axis would blend a good configuration with a
        // bad one and hide exactly the region this chart is drawn to find.
        // Saying which axis was collapsed is what stops the picture being read
        // as the whole search.
        // Distinct counts, no ties: `fast` explores four values, `slow`
        // three, `atr` two. So `fast` and `slow` are the axes the search
        // actually explored and the ones drawn, and `atr` is collapsed.
        let mut scored = Vec::new();
        for fast in [5.0, 10.0, 15.0, 20.0] {
            for slow in [30.0, 60.0, 120.0] {
                for atr in [1.0, 2.0] {
                    // One standout, hidden on the collapsed axis.
                    let sharpe = if fast == 5.0 && slow == 30.0 && atr == 2.0 {
                        1.8
                    } else {
                        0.3
                    };
                    scored.push(trial(
                        &[("fast", fast), ("slow", slow), ("atr", atr)],
                        sharpe,
                    ));
                }
            }
        }
        let drawn = surface(&selection(scored, Some(1.0))).expect("three axes");

        assert_eq!(drawn.x_axis, "fast", "the most-explored axis goes across");
        assert_eq!(drawn.y_axis, "slow");
        assert_eq!(drawn.collapsed, vec!["atr".to_owned()]);
        assert_eq!(drawn.cells.len(), 12, "one cell per drawn pair, not per trial");

        let corner = drawn
            .cells
            .iter()
            .find(|cell| {
                (cell.x - 5.0).abs() < f64::EPSILON && (cell.y - 30.0).abs() < f64::EPSILON
            })
            .expect("fast 5, slow 30");
        assert!(
            (corner.sharpe - 1.8).abs() < 1e-9,
            "the best over the collapsed axis, not the mean: {}",
            corner.sharpe
        );
    }

    #[test]
    fn a_cell_below_the_no_skill_bar_is_marked_as_not_a_result() {
        // The distinction the whole chart is drawn around. A score a
        // coin-flipping search of this size would have been expected to reach
        // anyway is not a weak finding, it is not a finding.
        let scored = vec![
            trial(&[("fast", 5.0), ("slow", 30.0)], 0.9),
            trial(&[("fast", 5.0), ("slow", 60.0)], 1.4),
            trial(&[("fast", 10.0), ("slow", 30.0)], 0.2),
            trial(&[("fast", 10.0), ("slow", 60.0)], 0.5),
        ];
        let drawn = surface(&selection(scored, Some(1.0))).expect("two axes");

        assert_eq!(
            drawn.cells.iter().filter(|cell| cell.above_null).count(),
            1,
            "only the 1.4 clears a bar of 1.0"
        );
        let chosen: Vec<_> = drawn.cells.iter().filter(|cell| cell.selected).collect();
        assert_eq!(chosen.len(), 1);
        assert!((chosen[0].sharpe - 1.4).abs() < 1e-9);
    }

    #[test]
    fn with_no_bar_to_clear_nothing_is_claimed_to_have_cleared_it() {
        // Too few trials to say what a no-skill search would produce. Marking
        // everything as beating a bar that was never computed would be the
        // most flattering possible default.
        let scored = vec![
            trial(&[("fast", 5.0), ("slow", 30.0)], 0.9),
            trial(&[("fast", 10.0), ("slow", 60.0)], 1.4),
        ];
        let drawn = surface(&selection(scored, None)).expect("two axes");
        assert_eq!(drawn.null_bar, None);
        assert!(
            drawn.cells.iter().all(|cell| cell.above_null),
            "with no bar there is nothing to fail, and the UI shades none of it"
        );
    }

    #[test]
    fn a_configuration_that_never_ran_leaves_a_hole_not_a_zero() {
        // It did not score badly; it did not score. A zero would be drawn as
        // a real, poor result.
        let scored = vec![
            trial(&[("fast", 5.0), ("slow", 30.0)], 0.9),
            trial(&[("fast", 5.0), ("slow", 60.0)], 1.4),
            trial(&[("fast", 10.0), ("slow", 30.0)], 0.2),
        ];
        let drawn = surface(&selection(scored, Some(1.0))).expect("two axes");
        assert_eq!(drawn.x_values.len() * drawn.y_values.len(), 4);
        assert_eq!(drawn.cells.len(), 3, "the fourth pair is absent, not zero");
    }
}

#[cfg(test)]
mod chart_tests {
    use super::*;
    use arvo_research::{Direction, ExitReason, Trade};

    fn at(day: u32, hour: u32, minute: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid")
            .and_hms_opt(hour, minute, 0)
            .expect("valid")
    }

    /// The curve a chart draws is the rule's own indicator: declared in the
    /// rule language, warmed up the same way, and named for a legend.
    #[test]
    fn an_indicator_curve_starts_when_the_rule_would_first_see_a_value() {
        let bars: Vec<arvo_data::Bar> = (1..=5)
            .map(|day| {
                let close = f64::from(day) * 10.0;
                arvo_data::Bar { at: at(day, 0, 0), open: close, high: close, low: close, close, volume: 1.0 }
            })
            .collect();
        let params = std::collections::BTreeMap::from([("n".to_owned(), 3.0)]);
        let resolved = resolve_indicator(r#"{"kind":"SMA","period":"n"}"#, &params).expect("resolves");
        let curve = indicator_curve(&bars, resolved);

        assert_eq!(curve.name, "SMA(close, 3)");
        let times: Vec<i64> = curve.points.iter().map(|point| point.time).collect();
        assert_eq!(times, vec![at(3, 0, 0).and_utc().timestamp(), at(4, 0, 0).and_utc().timestamp(), at(5, 0, 0).and_utc().timestamp()]);
        let values: Vec<f64> = curve.points.iter().map(|point| point.value).collect();
        assert_eq!(values, vec![20.0, 30.0, 40.0]);

        let missing = resolve_indicator(r#"{"kind":"SMA","period":"n"}"#, &std::collections::BTreeMap::new());
        assert!(missing.is_err_and(|why| why.contains("n")), "a period naming an unsupplied parameter is refused");
        assert!(resolve_indicator("not json", &params).is_err());
    }

    fn trade(opened: chrono::NaiveDateTime, closed: Option<chrono::NaiveDateTime>) -> Trade {
        Trade {
            instrument: String::new(),
            opened,
            closed,
            direction: Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: closed.map(|_| 110.0),
            pnl: 100.0,
            commission: 1.0,
            exit_reason: closed.map_or(ExitReason::StillOpen, |_| ExitReason::Stop),
            journal: None,
        }
    }

    #[test]
    fn a_marker_lands_on_the_bar_that_caused_it_not_the_one_after() {
        // The off-by-one this shift exists for. A fill is stamped at the close
        // of the bar the signal was read from; a candle is stamped at its
        // open. Without the shift every entry appears one bar late, and the
        // chart renders perfectly while saying something false.
        let interval = arvo_data::BarInterval::new(5, arvo_data::IntervalUnit::Minute);
        let bar_opens = at(2, 9, 30);
        let filled_at_its_close = at(2, 9, 35);

        let out = markers(&[trade(filled_at_its_close, None)], interval);
        assert_eq!(out[0].time, bar_opens.and_utc().timestamp());
    }

    #[test]
    fn a_chart_shows_only_the_trades_that_happened_in_its_own_instrument() {
        // The bug this exists to stop: a book's ledger holds every member's
        // round trips, so plotting all of them on one member's prices puts
        // entries on days that instrument never traded.
        let mut mine = trade(at(2, 0, 0), Some(at(4, 0, 0)));
        mine.instrument = "AAPL.NASDAQ".to_owned();
        let mut theirs = trade(at(6, 0, 0), Some(at(8, 0, 0)));
        theirs.instrument = "MSFT.NASDAQ".to_owned();
        let ledger = [mine, theirs];

        let head = trades_in(&ledger, "AAPL.NASDAQ", "AAPL.NASDAQ");
        assert_eq!(head.len(), 1);
        assert_eq!(head[0].instrument, "AAPL.NASDAQ");

        let member = trades_in(&ledger, "MSFT.NASDAQ", "AAPL.NASDAQ");
        assert_eq!(member.len(), 1);
        assert_eq!(member[0].instrument, "MSFT.NASDAQ");
    }

    #[test]
    fn a_ledger_from_before_instruments_were_named_still_charts_against_the_head() {
        // Every stored finding has an unnamed ledger, and all of those runs
        // were single-instrument. Filtering them out by name would empty the
        // price chart of every finding already on disk.
        let ledger = [trade(at(2, 0, 0), Some(at(4, 0, 0)))];
        assert_eq!(trades_in(&ledger, "AAPL.NASDAQ", "AAPL.NASDAQ").len(), 1);
        assert!(
            trades_in(&ledger, "MSFT.NASDAQ", "AAPL.NASDAQ").is_empty(),
            "an unnamed trade belongs to the head and to nothing else"
        );
    }

    #[test]
    fn a_daily_marker_lands_on_its_own_day() {
        let out = markers(&[trade(at(3, 0, 0), None)], arvo_data::BarInterval::DAILY);
        assert_eq!(out[0].time, at(2, 0, 0).and_utc().timestamp());
    }

    #[test]
    fn a_closed_trade_yields_two_markers_and_an_open_one_yields_one() {
        let interval = arvo_data::BarInterval::DAILY;
        let out = markers(
            &[
                trade(at(2, 0, 0), Some(at(4, 0, 0))),
                trade(at(6, 0, 0), None),
            ],
            interval,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out.iter().filter(|m| m.kind == "entry").count(), 2);
        assert_eq!(out.iter().filter(|m| m.kind == "exit").count(), 1);
    }

    #[test]
    fn markers_come_out_in_time_order() {
        // Not tidiness: the chart library throws on unsorted markers, and an
        // exception crossing back into wasm takes the calling future with it.
        let interval = arvo_data::BarInterval::DAILY;
        let out = markers(
            &[
                trade(at(8, 0, 0), Some(at(9, 0, 0))),
                trade(at(2, 0, 0), Some(at(3, 0, 0))),
            ],
            interval,
        );
        assert!(out.windows(2).all(|pair| pair[0].time <= pair[1].time));
    }

    #[test]
    fn a_stop_exit_is_marked_differently_from_a_signal_exit() {
        let out = markers(
            &[trade(at(2, 0, 0), Some(at(4, 0, 0)))],
            arvo_data::BarInterval::DAILY,
        );
        let exit = out.iter().find(|m| m.kind == "exit").expect("closed");
        assert_eq!(exit.reason, "stop");
    }

    fn point(day: u32, equity: f64) -> arvo_research::EquityPoint {
        arvo_research::EquityPoint {
            at: at(day, 0, 0),
            equity,
        }
    }

    #[test]
    fn underwater_is_depth_below_the_running_peak() {
        let curve = [
            point(1, 100.0),
            point(2, 120.0),
            point(3, 90.0),
            point(4, 120.0),
        ];
        let plot = underwater(&curve);
        assert!((plot[0].value - 0.0).abs() < 1e-9, "a new peak is the surface");
        assert!((plot[1].value - 0.0).abs() < 1e-9);
        // 90 against a peak of 120 is 25% down.
        assert!((plot[2].value + 25.0).abs() < 1e-9, "{:?}", plot[2].value);
        assert!((plot[3].value - 0.0).abs() < 1e-9, "back to the peak");
        assert!(
            plot.iter().all(|p| p.value <= 0.0),
            "the plot hangs below zero, always"
        );
    }

    #[test]
    fn an_intraday_curve_keeps_every_point_rather_than_one_a_day() {
        // The previous version keyed points by date and deduplicated, which
        // silently threw away all but the last point of each day — an
        // intraday curve of 780 bars became nine points.
        let curve: Vec<_> = (0..12)
            .map(|index| arvo_research::EquityPoint {
                at: at(2, 9, 30) + chrono::Duration::minutes(5 * index),
                equity: 100.0 + index as f64,
            })
            .collect();
        assert_eq!(curve_points(&curve).len(), 12);
    }
}

#[cfg(test)]
mod member_tests {
    use super::*;

    fn experiment(head: &str, rest: &[&str]) -> arvo_research::Experiment {
        arvo_research::Experiment {
            id: ExperimentId("book".to_owned()),
            hypothesis: HypothesisId("h".to_owned()),
            instrument: head.to_owned(),
            alongside: rest.iter().map(|name| (*name).to_owned()).collect(),
            underlying: None,
            window: DateRange::new(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid"),
                chrono::NaiveDate::from_ymd_opt(2024, 6, 1).expect("valid"),
            )
            .expect("ordered"),
            interval: arvo_data::BarInterval::DAILY,
            dataset: arvo_research::DatasetRef {
                id: "bars".to_owned(),
                version: "v1".to_owned(),
                adjustment: arvo_data::source::Adjustment::Split,
            },
            strategy: arvo_research::StrategySpec {
                rule: None,
                name: "sma_cross".to_owned(),
                params: std::collections::BTreeMap::new(),
            },
            costs: arvo_research::CostModel::proportional(0.0, 0.0),
            risk: arvo_research::RiskModel::default(),
            starting_cash: 100_000.0,
            seed: 7,
        }
    }

    fn trade(instrument: &str, pnl: f64) -> arvo_research::Trade {
        let opened = chrono::NaiveDate::from_ymd_opt(2024, 2, 1)
            .expect("valid")
            .and_time(chrono::NaiveTime::MIN);
        arvo_research::Trade {
            instrument: instrument.to_owned(),
            opened,
            closed: Some(opened + chrono::Duration::days(3)),
            direction: arvo_research::Direction::Long,
            quantity: 10.0,
            entry: 100.0,
            exit: Some(110.0),
            pnl,
            commission: 1.0,
            exit_reason: arvo_research::ExitReason::Signal,
            journal: None,
        }
    }

    #[test]
    fn a_member_that_never_traded_is_reported_rather_than_left_out() {
        // The failure mode a shared account introduces. A row that is simply
        // missing looks like an instrument nobody chose, when in fact it was
        // chosen and could not be funded.
        let book = experiment("AAPL.NASDAQ", &["MSFT.NASDAQ"]);
        let split = members(&book, &[trade("AAPL.NASDAQ", 100.0)]);

        assert_eq!(split.len(), 2, "both members belong in the table");
        let quiet = split
            .iter()
            .find(|member| member.instrument == "MSFT.NASDAQ")
            .expect("the silent member is still a member");
        assert!(quiet.silent);
        assert_eq!(quiet.trades, 0);
    }

    #[test]
    fn shares_say_which_member_produced_the_return() {
        // The single most useful question about a book, and the one its own
        // headline return cannot answer.
        let book = experiment("AAPL.NASDAQ", &["MSFT.NASDAQ"]);
        let split = members(
            &book,
            &[
                trade("AAPL.NASDAQ", 90.0),
                trade("MSFT.NASDAQ", 10.0),
            ],
        );

        let head = &split[0];
        assert!(
            (head.share.expect("a book that made money has shares") - 0.9).abs() < 1e-9,
            "{:?}",
            head.share
        );
    }

    #[test]
    fn a_book_that_realised_nothing_reports_no_share_rather_than_zero() {
        // Winners and losers that cancel leave no denominator. Dividing by a
        // total near zero hands out shares in the hundreds, which reads as a
        // measurement rather than as the absence of one.
        let book = experiment("AAPL.NASDAQ", &["MSFT.NASDAQ"]);
        let split = members(
            &book,
            &[
                trade("AAPL.NASDAQ", 100.0),
                trade("MSFT.NASDAQ", -100.0),
            ],
        );

        assert!(
            split.iter().all(|member| member.share.is_none()),
            "{split:?}"
        );
        // The P&L itself is still real and still reported.
        assert!((split[0].pnl - 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_single_instrument_study_has_no_member_table() {
        // Its whole ledger is its one instrument, so the table would be a
        // column of the same name repeated.
        let study = experiment("AAPL.NASDAQ", &[]);
        assert!(members(&study, &[trade("AAPL.NASDAQ", 100.0)]).is_empty());
    }
}
