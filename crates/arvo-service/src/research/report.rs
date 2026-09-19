//! A finding as a document (#159).
//!
//! Arvo judges well and presents little: a verdict, a read-this-first line
//! and a panel of numbers that exist only while the window is open. This
//! writes the same finding as Markdown — readable without Arvo, diffable in
//! git, and the thing a person actually pastes into a message to a
//! colleague.
//!
//! It composes from [`RecordView`] rather than the stored record because the
//! view is already the flattened form the window reads; a report derived
//! from anything else would be a second measurement, and two measurements of
//! one run is exactly what this platform exists to stop. Nothing here
//! decides anything: every number, verdict and caveat comes in already made.

use std::fmt::Write as _;

use arvo_views::{
    MetricsView, PanelView, RecommendationView, RecordView, ReportedView, StudyView, TradeRowView,
    TradesView, WalkForwardView,
};

/// What the report needs that the view does not carry: the finding's
/// identity, and the name of the figure written beside it.
#[derive(Debug, Clone, Default)]
pub struct ReportMeta {
    pub id: String,
    pub hypothesis: String,
    /// RFC 3339.
    pub recorded_at: String,
    /// The agent or script that ran it; empty for a person.
    pub author: String,
    /// The verdict's plain instruction on how far it may be read, as the
    /// engine says it. Passed in rather than matched on the verdict string
    /// here: there is one wording of it, and it lives with the verdict.
    pub read_this_first: String,
    /// The file name of the chart written next to the report, if one was
    /// captured. Relative, so the two travel together.
    pub figure: Option<String>,
}

/// The finding as Markdown: the verdict and how far it may be read, the
/// search it was held to, the figure, the numbers, the assumptions, and
/// every round trip behind them.
#[must_use]
pub fn compose(view: &RecordView, meta: &ReportMeta) -> String {
    let mut out = String::new();
    match view.of.as_ref() {
        Some(arvo_views::record_view::Of::Study(study)) => study_report(&mut out, study, meta),
        Some(arvo_views::record_view::Of::Walkforward(walk)) => walk_report(&mut out, walk, meta),
        Some(arvo_views::record_view::Of::Panel(panel)) => panel_report(&mut out, panel, meta),
        Some(arvo_views::record_view::Of::Reported(reported)) => reported_report(&mut out, reported, meta),
        // A kind this build does not know: say so rather than render nothing.
        None => out.push_str("This finding is of a kind this build cannot read.
"),
    }
    out
}

fn study_report(out: &mut String, study: &StudyView, meta: &ReportMeta) {
    heading(out, &format!("{} on {}", study.strategy_name, study.instrument), meta);
    verdict(out, &study.verdict, &study.reasons, meta);

    // ADR-0014: the search comes before the numbers it produced. A Sharpe of
    // 1.4 read without the two hundred configurations behind it is not a
    // result, and a reader who scrolls is a reader who missed it.
    let _ = writeln!(out, "## The search\n");
    let tried = study.trials + study.prior_trials;
    let _ = writeln!(
        out,
        "{tried} configurations were tried{}. Best in-sample Sharpe {:.2}{}.",
        if study.prior_trials > 0 {
            format!(" ({} here, {} before this experiment arrived)", study.trials, study.prior_trials)
        } else {
            String::new()
        },
        study.best_sharpe,
        study.expected_best_under_null.map_or_else(String::new, |null| format!(
            ", against {null:.2} expected from a search this size with no skill in it"
        )),
    );
    let _ = writeln!(
        out,
        "\n{}\n",
        if study.survived_deflation {
            "The winner beat what that search would throw up by chance."
        } else {
            "**The winner did not beat what that search would throw up by chance.**"
        }
    );
    let _ = writeln!(out, "Chosen on {}, judged on {}.\n", study.in_sample, study.out_of_sample);
    params(out, &study.selected_params);

    figure(out, meta);
    metrics(out, study.strategy(), study.benchmark(), study.excess_return);
    assumptions(
        out,
        &study.dataset_version,
        study.starting_cash,
        study.commission_bps,
        study.slippage_bps,
        &study.engine,
    );
    trades(out, &study.trades, study.trades_detail());
    advice(out, &study.recommendations);
}

fn walk_report(out: &mut String, walk: &WalkForwardView, meta: &ReportMeta) {
    heading(out, &format!("{} walk-forward", walk.instrument), meta);
    verdict(out, &walk.verdict, &walk.reasons, meta);

    let _ = writeln!(out, "## The search\n");
    let _ = writeln!(
        out,
        "{} folds, re-selecting every {} days on {} of history{}. {} of {} folds beat what a search that size would throw up by chance{}.\n",
        walk.folds.len(),
        walk.step_days,
        walk.in_sample_days,
        if walk.anchored { ", anchored" } else { ", rolling" },
        walk.folds_surviving_deflation,
        walk.folds.len(),
        if walk.folds_without_trades > 0 {
            format!("; {} opened no position at all", walk.folds_without_trades)
        } else {
            String::new()
        },
    );
    if !walk.folds.is_empty() {
        let _ = writeln!(out, "| Chose on | Judged on | Strategy | Benchmark | Trades | Beat chance |");
        let _ = writeln!(out, "| --- | --- | ---: | ---: | ---: | --- |");
        for fold in &walk.folds {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                fold.chose_on,
                fold.judged_on,
                pct(fold.strategy_return),
                pct(fold.benchmark_return),
                fold.trades,
                yes_no(fold.survived_deflation),
            );
        }
        out.push('\n');
    }
    if !walk.stability.is_empty() {
        let _ = writeln!(out, "How the selection moved:\n");
        let _ = writeln!(out, "| Parameter | Distinct values | Most common | Share of folds |");
        let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
        for axis in &walk.stability {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                axis.axis,
                axis.distinct,
                trim(axis.modal),
                pct(axis.modal_share)
            );
        }
        out.push('\n');
    }

    figure(out, meta);
    metrics(out, walk.strategy(), walk.benchmark(), walk.excess_return);
    assumptions(
        out,
        &walk.dataset_version,
        walk.starting_cash,
        walk.commission_bps,
        walk.slippage_bps,
        &walk.engine,
    );
    trades(out, &walk.trades, walk.trades_detail());
    advice(out, &walk.recommendations);
}

