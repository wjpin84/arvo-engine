//! Runs a study from the command line, without launching the window.
//!
//!     cargo run -p arvo-runtime --example study -- \
//!         <data-dir> [--strategy NAME] [--walk-forward] [instrument...]
//!
//! Exists because the research path and the GUI fail in completely different
//! ways, and only one of them can be checked in a terminal. This runs exactly
//! what the Research view runs — the same grid, the same split, the same
//! criteria, via [`arvo_service::research::study_for`] — so a verdict here is
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

    // `--since YYYY-MM-DD` trims every instrument to the same starting date.
    //
    // Comparing two sources' copies of one stock is meaningless while one has
    // six more years than the other: the extra history changes the fold count,
    // which changes the selection, which changes everything downstream. Two
    // answers that differ then say nothing about whether the *data* differs.
    let since: Option<chrono::NaiveDate> = match requested.iter().position(|arg| arg == "--since") {
        Some(at) => {
            let value = requested
                .get(at + 1)
                .ok_or("--since wants a date like 2006-09-13")?
                .clone();
            requested.drain(at..=at + 1);
            Some(value.parse()?)
        }
        None => None,
    };
    // `--cash N` runs the study on an account of that size. Not a rescaling of
    // the percentages: sizing is in units of the instrument, so a smaller
    // account buys less of it and the gate caps what it cannot afford — which is
    // the whole question when someone asks what a rule would have made on $100.
    let cash: Option<f64> = match requested.iter().position(|arg| arg == "--cash") {
        Some(at) => {
            let value = requested.get(at + 1).ok_or("--cash wants an amount")?.clone();
            requested.drain(at..=at + 1);
            Some(value.parse()?)
        }
        None => None,
    };
    // `--record` writes the finding to the evidence store, through
    // `research::run_study` — the same function the engine's RunStudy handler
    // calls, not a second writer of the same format. A session starts from a
    // stored finding, so this is how one gets there without the window.
    //
    // Off by default: a terminal run is usually a question, and a store that
    // fills up with answers nobody asked to keep is one nobody reads.
    let record = requested.iter().any(|arg| arg == "--record");
    requested.retain(|arg| arg != "--record");
    // `--position-fraction N` caps an entry at that share of the account. The
    // shipped model allows all of it, which at intraday resolutions is how a
    // rule ends up asking to be all-in on every signal and being refused by the
    // venue (arvo-desktop #251). A flag rather than an edit to the project's
    // risk file, so the question can be answered without changing what every
    // other study in the project is measured against.
    let fraction: Option<f64> = match requested.iter().position(|arg| arg == "--position-fraction") {
        Some(at) => {
            let value = requested.get(at + 1).ok_or("--position-fraction wants a fraction")?.clone();
            requested.drain(at..=at + 1);
            Some(value.parse()?)
        }
        None => None,
    };
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
    // The project's own rules and rulesets, as the view loads them: without
    // this, `--strategy` could only name a rule compiled into the binary, and a
    // rule the project wrote as data (#225) would report as unknown. The data
    // directory is the project directory for this example's purposes, which is
    // where `rules/` and `rulesets/` sit beside the library.
    let project = std::path::Path::new(&root).parent().unwrap_or_else(|| std::path::Path::new(&root));
    arvo_service::rules::refresh_at(project);
    arvo_service::rulesets::refresh_at(project);
    let plan = arvo_service::research::StrategyPlan::find(strategy)
        .ok_or_else(|| format!("no strategy called {strategy:?}"))?;
    // The strategy's resolution, as the view uses. This read daily bars for
    // every rule, so an intraday rule was handed a daily file — or reported an
    // instrument that only has intraday bars as unknown.
    let interval = plan.interval();

    // The panel is the daily control's cross-section and does not take a
    // strategy, so it says nothing about an intraday rule.
    if interval.is_intraday() {
        println!("(no panel: {strategy} is defined at {interval}, and the panel is daily)");
    } else {
        run_panel_over(&bars, &simulation, &instruments, &criteria)?;
    }

    // A ranking rule's whole content is the comparison between instruments,
    // so running it once per instrument would report N findings about a rule
    // that was never asked its question. The runtime refuses that outright;
    // here it takes the other path instead.
    if plan_ranks(strategy) {
        return run_ranked(&bars, &simulation, &instruments, strategy, &criteria);
    }

    for instrument in instruments {
        println!("\n=== {instrument} ===");
        // The view's own window and dataset identity — for an option rule,
        // where the chain exists as well.
        let (window, fingerprint) =
            match arvo_service::research::study_data(&bars, &instrument, plan) {
                Ok(found) => found,
                Err(reason) => {
                    println!("  {reason}");
                    continue;
                }
            };
        // Later of the two, so `--since` narrows and never invents coverage an
        // instrument does not have.
        let (from, to) = (since.map_or(window.from, |floor| window.from.max(floor)), window.to);
        if from >= to {
            println!("  no bars after {from}");
            continue;
        }
        let window = DateRange::new(from, to)?;
        println!("  {} .. {} ({} days)", from, to, window.days());
        println!("  {interval} dataset {}", &fingerprint[..16.min(fingerprint.len())]);

        if rolling {
            let procedure = arvo_service::research::walk_forward_for(
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

        if record {
            if cash.is_some() || fraction.is_some() {
                println!(
                    "  --cash and --position-fraction are ignored with --record: a stored finding                      is the study the engine ran, and one recorded against an account or a cap                      nobody configured could not be reproduced from the project's own settings"
                );
            }
            let service = arvo_service::research::ResearchService::new(
                std::path::PathBuf::from(&root),
                project.join(arvo_service::research::EVIDENCE_SUBDIR),
            );
            match arvo_service::research::study::run_study(&service, &instrument, Some(strategy)) {
                Err(err) => println!("  FAILED: {err}"),
                Ok(view) => println!("  verdict: {} — recorded as {}", view.verdict, view.id),
            }
            continue;
        }

        let mut family =
            arvo_service::research::study_for(&instrument, plan, window, &fingerprint);
        if let Some(cash) = cash {
            family.template.starting_cash = cash;
        }
        if let Some(fraction) = fraction {
            family.template.risk.max_position_fraction = Some(fraction);
        }
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

    // The count is the readable summary; the margins are what the verdict
    // actually turns on. A reader who only sees "5 of 7" cannot tell a
    // procedure that cleared its bars comfortably from one that scraped over
    // five and fell well short of two, and those are opposite findings.
    let margins: Vec<f64> = found
        .folds
        .iter()
        .filter_map(|fold| {
            let bar = fold.selection.expected_best_under_null?;
            Some(fold.selection.best_sharpe - bar)
        })
        .collect();
    if !margins.is_empty() {
        let mean = margins.iter().sum::<f64>() / margins.len() as f64;
        let shown: Vec<String> = margins.iter().map(|m| format!("{m:+.3}")).collect();
        println!("  margins over the no-skill bar: {}", shown.join(" "));
        println!("  mean margin {mean:+.4}");
    }
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

    let study = arvo_service::research::panel_for(usable, window, &dataset);
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
            // How much of the panel's apparent breadth is real. Three
            // instruments that moved together are one observation wearing a
            // three, and the pooled numbers above cannot tell you which.
            if let Some(breadth) = &found.breadth {
                match (breadth.effective, breadth.overstatement()) {
                    (Some(effective), Some(overstatement)) => println!(
                        "  breadth: {} instruments behave like {effective:.2} independent ones (mean correlation {:.2}); pooled average {overstatement:.2}x less certain than the count suggests",
                        breadth.instruments.len(),
                        breadth.mean_correlation.unwrap_or_default(),
                    ),
                    _ => println!("  breadth: too little overlap to say"),
                }
            }
            for reason in &found.reasons {
                println!("  - {reason}");
            }
        }
    }
    Ok(())
}

