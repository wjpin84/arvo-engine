//! Re-runs every stored finding's out-of-sample experiment with this build,
//! and says what changed.
//!
//!     cargo run -p arvo-runtime --example recheck -- <data-dir> <evidence-dir>
//!
//! A finding records the engine it ran on, not the code around the engine.
//! When the platform fixes something that changed results — sizing, a refused
//! order, a session boundary — every stored verdict produced before the fix
//! still reads as settled. `replay_record` answers "does it still come out the
//! same" one finding at a time, and stops at the first difference; this asks
//! the whole store and prints the numbers a reader decides on: trades,
//! refusals, return, verdict.
//!
//! Only the out-of-sample run is repeated — the configuration the finding
//! chose, on the window it was judged on. Deflation is a property of the
//! search and does not move, so a finding that failed it still fails it; what
//! can change is whether the chosen configuration's own result holds.
//!
//! Writes nothing. A different answer here is something to look at and re-run
//! from the window, not something to overwrite the record with.

use arvo_data::{BarProvider, CsvBars};
use arvo_nautilus::NautilusSimulation;
use arvo_research::{EvidenceStore, Record};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(data), Some(evidence)) = (args.next(), args.next()) else {
        return Err("usage: recheck <data-dir> <evidence-dir>".into());
    };
    let bars = CsvBars::new(&data);
    let simulation = NautilusSimulation::new(CsvBars::new(&data));
    let loaded = EvidenceStore::new(&evidence).load()?;
    for problem in &loaded.problems {
        println!("unreadable: {problem}");
    }

    for stored in loaded.records.iter().rev() {
        let label = format!("{} {} {}", &stored.id[..8], stored.record.kind(), stored.record.subject());
        let Record::Study(found) = &stored.record else {
            println!("{label}: skipped — only a study's single out-of-sample run is repeated here");
            continue;
        };
        let experiment = &found.selected;
        let before = &found.out_of_sample_evidence.evaluation;

        // Whether the data is still the data. A different answer on changed
        // data says nothing about the code.
        let same_data = if experiment.alongside.is_empty() {
            match bars.fingerprint(&experiment.instrument, experiment.interval)? {
                Some(now) if now == experiment.dataset.version => "same data",
                Some(_) => "DATA CHANGED",
                None => "DATA GONE",
            }
        } else {
            "book: data not checked"
        };

        match arvo_research::evaluate_against_benchmark(
            &simulation,
            experiment,
            &found.out_of_sample_evidence.criteria,
        ) {
            Err(err) => println!("{label}: could not re-run — {err}"),
            Ok(evidence) => {
                let after = &evidence.evaluation;
                let changed = before.strategy.trades != after.strategy.trades
                    || before.verdict != after.verdict;
                println!(
                    "{label} [{}] {} ({same_data})",
                    experiment.strategy.name,
                    if changed { "CHANGED" } else { "unchanged" },
                );
                println!(
                    "    trades {:>4} -> {:<4}  refused {} entries / {} exits  return {:+.2}% -> {:+.2}%  excess {:+.2}% -> {:+.2}%",
                    before.strategy.trades,
                    after.strategy.trades,
                    after.refused_orders.entries,
                    after.refused_orders.exits,
                    before.strategy.total_return * 100.0,
                    after.strategy.total_return * 100.0,
                    before.excess_return * 100.0,
                    after.excess_return * 100.0,
                );
                println!(
                    "    out-of-sample verdict {:?} -> {:?}; the finding's verdict {:?}{}",
                    before.verdict,
                    after.verdict,
                    found.verdict,
                    if found.selection.survived_deflation {
                        ""
                    } else {
                        " (it failed deflation, which a re-run cannot change)"
                    },
                );
            }
        }
    }
    Ok(())
}