fn panel_report(out: &mut String, panel: &PanelView, meta: &ReportMeta) {
    heading(out, &format!("{} across {} instruments", panel.strategy_name, panel.instruments), meta);
    verdict(out, &panel.verdict, &panel.reasons, meta);

    let _ = writeln!(out, "## The search\n");
    let _ = writeln!(
        out,
        "{} configurations were tried. Best in-sample Sharpe {:.2}{}. {}\n",
        panel.trials,
        panel.best_sharpe,
        panel.expected_best_under_null.map_or_else(String::new, |null| format!(
            ", against {null:.2} expected with no skill in it"
        )),
        if panel.survived_deflation {
            "The winner beat what that search would throw up by chance."
        } else {
            "**The winner did not beat what that search would throw up by chance.**"
        },
    );
    let _ = writeln!(out, "Chosen on {}, judged on {}.\n", panel.in_sample, panel.out_of_sample);
    params(out, &panel.selected_params);

    figure(out, meta);
    let _ = writeln!(out, "## Across the panel\n");
    let _ = writeln!(
        out,
        "| Instruments | Beat benchmark | Mean excess | Mean drawdown | Worst drawdown | Trades |",
    );
    let _ = writeln!(out, "| ---: | ---: | ---: | ---: | ---: | ---: |");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} | {} |\n",
        panel.instruments,
        panel.beat_benchmark,
        pct(panel.mean_excess_return),
        pct(panel.mean_max_drawdown),
        pct(panel.worst_max_drawdown),
        panel.total_trades,
    );
    if !panel.per_instrument.is_empty() {
        let _ = writeln!(out, "| Instrument | Strategy | Benchmark | Excess | Drawdown | Trades |");
        let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: | ---: |");
        for outcome in &panel.per_instrument {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                outcome.instrument,
                pct(outcome.strategy_return),
                pct(outcome.benchmark_return),
                pct(outcome.excess_return),
                pct(outcome.max_drawdown),
                outcome.trades,
            );
        }
        out.push('\n');
    }
    if !panel.failures.is_empty() {
        let _ = writeln!(out, "Instruments that could not be run: {}.\n", panel.failures.join(", "));
    }
    assumptions(
        out,
        &panel.dataset_version,
        panel.starting_cash,
        panel.commission_bps,
        panel.slippage_bps,
        &panel.engine,
    );
    advice(out, &panel.recommendations);
}

