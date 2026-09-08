//! Fetches from Yahoo and compares it against the broker's copy.
//!
//!     cargo run -p arvo-runtime --example second_source -- \
//!         <data-dir> [--years N] SYMBOL...
//!
//! The first thing in this codebase that checks a price against something
//! other than itself. Every quality check inspects a series against its own
//! shape — a low above a high, a gap where a session should be — and those
//! catch what is impossible, never what is merely wrong. A close that is off
//! by forty cents is a perfectly well-formed bar.
//!
//! Writes the Yahoo copy under its own venue, so the two are two datasets with
//! two content hashes and a study can never silently run on whichever was
//! fetched last. Then compares them and says which of the four things
//! happened: they agree, one is a rescaling of the other, they genuinely
//! disagree, or they cover different periods.

use arvo_data::{agreement, BarInterval, BarProvider, CsvBars};
use arvo_runtime_lib::{feed, yahoo};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return Err("usage: second_source <data-dir> [--years N] SYMBOL...".into());
    }
    let root = args.remove(0);
    let years: i64 = match args.iter().position(|a| a == "--years") {
        Some(at) => {
            let value = args.get(at + 1).ok_or("--years wants a number")?.clone();
            args.drain(at..=at + 1);
            value.parse()?
        }
        None => 25,
    };
    if args.is_empty() {
        return Err("name at least one symbol".into());
    }

    let interval = BarInterval::DAILY;
    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(365 * years);
    let library = CsvBars::new(&root);
    println!("{interval} bars, {from} to {to}\n");

    for symbol in &args {
        match yahoo::fetch(root.as_ref(), symbol, interval, from, to).await {
            Err(err) => println!("{symbol}: {err}\n"),
            Ok(report) => {
                println!("{}: {} bars", report.instrument, report.bars);
                if let (Some(first), Some(last)) = (report.from, report.to) {
                    println!("  {} .. {}", first.date(), last.date());
                }
                for finding in &report.quality.findings {
                    println!("  [{:?}] {}", finding.severity, finding.detail);
                }

                // The point of the exercise: the same instrument, from a
                // source that has never seen the other one's answer.
                let broker = format!("{symbol}.{}", feed::FETCHED_VENUE);
                match library.bars(&broker, interval, from, to) {
                    Ok(held) if !held.is_empty() => {
                        let mine = library
                            .bars(&report.instrument, interval, from, to)
                            .unwrap_or_default();
                        let (verdict, coverage) = agreement::compare(&held, &mine);
                        println!("  against {broker}: {verdict:?}");
                        println!(
                            "  coverage: {} shared, {} only in the broker's, {} only in Yahoo's",
                            coverage.shared, coverage.only_first, coverage.only_second
                        );
                    }
                    // No broker copy is not a failure. It is the ordinary case
                    // for a symbol Yahoo has and the broker was never asked
                    // for, and the fetch above still stands on its own.
                    _ => println!("  no broker copy to compare against"),
                }
                println!();
            }
        }
    }
    Ok(())
}
