//! Sells SPY put spreads over a library's chain and judges the result against
//! holding SPY — a month out (#86) or the same day (#87). A backtest:
//! read-only, no broker.
//!
//! ```text
//! cargo run -p arvo-runtime --example put_spread -- <data-dir> <from> <to> [key=value ...]
//! # a month out, daily bars:
//! ... put_spread -- lib 2024-01-02 2026-08-21 short_delta=0.2 dte=35
//! # the same day, five-minute bars:
//! ... put_spread -- lib 2026-03-02 2026-09-11 strategy=zero_dte_put_spread short_delta=0.1
//! # buying same-day options on an opening-range break:
//! ... put_spread -- lib 2026-03-02 2026-09-11 strategy=zero_dte_breakout delta=0.5
//! ```
//!
//! Any rule parameter is a `key=value`; `cash` and `spreads` size the account
//! and the trade; `min_half_spread` and `half_spread_fraction` replace the
//! measured option spread, to see how much a verdict rests on it. The chain comes from `fetch_chain`, the underlying from
//! `fetch --source alpaca-iex`. Rate 4% and dividend yield 1.3% unless stated.

use std::collections::BTreeMap;

use arvo_data::{BarInterval, BarProvider, CsvBars};
use arvo_nautilus::{NautilusSimulation, PUT_SPREAD, ZERO_DTE_BREAKOUT, ZERO_DTE_PUT_SPREAD};
use arvo_research::{
    CostModel, DatasetRef, DateRange, EvaluationCriteria, Experiment, ExperimentId, HypothesisId,
    OptionSpread, RiskModel, StrategySpec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: <data-dir> <from> <to> [key=value ...]".into());
    }
    let root = &args[0];
    let window = DateRange::new(args[1].parse()?, args[2].parse()?)?;
    let mut given: BTreeMap<String, String> = BTreeMap::new();
    for pair in &args[3..] {
        let (key, value) = pair.split_once('=').ok_or("parameters are key=value")?;
        given.insert(key.to_owned(), value.to_owned());
    }
    let strategy = given
        .remove("strategy")
        .unwrap_or_else(|| PUT_SPREAD.to_owned());
    let same_day = strategy == ZERO_DTE_PUT_SPREAD || strategy == ZERO_DTE_BREAKOUT;
    let number = |given: &mut BTreeMap<String, String>,
                  key: &str,
                  default: f64|
     -> Result<f64, Box<dyn std::error::Error>> {
        Ok(given.remove(key).map_or(Ok(default), |v| v.parse())?)
    };
    let cash = number(&mut given, "cash", 25_000.0)?;
    // The option spread assumed, for seeing how much a verdict rests on it.
    let spread = OptionSpread {
        min_half_spread: number(
            &mut given,
            "min_half_spread",
            OptionSpread::MEASURED.min_half_spread,
        )?,
        half_spread_fraction: number(
            &mut given,
            "half_spread_fraction",
            OptionSpread::MEASURED.half_spread_fraction,
        )?,
    };
    let spreads = number(&mut given, "spreads", 1.0)?;
    let defaults: &[(&str, f64)] = match strategy.as_str() {
        ZERO_DTE_PUT_SPREAD => &[
            ("short_delta", 0.10),
            ("width", 2.0),
            ("take_profit", 1.0),
            ("stop_multiple", 2.0),
            ("entry_minutes", 30.0),
        ],
        ZERO_DTE_BREAKOUT => &[
            ("range_bars", 6.0),
            ("delta", 0.5),
            ("target_multiple", 2.0),
            ("stop_fraction", 0.5),
        ],
        _ => &[
            ("dte", 35.0),
            ("short_delta", 0.20),
            ("width", 5.0),
            ("take_profit", 0.5),
            ("exit_dte", 21.0),
        ],
    };
    let mut params: BTreeMap<String, f64> = defaults
        .iter()
        .map(|(k, v)| ((*k).to_owned(), *v))
        .collect();
    params.insert("rate".to_owned(), 0.04);
    params.insert("dividend_yield".to_owned(), 0.013);
    for (key, value) in std::mem::take(&mut given) {
        params.insert(key, value.parse()?);
    }
    params.insert("trade_size".to_owned(), spreads * 100.0);
    let interval = if same_day {
        BarInterval::new(5, arvo_data::IntervalUnit::Minute)
    } else {
        BarInterval::DAILY
    };

    let library = CsvBars::new(root);
    let underlying = "SPY.AIEX";
    let experiment = Experiment {
        id: ExperimentId::from("put-spread"),
        hypothesis: HypothesisId::from("selling SPY put spreads beats holding SPY"),
        instrument: underlying.to_owned(),
        alongside: Vec::new(),
        underlying: None,
        window,
        interval,
        dataset: DatasetRef {
            id: underlying.to_owned(),
            version: library
                .fingerprint(underlying, interval)?
                .unwrap_or_default(),
            adjustment: arvo_data::source::Adjustment::Split,
        },
        strategy: StrategySpec {
            rule: None,
            name: strategy.clone(),
            params: params.clone(),
        },
        costs: CostModel {
            option_spread: Some(spread),
            ..CostModel::proportional(0.0, 0.0)
        },
        risk: RiskModel::default(),
        starting_cash: cash,
        seed: 1,
    };

    let simulation = NautilusSimulation::new(CsvBars::new(root));
    let started = std::time::Instant::now();
    let evidence = arvo_research::evaluate_against_benchmark(
        &simulation,
        &experiment,
        &EvaluationCriteria::default(),
    )?;
    let evaluation = &evidence.evaluation;
    println!(
        "{underlying} {strategy} {}..{}: {:?} in {:.1}s",
        window.from,
        window.to,
        params,
        started.elapsed().as_secs_f64()
    );
    println!("  verdict {:?}", evaluation.verdict);
    for reason in &evaluation.reasons {
        println!("    {reason}");
    }
    let show = |name: &str, m: &arvo_research::Metrics| {
        println!(
            "  {name:<9} return {:+.1}%  max drawdown {:.1}%  sharpe {}  trades {}",
            m.total_return * 100.0,
            m.max_drawdown * 100.0,
            m.sharpe.map_or("n/a".to_owned(), |s| format!("{s:.2}")),
            m.trades
        );
    };
    show("strategy", &evaluation.strategy);
    show("hold SPY", &evaluation.benchmark);
    if let Some(tail) = arvo_research::evaluation::tail(&evaluation.strategy_curve) {
        println!(
            "  tail: worst day {:+.2}%, worst month {}, mean of worst 5% of days {:+.2}%",
            tail.worst_period * 100.0,
            tail.worst_month
                .map_or("n/a".to_owned(), |m| format!("{:+.2}%", m * 100.0)),
            tail.expected_shortfall * 100.0
        );
    }

    if let Some(stress) = &evaluation.stress {
        println!("  crash replay ({} positions unpriced):", stress.unpriced);
        for shock in &stress.shocks {
            let multiple = if shock.credit > 0.0 {
                format!(", {:.1}x its credit", shock.loss / shock.credit)
            } else {
                String::new()
            };
            println!(
                "    {} SPY {:+.1}% VIX {:.0}: worst loss {:.0} ({:.1}% of the account{multiple})  {}",
                shock.day,
                shock.spot_move * 100.0,
                shock.vix,
                shock.loss,
                shock.loss / cash * 100.0,
                shock.what
            );
        }
    }

    // Spreads, as the evaluation counts them: legs taken on and off together.
    let ledger = &evaluation.strategy_ledger;
    let positions = arvo_research::trade::positions(ledger);
    let spreads: Vec<(chrono::NaiveDateTime, f64, &str, &str)> = positions
        .iter()
        .map(|spread| {
            let reason = match spread.exit_reason {
                arvo_research::ExitReason::Expired => "expired",
                arvo_research::ExitReason::StillOpen => "open",
                _ => "closed",
            };
            (
                spread.opened,
                spread.pnl,
                spread.instrument.as_str(),
                reason,
            )
        })
        .collect();
    let wins = spreads.iter().filter(|s| s.1 > 0.0).count();
    let worst = spreads.iter().map(|s| s.1).fold(f64::INFINITY, f64::min);
    println!(
        "  {} positions, {wins} won, worst {worst:+.0}, total {:+.0}",
        spreads.len(),
        spreads.iter().map(|s| s.1).sum::<f64>()
    );
    for (opened, pnl, short, reason) in &spreads {
        println!("    {opened} {short:<52} {reason:<8} {pnl:+8.0}");
    }
    let discrepancies =
        arvo_research::reconcile::reconcile_parts(&experiment, &evaluation.strategy_curve, ledger);
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