fn reported_report(out: &mut String, reported: &ReportedView, meta: &ReportMeta) {
    heading(
        out,
        &format!("{} on {}, reported by {}", reported.strategy, reported.instrument, reported.engine),
        meta,
    );
    if !reported.claim.is_empty() {
        let _ = writeln!(out, "> {}\n", reported.claim);
    }
    verdict(out, &reported.verdict, &reported.reasons, meta);

    // ADR-0026: Arvo did not compute these numbers, and a reader of the
    // document has to know that before reading them.
    let _ = writeln!(
        out,
        "The evidence below was computed by {}, not by Arvo. Arvo judged it and nothing more.\n",
        reported.engine
    );
    let _ = writeln!(out, "## The search\n");
    let _ = writeln!(
        out,
        "{}\n",
        reported.trials.map_or_else(
            || "The author did not say how many configurations their search tried, so the verdict is not held to one.".to_owned(),
            |trials| format!("{trials} configurations were tried, as the author counted them."),
        )
    );

    figure(out, meta);
    let _ = writeln!(out, "## The numbers\n");
    let _ = writeln!(out, "| Measure | Value |");
    let _ = writeln!(out, "| --- | ---: |");
    let _ = writeln!(out, "| Total return | {} |", maybe_pct(reported.total_return));
    let _ = writeln!(out, "| Excess over buy and hold | {} |", maybe_pct(reported.excess_return));
    let _ = writeln!(out, "| Sharpe | {} |", reported.sharpe.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}")));
    let _ = writeln!(out, "| Max drawdown | {} |", maybe_pct(reported.max_drawdown));
    let _ = writeln!(out, "| Trades | {} |\n", reported.trades);

    let _ = writeln!(out, "## What it assumed\n");
    let _ = writeln!(out, "- Window: {} to {}, {}", reported.from, reported.to, reported.interval);
    let _ = writeln!(out, "- Dataset: `{}` (the author's own, not in Arvo's library)", reported.dataset);
    let _ = writeln!(out, "- Engine: {}\n", reported.engine);
}

/// Title, and the line that says what this is a report of.
fn heading(out: &mut String, subject: &str, meta: &ReportMeta) {
    let _ = writeln!(out, "# {subject}\n");
    let _ = writeln!(
        out,
        "Hypothesis `{}`, recorded {} by {}. Finding `{}`.\n",
        meta.hypothesis,
        meta.recorded_at,
        if meta.author.is_empty() { "a person" } else { &meta.author },
        meta.id,
    );
}

/// The verdict, how far it may be read, and why it came out that way —
/// before any number, which is the order the window shows them in.
fn verdict(out: &mut String, verdict: &str, reasons: &[String], meta: &ReportMeta) {
    let _ = writeln!(out, "## {}\n", spaced(verdict));
    if !meta.read_this_first.is_empty() {
        let _ = writeln!(out, "**{}**\n", meta.read_this_first);
    }
    for reason in reasons {
        let _ = writeln!(out, "- {reason}");
    }
    if !reasons.is_empty() {
        out.push('\n');
    }
}

fn figure(out: &mut String, meta: &ReportMeta) {
    if let Some(name) = &meta.figure {
        let _ = writeln!(out, "![Strategy against buy and hold]({name})\n");
    }
}

