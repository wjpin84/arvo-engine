//! Runs a study from the command line, without launching the window.
//!
//!     cargo run -p arvo-runtime --example study -- \
//!         <data-dir> [--strategy NAME] [--walk-forward] [instrument...]
//!
//! Exists because the research path and the GUI fail in completely different
//! ways, and only one of them can be checked in a terminal. This runs exactly
//! what the Research view runs — the same grid, the same split, the same
//! criteria, via [`arvo_runtime_lib::research::study_for`] — so a verdict here is
//! the verdict the view will show.

use arvo_data::{BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{DateRange, EvaluationCriteria};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .ok_or("usage: study <data-dir> [--strategy NAME] [instrument...]")?;

    let bars = CsvBars::new(&root);
    // `--strategy NAME` anywhere in the tail; everything else is an
    // instrument. Enough argument parsing for an example, and no more.
    let mut requested: Vec<String> = args.collect();
    let mut strategy = "sma_cross".to_owned();
    if let Some(flag) = requested.iter().position(|arg| arg == "--strategy") {
        strategy = requested
            .get(flag + 1)
            .ok_or("--strategy needs a name")?
            .clone();
        requested.drain(flag..=flag + 1);
    }
    let strategy = strategy.as_str();
    // `--walk-forward` re-selects on a rolling schedule instead of splitting
    // the window once. The two answer different questions, so this is a mode
    // rather than a flag on the same output.
    let rolling = requested.iter().any(|arg| arg == "--walk-forward");
    requested.retain(|arg| arg != "--walk-forward");
    let instruments = if requested.is_empty() {
        bars.instruments()?
    } else {
        requested
    };

    if instruments.is_empty() {
        println!("no instruments in {root}");
        return Ok(());
    }

    let simulation = NautilusSimulation::new(CsvBars::new(&root));
    let criteria = EvaluationCriteria::default();

    run_panel_over(&bars, &simulation, &instruments, &criteria)?;

    for instrument in instruments {
        println!("\n=== {instrument} ===");
        let Some((from, to)) = bars.coverage(&instrument, arvo_data::BarInterval::DAILY)? else {
            println!("  no bars");
            continue;
        };
        let window = DateRange::new(from, to)?;
        println!("  {} .. {} ({} days)", from, to, window.days());

        let fingerprint = bars
            .fingerprint(&instrument, arvo_data::BarInterval::DAILY)?
            .ok_or("instrument holds no bars")?;
        println!("  dataset {}", &fingerprint[..16]);
        let plan = arvo_runtime_lib::research::StrategyPlan::find(strategy)
            .ok_or_else(|| format!("no strategy called {strategy:?}"))?;

        if rolling {
            let procedure = arvo_runtime_lib::research::walk_forward_for(
                &instrument,
                plan,
                window,
                &fingerprint,
            );
            match arvo_research::run_walk_forward(&simulation, &procedure, &criteria) {
                Err(reason) => println!("  could not run: {reason}"),
                Ok(found) => report_walk_forward(&found),
            }
            continue;
        }

        let family =
            arvo_runtime_lib::research::study_for(&instrument, plan, window, &fingerprint);
        match arvo_research::run_family(&simulation, &family, &criteria) {
            Err(err) => println!("  FAILED: {err}"),
            Ok(found) => {
                let evaluation = &found.out_of_sample_evidence.evaluation;
                println!("  verdict: {:?}", found.verdict);
                println!(
                    "  chose on {}..{}, judged on {}..{}",
                    found.in_sample.from,
                    found.in_sample.to,
                    found.out_of_sample.from,
                    found.out_of_sample.to
                );
                println!(
                    "  {} trials, best in-sample Sharpe {:.2} vs {:?} expected under null ({})",
                    found.selection.trials,
                    found.selection.best_sharpe,
                    found
                        .selection
                        .expected_best_under_null
                        .map(|v| format!("{v:.2}")),
                    if found.selection.survived_deflation {
                        "survived"
                    } else {
                        "FAILED deflation"
                    }
                );
                println!("  winner: {:?}", found.selected.strategy.params);
                println!(
                    "  out-of-sample: strategy {:+.2}% ({} trades), buy-and-hold {:+.2}%, excess {:+.2}%",
                    evaluation.strategy.total_return * 100.0,
                    evaluation.strategy.trades,
                    evaluation.benchmark.total_return * 100.0,
                    evaluation.excess_return * 100.0
                );
                println!(
                    "  drawdown: strategy {:.2}%, buy-and-hold {:.2}%",
                    evaluation.strategy.max_drawdown * 100.0,
                    evaluation.benchmark.max_drawdown * 100.0
                );
                // The ledger, not the curve. A return says a rule made money;
                // these say whether it did so in a way that could repeat.
                let trades = &evaluation.strategy_trades;
                let show = |value: Option<f64>| {
                    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.2}"))
                };
                println!(
                    "  ledger: {} closed ({} still open), {} won, {} lost on stops",
                    trades.closed, trades.still_open, trades.wins, trades.stop_exits
                );
                println!(
                    "  win rate {}, profit factor {}, expectancy {} per trade",
                    trades
                        .win_rate
                        .map_or_else(|| "n/a".to_owned(), |v| format!("{:.0}%", v * 100.0)),
                    show(trades.profit_factor),
                    show(trades.expectancy())
                );
                println!(
                    "  held {} on average; fees {:.2} ({:.2}% of capital, slippage not incl.)",
                    trades.average_holding_secs.map_or_else(
                        || "n/a".to_owned(),
                        |secs| format!("{:.1} days", secs / 86_400.0)
                    ),
                    trades.total_commission,
                    trades.total_commission / found.selected.starting_cash * 100.0
                );
                for item in arvo_research::recommend(&found) {
                    println!("  [{}] {}", item.severity.label(), item.finding);
                    println!("      {}", item.action);
                    println!("      ({})", item.evidence);
                }
                for reason in &found.reasons {
                    println!("  - {reason}");
                }
                if !found.failures.is_empty() {
                    println!("  {} trials did not run:", found.failures.len());
                    for failure in &found.failures {
                        println!("    {failure}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// The panel: one configuration chosen across every instrument, judged on all
/// of them. This is the run that can actually reach a verdict, so it goes
/// first.
fn report_walk_forward(found: &arvo_research::WalkForwardEvidence) {
    println!("  verdict: {:?}", found.verdict);
    println!(
        "  {} folds, {} of which selected better than chance",
        found.folds.len(),
        found.folds_surviving_deflation
    );
    if found.folds_without_trades > 0 {
        println!(
            "  {} folds never opened a position at all",
            found.folds_without_trades
        );
    }
    for fold in &found.folds {
        println!(
            "    chose on {}..{} → judged {}..{}: {:?} {:+.2}%",
            fold.in_sample.from,
            fold.in_sample.to,
            fold.out_of_sample.from,
            fold.out_of_sample.to,
            fold.selected.strategy.params,
            fold.out_of_sample_evidence.evaluation.strategy.total_return * 100.0,
        );
    }
    // The stitched record is the point: one continuous out-of-sample track
    // rather than a slice, which is what makes the trade minimum reachable.
    println!(
        "  stitched: {:+.2}% vs buy-and-hold {:+.2}% (excess {:+.2}%), {} round trips",
        found.combined.total_return * 100.0,
        found.benchmark.total_return * 100.0,
        found.excess_return * 100.0,
        found.combined_trades.closed,
    );
    println!(
        "  drawdown {:.2}%, sharpe {:?}",
        found.combined.max_drawdown * 100.0,
        found.combined.sharpe.map(|value| format!("{value:.2}"))
    );
    for axis in &found.stability {
        println!(
            "  {} settled on {} in {:.0}% of folds ({} distinct values tried)",
            axis.axis,
            axis.modal,
            axis.modal_share * 100.0,
            axis.distinct
        );
    }
    for reason in &found.reasons {
        println!("  - {reason}");
    }
}

fn run_panel_over(
    bars: &CsvBars,
    simulation: &NautilusSimulation<CsvBars>,
    instruments: &[String],
    criteria: &EvaluationCriteria,
) -> Result<(), Box<dyn std::error::Error>> {
    // The overlap, not the union: instruments judged on different periods are
    // not a cross-section.
    let mut from = chrono::NaiveDate::MIN;
    let mut to = chrono::NaiveDate::MAX;
    let mut hasher = blake3::Hasher::new();
    let mut usable = Vec::new();

    for instrument in instruments {
        let Some((first, last)) = bars.coverage(instrument, arvo_data::BarInterval::DAILY)? else {
            continue;
        };
        from = from.max(first);
        to = to.min(last);
        if let Some(fingerprint) = bars.fingerprint(instrument, arvo_data::BarInterval::DAILY)? {
            hasher.update(fingerprint.as_bytes());
        }
        usable.push(instrument.clone());
    }
    if usable.is_empty() {
        return Ok(());
    }

    let window = DateRange::new(from, to)?;
    let dataset = hasher.finalize().to_hex().to_string();
    println!("\n=== PANEL: {} instruments ===", usable.len());
    println!("  {} .. {}  dataset {}", from, to, &dataset[..16]);

    let study = arvo_runtime_lib::research::panel_for(usable, window, &dataset);
    match arvo_research::run_panel(simulation, &study, criteria) {
        Err(err) => println!("  FAILED: {err}"),
        Ok(found) => {
            println!("  verdict: {:?}", found.verdict);
            println!(
                "  chose on {}..{}, judged on {}..{}",
                found.in_sample.from,
                found.in_sample.to,
                found.out_of_sample.from,
                found.out_of_sample.to
            );
            println!(
                "  one configuration for the panel: {:?} (pooled in-sample Sharpe {:.2} vs {:?})",
                found.selected_params,
                found.selection.best_sharpe,
                found
                    .selection
                    .expected_best_under_null
                    .map(|v| format!("{v:.2}"))
            );
            println!(
                "  pooled: {} trades, mean excess {:+.2}%, beat benchmark on {}/{}",
                found.pooled.total_trades,
                found.pooled.mean_excess_return * 100.0,
                found.pooled.beat_benchmark,
                found.pooled.instruments
            );
            for outcome in &found.per_instrument {
                println!(
                    "    {:<12} strategy {:+7.2}%  benchmark {:+7.2}%  excess {:+7.2}%  {} trades",
                    outcome.instrument,
                    outcome.strategy.total_return * 100.0,
                    outcome.benchmark.total_return * 100.0,
                    outcome.excess_return * 100.0,
                    outcome.strategy.trades
                );
            }
            for reason in &found.reasons {
                println!("  - {reason}");
            }
        }
    }
    Ok(())
}
