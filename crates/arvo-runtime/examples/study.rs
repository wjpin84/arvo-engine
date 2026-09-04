//! Runs a study from the command line, without launching the window.
//!
//!     cargo run -p arvo-runtime --example study -- <data-dir> [instrument...]
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
        .ok_or("usage: study <data-dir> [instrument...]")?;

    let bars = CsvBars::new(&root);
    let requested: Vec<String> = args.collect();
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

    for instrument in instruments {
        println!("\n=== {instrument} ===");
        let Some((from, to)) = bars.coverage(&instrument)? else {
            println!("  no bars");
            continue;
        };
        let window = DateRange::new(from, to)?;
        println!("  {} .. {} ({} days)", from, to, window.days());

        let family = arvo_runtime_lib::research::study_for(&instrument, window);
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