fn params(out: &mut String, chosen: &[arvo_views::NamedNumber]) {
    if chosen.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "Chosen configuration: {}.\n",
        chosen.iter().map(|chose| format!("`{}` {}", chose.name, trim(chose.value))).collect::<Vec<_>>().join(", ")
    );
}

fn metrics(out: &mut String, strategy: &MetricsView, benchmark: &MetricsView, excess: f64) {
    let _ = writeln!(out, "## Out of sample\n");
    let _ = writeln!(out, "| Measure | Strategy | Buy and hold |");
    let _ = writeln!(out, "| --- | ---: | ---: |");
    let _ = writeln!(out, "| Total return | {} | {} |", pct(strategy.total_return), pct(benchmark.total_return));
    let _ = writeln!(out, "| CAGR | {} | {} |", pct(strategy.cagr), pct(benchmark.cagr));
    let _ = writeln!(out, "| Max drawdown | {} | {} |", pct(strategy.max_drawdown), pct(benchmark.max_drawdown));
    let _ = writeln!(out, "| Volatility | {} | {} |", pct(strategy.volatility), pct(benchmark.volatility));
    let _ = writeln!(out, "| Sharpe | {} | {} |", ratio(strategy.sharpe), ratio(benchmark.sharpe));
    let _ = writeln!(out, "| Sortino | {} | {} |", ratio(strategy.sortino), ratio(benchmark.sortino));
    let _ = writeln!(out, "| Calmar | {} | {} |", ratio(strategy.calmar), ratio(benchmark.calmar));
    // The second number a Sharpe needs: the same 1.2 from forty returns and
    // from four thousand are not the same finding.
    let _ = writeln!(
        out,
        "| P(Sharpe > 0) | {} | {} |",
        strategy.psr.map_or_else(|| "—".to_owned(), pct),
        benchmark.psr.map_or_else(|| "—".to_owned(), pct)
    );
    let _ = writeln!(out, "| Trades | {} | {} |\n", strategy.trades, benchmark.trades);
    let _ = writeln!(out, "Excess over buy and hold: **{}**.\n", pct(excess));
}

fn assumptions(
    out: &mut String,
    dataset_version: &str,
    starting_cash: f64,
    commission_bps: f64,
    slippage_bps: f64,
    engine: &str,
) {
    // A verdict without its assumptions is decoration: the same rule at zero
    // cost and at five basis points is two different findings.
    let _ = writeln!(out, "## What it assumed\n");
    let _ = writeln!(out, "- Starting cash: {starting_cash:.2}");
    let _ = writeln!(out, "- Commission: {commission_bps} bps a side");
    let _ = writeln!(out, "- Slippage: {slippage_bps} bps a side");
    let _ = writeln!(out, "- Data: `{dataset_version}`");
    let _ = writeln!(out, "- Engine: {engine}\n");
}

fn trades(out: &mut String, rows: &[TradeRowView], detail: &TradesView) {
    let _ = writeln!(out, "## Trades\n");
    let _ = writeln!(
        out,
        "{} closed, {} still open. Win rate {}, profit factor {}, expectancy {}. Fees paid {:.2} ({} of starting capital).\n",
        detail.closed,
        detail.still_open,
        detail.win_rate.map_or_else(|| "—".to_owned(), pct),
        detail.profit_factor.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}")),
        detail.expectancy.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}")),
        detail.fees_paid,
        pct(detail.fees_fraction),
    );
    if rows.is_empty() {
        return;
    }
    // Every round trip, uncut: a table that stopped at fifty would be a
    // summary of the evidence rather than the evidence.
    let book = rows.iter().any(|row| !row.instrument.is_empty());
    let _ = writeln!(
        out,
        "|{} Opened | Closed | Direction | Quantity | Entry | Exit | P&L | Fees | Held (days) | Exit |",
        if book { " Instrument |" } else { "" }
    );
    let _ = writeln!(
        out,
        "|{} --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
        if book { " --- |" } else { "" }
    );
    for row in rows {
        let _ = writeln!(
            out,
            "|{} {} | {} | {} | {} | {:.2} | {} | {:.2} | {:.2} | {} | {} |",
            if book { format!(" {} |", row.instrument) } else { String::new() },
            row.opened,
            if row.closed.is_empty() { "—" } else { &row.closed },
            row.direction,
            trim(row.quantity),
            row.entry,
            row.exit.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}")),
            row.pnl,
            row.commission,
            row.held_days.map_or_else(|| "—".to_owned(), |v| format!("{v:.1}")),
            row.exit_reason,
        );
    }
    out.push('\n');
}

