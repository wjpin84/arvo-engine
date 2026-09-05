//! Runs one experiment at an explicit resolution, to prove the interval is
//! honoured end to end.
//!
//!     cargo run -p arvo-runtime --example intraday -- //!         <data-dir> <instrument> <interval> [risk-per-trade] [slippage-bps]
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
        .ok_or("usage: intraday <data-dir> <instrument> <interval> [risk] [slippage-bps]")?;
    let instrument = args.next().ok_or("missing instrument")?;
    let interval: BarInterval = args.next().ok_or("missing interval")?.parse()?;
    // Optional 4th argument: "none" for fixed sizing, so risk-based sizing
    // can be isolated as a cause rather than assumed.
    let risk_arg = args.next().unwrap_or_else(|| "0.01".to_owned());
    // Optional 5th argument. Slippage often exceeds the edge at intraday
    // resolution, so being able to vary it here is how that gets checked
    // rather than assumed.
    let slippage_bps: f64 = args.next().unwrap_or_else(|| "0".to_owned()).parse()?;

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

    println!("{instrument} at {interval}, slippage {slippage_bps} bps");
    println!("  {} bars, {} .. {}", series.len(), first.at, last.at);
    println!(
        "  {:.1} periods a year (a daily bar is {:.0})",
        interval.periods_per_year(),
        BarInterval::DAILY.periods_per_year()
    );

    let experiment = Experiment {
        id: ExperimentId::from("intraday-probe"),
        hypothesis: HypothesisId::from("a moving-average crossover works intraday"),
        instrument: instrument.clone(),
        window: DateRange::new(first.at.date(), last.at.date())?,
        interval,
        dataset: DatasetRef {
            id: instrument.clone(),
            version: bars.fingerprint(&instrument, interval)?.unwrap_or_default(),
        },
        strategy: StrategySpec {
            name: "sma_cross".to_owned(),
            params: [
                ("fast".to_owned(), 10.0),
                ("slow".to_owned(), 30.0),
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
    println!(
        "  ran: {} trades, {} equity points",
        result.trades,
        result.equity_curve.len()
    );

    match Metrics::from_curve(
        &result.equity_curve,
        result.trades,
        interval.periods_per_year(),
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
