//! Asks two vendors the same question and reports how far apart they are.
//!
//!     cargo run -p arvo-runtime --example second_source -- \
//!         [--first robinhood] [--second yahoo] [--years N] SYMBOL...
//!
//! The first thing in this codebase that checks a price against something other
//! than itself. Every quality check inspects a series against its own shape — a
//! low above a high, a gap where a session should be — and those catch what is
//! impossible, never what is merely wrong. A close that is off by forty cents is
//! a perfectly well-formed bar.
//!
//! # Writes nothing
//!
//! It used to fetch the second vendor's copy into the library and then compare
//! against what was on disk. It no longer does either: `source::compare` holds
//! both series in memory, so asking the question does not change what the
//! library holds — and a disagreement is something to look at rather than
//! something to resolve automatically by preferring whichever source answered
//! second. Use the `fetch` example with `--source` to actually pull one in.
//!
//! Same code path as the window's Cross-check button, which is the point.

use arvo_data::{agreement::Agreement, BarInterval};
use arvo_runtime_lib::source;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let first = source::by_id(
        &take_value(&mut args, "--first").unwrap_or_else(|| "robinhood".to_owned()),
    )?;
    let second =
        source::by_id(&take_value(&mut args, "--second").unwrap_or_else(|| "yahoo".to_owned()))?;
    let years: i64 = take_value(&mut args, "--years")
        .unwrap_or_else(|| "25".to_owned())
        .parse()
        .map_err(|_| "--years wants a number")?;

    if args.is_empty() {
        return Err("name at least one symbol".into());
    }
    if first.id() == second.id() {
        return Err("a series compared against itself agrees by construction".into());
    }

    let interval = BarInterval::DAILY;
    let to = chrono::Utc::now().date_naive();
    let from = to - chrono::Duration::days(365 * years);
    println!(
        "{interval} bars, {from} to {to}: {} against {}\n",
        first.label(),
        second.label()
    );

    let mut diverged = 0_usize;
    for symbol in &args {
        match source::compare(first.as_ref(), second.as_ref(), symbol, interval, from, to).await {
            // Named and carried on. One unknown symbol is not a reason to
            // abandon the four after it.
            Err(err) => println!("{symbol}: {err}\n"),
            Ok(outcome) => {
                println!(
                    "{symbol}: {} bars from {}, {} from {}",
                    outcome.first_bars, outcome.first, outcome.second_bars, outcome.second
                );
                println!(
                    "  coverage: {} shared, {} only in the first, {} only in the second",
                    outcome.coverage.shared,
                    outcome.coverage.only_first,
                    outcome.coverage.only_second
                );
                // Classified before it is counted. Two vendors disagreeing is
                // the normal case and most of the ways they disagree are not
                // faults — a different adjustment basis is the commonest, and
                // reporting it as thousands of bad bars is how a check gets
                // switched off.
                match &outcome.agreement {
                    Agreement::NoOverlap => println!("  nothing to compare"),
                    Agreement::Aligned { compared } => {
                        println!("  agree on all {compared} shared bars");
                    }
                    Agreement::Rescaled { factor, compared } => println!(
                        "  a near-constant factor of {factor:.4} across {compared} bars — an \
                         adjustment difference, not bad data on either side"
                    ),
                    Agreement::Diverged {
                        disagreeing,
                        compared,
                        worst,
                        at,
                    } => {
                        diverged += 1;
                        println!(
                            "  {disagreeing} of {compared} genuinely disagree, worst {:.2}% on \
                             {} — at least one of these has prices nobody traded at",
                            worst * 100.0,
                            at.format("%Y-%m-%d")
                        );
                    }
                }
                println!();
            }
        }
    }

    println!("{diverged} of {} instruments diverged", args.len());
    Ok(())
}

/// Takes `--flag value` out of the arguments, if it is there.
fn take_value(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == flag)?;
    let value = args.get(at + 1).cloned();
    args.drain(at..=(at + 1).min(args.len() - 1));
    value
}
