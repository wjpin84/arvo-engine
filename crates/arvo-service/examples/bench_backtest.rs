//! How long one backtest takes, and how long a family sweep takes (#212).
//!
//! The number every compute-plane question is gated on: no claim about a
//! faster sweep, on more cores or on a GPU, can be checked without it. Run
//! it again after a Nautilus bump, and record what it says in
//! `docs/benchmarks.md` beside the commit and the machine.
//!
//! ```text
//! cargo run --release -p arvo-service --example bench_backtest -- <data-dir> [instrument] [--strategy NAME] [--runs N]
//! RAYON_NUM_THREADS=1 cargo run --release ...   # the sweep on one core, for the speed-up
//! ```
//!
//! Release, always: a debug Nautilus is an order of magnitude slower and
//! says nothing about the shipped engine. Reads the library only; records no
//! finding.

use std::time::{Duration, Instant};

use arvo_data::CsvBars;
use arvo_nautilus::NautilusSimulation;
use arvo_research::{run_family, EvaluationCriteria, SimulationProvider};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let root = if args.is_empty() {
        return Err("usage: bench_backtest <data-dir> [instrument] [--strategy NAME] [--runs N]".into());
    } else {
        args.remove(0)
    };
    let strategy = take(&mut args, "--strategy").unwrap_or_else(|| "sma_cross".to_owned());
    let runs: usize = take(&mut args, "--runs").map_or(Ok(5), |n| n.parse())?;
    let instrument = args.first().cloned().unwrap_or_else(|| "DRIFT.SIM".to_owned());

    let bars = CsvBars::new(&root);
    let simulation = NautilusSimulation::new(CsvBars::new(&root));
    let plan = arvo_service::research::StrategyPlan::find(&strategy)
        .ok_or_else(|| format!("no strategy called {strategy:?}"))?;
    let (window, fingerprint) = arvo_service::research::study_data(&bars, &instrument, plan)?;
    let family = arvo_service::research::study_for(&instrument, plan, window, &fingerprint);
    let threads = std::env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get));

    println!("{instrument} {strategy}: {} .. {} ({} days, {})", window.from, window.to, window.days(), plan.interval());
    println!("build {}, {threads} thread(s)", arvo_service::research::code_commit());

    // One backtest: the template with every axis at its first value, which
    // is one of the configurations the sweep below runs.
    let mut one = family.template.clone();
    for (name, values) in plan.axes {
        if let Some(first) = values.first() {
            one.strategy.params.insert((*name).to_owned(), *first);
        }
    }
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let result = simulation.run(&one)?;
        times.push(started.elapsed());
        std::hint::black_box(result);
    }
    times.sort();
    let median = times[times.len() / 2];
    println!(
        "one backtest: median {} over {runs} run(s), fastest {}, slowest {}",
        show(median),
        show(times[0]),
        show(times[times.len() - 1])
    );

    // The sweep: every configuration in the grid, in sample, then the
    // winner out of sample and its benchmark.
    let trials = family.grid.size();
    let criteria = EvaluationCriteria::default();
    let started = Instant::now();
    let found = run_family(&simulation, &family, &criteria)?;
    let sweep = started.elapsed();
    println!(
        "sweep: {trials} trials in {} ({} per trial, {:.1}x one backtest); verdict {:?}",
        show(sweep),
        show(sweep / u32::try_from(trials.max(1))?),
        sweep.as_secs_f64() / median.as_secs_f64().max(f64::EPSILON),
        found.verdict,
    );
    Ok(())
}

fn take(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == flag)?;
    let value = args.get(at + 1).cloned();
    args.drain(at..(at + 2).min(args.len()));
    value
}

fn show(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{} ms", elapsed.as_millis())
    } else {
        format!("{:.2} s", elapsed.as_secs_f64())
    }
}
