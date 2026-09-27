//! Runs one experiment at an explicit resolution, to prove the interval is
//! honoured end to end.
//!
//!     cargo run -p arvo-runtime --example intraday -- //!         <data-dir> <instrument> <interval> [risk] [slippage-bps] [strategy]
//!
//! Exists because the interval touches four crates — the data tier, the
//! record, the engine boundary and annualisation — and a mistake in any of
//! them is invisible in the output. The Sharpe printed here is the one number
//! that would silently be wrong if annualisation were still hardcoded.

use arvo_data::{BarInterval, BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{
    CostModel, DatasetRef, DateRange, Experiment, ExperimentId, HypothesisId, Metrics, RiskModel,
    SimulationProvider, StrategySpec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .ok_or("usage: intraday <data-dir> <instrument> <interval> [risk] [slippage] [strategy]")?;
    let instrument = args.next().ok_or("missing instrument")?;
    let interval: BarInterval = args.next().ok_or("missing interval")?.parse()?;
    // The calendar the annualisation below is scaled on: the instrument's, so a
    // coin is counted over 365 days of 1440 minutes and a share over 252 of 390.
    let hours = arvo_data::Instrument::of(&instrument).hours;
    // Optional 4th argument: "none" for fixed sizing, so risk-based sizing
    // can be isolated as a cause rather than assumed.
    let risk_arg = args.next().unwrap_or_else(|| "0.01".to_owned());
    // Optional 5th argument. Slippage often exceeds the edge at intraday
    // resolution, so being able to vary it here is how that gets checked
    // rather than assumed.
    let slippage_bps: f64 = args.next().unwrap_or_else(|| "0".to_owned()).parse()?;
    // Optional 6th argument. The session-anchored rules only exist at this
    // resolution, so this example is the only place they can be exercised on
    // real data at all.
    let strategy = args.next().unwrap_or_else(|| "sma_cross".to_owned());

    let bars = CsvBars::new(&root);
    let series = bars.bars(
        &instrument,
        interval,
        chrono::NaiveDate::MIN,
        chrono::NaiveDate::MAX,
    )?;
    let (Some(first), Some(last)) = (series.first(), series.last()) else {
        println!("no {interval} bars for {instrument} under {root}");
        return Ok(());
    };

    println!("{instrument} at {interval}: {strategy}, slippage {slippage_bps} bps");

    // What is wrong with the bars, before anything is concluded from them.
    let quality = arvo_data::quality::inspect(&series, interval);
    if quality.is_clean() {
        println!("  data: nothing to report across {} bars", quality.bars);
    } else {
        println!("  data: {} findings across {} bars", quality.findings.len(), quality.bars);
        for finding in &quality.findings {
            println!(
                "    [{}] {} {}: {}",
                match finding.severity {
                    arvo_data::quality::Severity::Fault => "fault",
                    arvo_data::quality::Severity::Suspect => "suspect",
                },
                finding.kind,
                finding.at.map(|at| at.to_string()).unwrap_or_default(),
                finding.detail,
            );
        }
    }
    println!("  {} bars, {} .. {}", series.len(), first.at, last.at);
    println!(
        "  {:.1} periods a year (a daily bar is {:.0})",
        interval.periods_per_year(hours),
        BarInterval::DAILY.periods_per_year(hours)
    );

    let experiment = Experiment {
        id: ExperimentId::from("intraday-probe"),
        hypothesis: HypothesisId::from("a moving-average crossover works intraday"),
        instrument: instrument.clone(),
        alongside: Vec::new(),
        underlying: None,
        window: DateRange::new(first.at.date(), last.at.date())?,
        interval,
        dataset: DatasetRef {
            id: instrument.clone(),
            version: bars.fingerprint(&instrument, interval)?.unwrap_or_default(),
            adjustment: arvo_service::source::adjustment_across([instrument.as_str()]),
        },
        strategy: StrategySpec {
            rule: None,
            name: strategy.clone(),
            // Every rule's parameters at once. A strategy takes the ones it
            // names and ignores the rest, so one example can drive all of
            // them without a match on the name here.
            params: [
                ("fast".to_owned(), 10.0),
                ("slow".to_owned(), 30.0),
                ("range_bars".to_owned(), 6.0),
                ("target_range_multiple".to_owned(), 2.0),
                ("entry_atr_multiple".to_owned(), 1.0),
                ("atr_period".to_owned(), 14.0),
                ("entry_deviations".to_owned(), 1.5),
                ("entry_period".to_owned(), 20.0),
                ("exit_period".to_owned(), 10.0),
                ("trade_size".to_owned(), 10.0),
            ]
            .into_iter()
            .collect(),
        },
        costs: CostModel::proportional(1.0, slippage_bps),
        risk: RiskModel {
            stop_atr_multiple: Some(2.0),
            atr_period: 14,
            risk_per_trade: risk_arg.parse::<f64>().ok(),
            ..RiskModel::default()
        },
        starting_cash: 100_000.0,
        seed: 1,
    };

    let result = NautilusSimulation::new(CsvBars::new(&root)).run(&experiment)?;
    let trades = arvo_research::TradeStats::from_ledger(&result.ledger);
    println!(
        "  ran: {} trades ({} closed, {} still open), {} equity points",
        result.trades,
        trades.closed,
        trades.still_open,
        result.equity_curve.len()
    );
    println!(
        "  win rate {}, profit factor {}, {} stop exits",
        trades
            .win_rate
            .map_or_else(|| "n/a".to_owned(), |v| format!("{:.0}%", v * 100.0)),
        trades
            .profit_factor
            .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.2}")),
        trades.stop_exits
    );

    for trade in &result.ledger {
        println!(
            "    {} -> {}  {:.0} @ {:.2} -> {:.2}  pnl {:+.2}  {:?}",
            trade.opened,
            trade.closed.map_or_else(|| "open".to_owned(), |at| at.to_string()),
            trade.quantity,
            trade.entry,
            trade.exit.unwrap_or(f64::NAN),
            trade.pnl,
            trade.exit_reason
        );
    }
    let realised: f64 = result.ledger.iter().map(|t| t.pnl).sum();
    println!(
        "  realised {realised:+.2} vs curve {:+.2}",
        result.equity_curve.last().map_or(0.0, |p| p.equity) - experiment.starting_cash
    );

    match Metrics::from_curve(
        &result.equity_curve,
        result.trades,
        interval.periods_per_year(hours),
    ) {
        None => println!("  too few equity points to evaluate"),
        Some(metrics) => {
            println!("  return {:+.3}%", metrics.total_return * 100.0);
            println!(
                "  volatility {:.2}%  (annualised at this resolution)",
                metrics.volatility * 100.0
            );
            // The number that would silently be wrong if annualisation were
            // still a hardcoded 252.
            println!("  sharpe {:?}", metrics.sharpe.map(|s| format!("{s:.2}")));
        }
    }
    Ok(())
}
