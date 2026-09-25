//! Buys (or sells) one option contract and holds it through expiry, on real data, to show
//! settlement end to end (#84). Read-only: a backtest, no broker.
//!
//! ```text
//! cargo run -p arvo-runtime --example option_expiry -- <data-dir> <contract> <underlying> [interval] [starting-cash] [strategy]
//! # e.g. ... SPY250912C00640000.AOPT SPY.AIEX 1day
//! ```

use arvo_data::option::OptionContract;
use arvo_data::{BarInterval, BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{
    CostModel, DatasetRef, DateRange, Experiment, ExperimentId, HypothesisId, OptionSpread,
    RiskModel, SimulationProvider, StrategySpec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: <data-dir> <contract> <underlying> [interval] [starting-cash] [strategy]".into());
    }
    let (root, contract_name, underlying) = (&args[0], &args[1], &args[2]);
    let interval: BarInterval = args.get(3).map_or(Ok(BarInterval::DAILY), |a| a.parse())?;
    let starting_cash: f64 = args.get(4).map_or(Ok(100_000.0), |a| a.parse())?;
    let strategy = args
        .get(5)
        .map_or(arvo_research::evaluation::BUY_AND_HOLD, String::as_str);
    let contract = OptionContract::parse(contract_name).ok_or("not an OCC contract name")?;

    let library = CsvBars::new(root);
    let premiums = library.bars(
        contract_name,
        interval,
        chrono::NaiveDate::MIN,
        chrono::NaiveDate::MAX,
    )?;
    let first = premiums.first().ok_or("the contract has no bars")?;
    let spot = library.bars(
        underlying,
        interval,
        contract.expiration,
        contract.expiration,
    )?;
    let settle = spot
        .last()
        .ok_or("the underlying has no bar on the expiration date")?
        .close;

    let experiment = Experiment {
        id: ExperimentId::from("option-expiry"),
        hypothesis: HypothesisId::from("settlement at expiry is what the contract promised"),
        instrument: contract_name.clone(),
        alongside: Vec::new(),
        underlying: Some(underlying.clone()),
        window: DateRange::new(first.at.date(), contract.expiration)?,
        interval,
        dataset: DatasetRef {
            id: contract_name.clone(),
            version: library
                .fingerprint(contract_name, interval)?
                .unwrap_or_default(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: StrategySpec {
            rule: None,
            name: strategy.to_owned(),
            params: [("trade_size".to_owned(), 100.0)].into_iter().collect(),
        },
        costs: CostModel {
            option_spread: Some(OptionSpread::MEASURED),
            ..CostModel::proportional(0.0, 0.0)
        },
        risk: RiskModel::default(),
        starting_cash,
        seed: 1,
    };

    let result = NautilusSimulation::new(CsvBars::new(root)).run(&experiment)?;
    println!(
        "{contract_name} ({strategy}): first bar {} at {:.2}; {} closed at {settle:.2} on {}, intrinsic {:.2}",
        first.at.date(),
        first.close,
        underlying,
        contract.expiration,
        contract.intrinsic(settle)
    );
    for trade in &result.ledger {
        println!(
            "  {:<26} {:>5} @ {:>8.2} -> {:>8} {:?}, pnl {:+.2}",
            trade.instrument,
            trade.quantity,
            trade.entry,
            trade
                .exit
                .map_or("open".to_owned(), |exit| format!("{exit:.2}")),
            trade.exit_reason,
            trade.pnl
        );
    }
    let end = result.equity_curve.last().ok_or("no curve")?;
    println!("  equity at {}: {:.2}", end.at, end.equity);
    let discrepancies = arvo_research::reconcile::reconcile(&experiment, &result);
    println!(
        "  reconciliation: {}",
        if discrepancies.is_empty() {
            "clean".to_owned()
        } else {
            format!("{discrepancies:?}")
        }
    );
    Ok(())
}