/// Whether this strategy ranks instruments against each other.
fn plan_ranks(strategy: &str) -> bool {
    arvo_service::research::StrategyPlan::find(strategy)
        .is_some_and(arvo_service::research::StrategyPlan::ranks_a_set)
}

/// Runs a ranking rule over the whole set as one book.
///
/// The window is the intersection of every member's coverage and the dataset
/// hash covers all of them, for the reasons `run_book` gives: a rule cannot
/// rank an instrument over a period it has no prices for, and a hash naming
/// one member would call the run stale when that member changed and fresh
/// when any other did.
fn run_ranked(
    bars: &CsvBars,
    simulation: &NautilusSimulation<CsvBars>,
    instruments: &[String],
    strategy: &str,
    criteria: &EvaluationCriteria,
) -> Result<(), Box<dyn std::error::Error>> {
    let plan =
        arvo_service::research::StrategyPlan::find(strategy).ok_or("no such strategy")?;
    let (mut from, mut to) = (chrono::NaiveDate::MIN, chrono::NaiveDate::MAX);
    let mut hasher = blake3::Hasher::new();
    for instrument in instruments {
        let Some((first, last)) = bars.coverage(instrument, arvo_data::BarInterval::DAILY)?
        else {
            continue;
        };
        if let Some(print) = bars.fingerprint(instrument, arvo_data::BarInterval::DAILY)? {
            hasher.update(instrument.as_bytes());
            hasher.update(print.as_bytes());
        }
        from = from.max(first);
        to = to.min(last);
    }

    let window = DateRange::new(from, to)?;
    println!("\n=== RANKED: {} instruments ===", instruments.len());
    println!("  {from} .. {to}");

    let mut family = arvo_service::research::study_for(
        &instruments[0],
        plan,
        window,
        hasher.finalize().to_hex().as_str(),
    );
    family.template.alongside = instruments[1..].to_vec();

    let found = arvo_research::run_family(simulation, &family, criteria)?;
    println!("  verdict: {:?}", found.verdict);
    println!(
        "  chose {:?}",
        found
            .selected
            .strategy
            .params
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
    );
    for reason in &found.reasons {
        println!("  - {reason}");
    }

    let evaluation = &found.out_of_sample_evidence.evaluation;
    println!(
        "  out-of-sample: strategy {:+.2}% ({} trades), buy-and-hold {:+.2}%",
        evaluation.strategy.total_return * 100.0,
        evaluation.strategy.trades,
        evaluation.benchmark.total_return * 100.0,
    );
    let mut held: Vec<&str> = evaluation
        .strategy_ledger
        .iter()
        .map(|trade| trade.instrument.as_str())
        .collect();
    held.sort_unstable();
    held.dedup();
    println!("  held at some point: {}", held.join(" "));
    Ok(())
}