fn advice(out: &mut String, recommendations: &[RecommendationView]) {
    if recommendations.is_empty() {
        return;
    }
    let _ = writeln!(out, "## What to do about it\n");
    for item in recommendations {
        let _ = writeln!(out, "**{}** — {}\n", spaced(&item.severity), item.finding);
        let _ = writeln!(out, "{}\n", item.action);
        if !item.evidence.is_empty() {
            let _ = writeln!(out, "> {}\n", item.evidence);
        }
    }
}

/// `NotSupported` reads as a type name; a document is not a type.
fn spaced(camel: &str) -> String {
    let mut out = String::with_capacity(camel.len() + 2);
    for (index, c) in camel.chars().enumerate() {
        if index > 0 && c.is_uppercase() {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn yes_no(yes: bool) -> &'static str {
    if yes { "yes" } else { "no" }
}

fn pct(value: f64) -> String {
    format!("{:.2}%", value * 100.0)
}

fn maybe_pct(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), pct)
}

fn ratio(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"))
}

/// Parameters are whole numbers as often as not, and `20.000` reads like a
/// measurement rather than a setting.
fn trim(value: f64) -> String {
    if (value.fract()).abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics_view(total: f64) -> MetricsView {
        MetricsView {
            total_return: total,
            cagr: total / 2.0,
            max_drawdown: 0.1,
            volatility: 0.2,
            sharpe: Some(1.1),
            sortino: None,
            calmar: None,
            psr: Some(0.8),
            trades: 4,
        }
    }

    fn a_study() -> StudyView {
        StudyView {
            id: "study-1".to_owned(),
            instrument: "SPY.RH".to_owned(),
            instruments: vec!["SPY.RH".to_owned()],
            members: vec![],
            verdict: "NotSupported".to_owned(),
            reasons: vec!["the winner did not survive deflation".to_owned()],
            trials: 200,
            prior_trials: 0,
            best_sharpe: 1.4,
            expected_best_under_null: Some(1.9),
            survived_deflation: false,
            surface: None,
            in_sample: "2020-01-01 to 2022-12-31".to_owned(),
            out_of_sample: "2023-01-01 to 2023-12-31".to_owned(),
            selected_params: vec![
                arvo_views::NamedNumber { name: "fast".to_owned(), value: 20.0 },
                arvo_views::NamedNumber { name: "slow".to_owned(), value: 50.5 },
            ],
            strategy: Some(metrics_view(0.12)),
            benchmark: Some(metrics_view(0.20)),
            excess_return: -0.08,
            dividend_gap: None,
            after_tax: None,
            strategy_curve: vec![],
            benchmark_curve: vec![],
            price: vec![],
            markers: vec![],
            alongside_charts: vec![],
            underwater: vec![],
            data_findings: vec![],
            trades: vec![TradeRowView {
                instrument: String::new(),
                opened: "2023-02-01".to_owned(),
                closed: "2023-02-08".to_owned(),
                direction: "long".to_owned(),
                quantity: 10.0,
                entry: 100.0,
                exit: Some(101.5),
                pnl: 15.0,
                commission: 1.0,
                held_days: Some(7.0),
                exit_reason: "signal".to_owned(),
            }],
            monthly: vec![],
            trades_detail: Some(TradesView {
                closed: 1,
                still_open: 0,
                win_rate: Some(1.0),
                profit_factor: None,
                expectancy: Some(15.0),
                average_win: Some(15.0),
                average_loss: None,
                average_holding_days: Some(7.0),
                fees_paid: 1.0,
                fees_fraction: 0.0001,
                signal_exits: 1,
                stop_exits: 0,
            }),
            recommendations: vec![RecommendationView {
                severity: "Blocking".to_owned(),
                finding: "the search was not beaten".to_owned(),
                action: "do not trade this".to_owned(),
                evidence: "200 trials, best 1.4 against 1.9 expected".to_owned(),
            }],
            dataset_version: "sha256-abc".to_owned(),
            strategy_name: "sma_cross".to_owned(),
            starting_cash: 100_000.0,
            commission_bps: 5.0,
            slippage_bps: 2.0,
            engine: "arvo 0.1".to_owned(),
        }
    }

    #[test]
    fn a_report_puts_the_verdict_and_the_search_above_the_numbers() {
        let meta = ReportMeta {
            id: "study-1".to_owned(),
            hypothesis: "h-momentum".to_owned(),
            recorded_at: "2026-09-17T10:00:00Z".to_owned(),
            author: String::new(),
            read_this_first: "Not supported: do not report them as an edge.".to_owned(),
            figure: Some("figure.png".to_owned()),
        };
        let markdown = compose(&RecordView::study(a_study()), &meta);

        let verdict = markdown.find("## Not supported").expect("the verdict");
        let search = markdown.find("## The search").expect("the search");
        let numbers = markdown.find("## Out of sample").expect("the numbers");
        assert!(verdict < search && search < numbers, "verdict, then search, then numbers");

        // ADR-0014's number, and the warning when it was not cleared.
        assert!(markdown.contains("200 configurations were tried"));
        assert!(markdown.contains("**The winner did not beat"));
        // The assumptions, the figure, the round trip, and what to do.
        assert!(markdown.contains("Commission: 5 bps a side"));
        assert!(markdown.contains("![Strategy against buy and hold](figure.png)"));
        assert!(markdown.contains("| 2023-02-01 | 2023-02-08 | long | 10 | 100.00 | 101.50 |"));
        assert!(markdown.contains("**Blocking** — the search was not beaten"));
        assert!(markdown.contains("`fast` 20, `slow` 50.5"));
    }

    #[test]
    fn a_reported_finding_says_whose_numbers_they_are() {
        let view = ReportedView {
            id: "reported-1".to_owned(),
            hypothesis: "h-momentum".to_owned(),
            claim: "momentum persists for a month".to_owned(),
            instrument: "SPY.THEIRS".to_owned(),
            engine: "their-engine 0.2".to_owned(),
            from: "2024-01-01".to_owned(),
            to: "2024-10-26".to_owned(),
            interval: "1day".to_owned(),
            dataset: "theirs:SPY@sha256-xyz".to_owned(),
            strategy: "rsi2-pullback".to_owned(),
            verdict: "Inconclusive".to_owned(),
            reasons: vec!["no benchmark was given".to_owned()],
            trades: 40,
            trials: None,
            total_return: None,
            excess_return: None,
            sharpe: None,
            max_drawdown: None,
            recorded_at: "2026-09-17T10:00:00Z".to_owned(),
            author: "script:their-engine".to_owned(),
            attachments: vec![],
        };
        // As the runtime fills it: the identity comes from the stored
        // record, not from the view.
        let meta = ReportMeta {
            id: "reported-1".to_owned(),
            hypothesis: "h-momentum".to_owned(),
            recorded_at: "2026-09-17T10:00:00Z".to_owned(),
            author: "script:their-engine".to_owned(),
            read_this_first: "Inconclusive: do not report the numbers below as a result.".to_owned(),
            figure: None,
        };
        let markdown = compose(&RecordView::reported(view), &meta);

        assert!(markdown.contains("computed by their-engine 0.2, not by Arvo"));
        assert!(markdown.contains("> momentum persists for a month"));
        assert!(markdown.contains("did not say how many configurations"));
        assert!(markdown.contains("| Total return | — |"));
        assert!(markdown.contains("by script:their-engine"));
    }
}
